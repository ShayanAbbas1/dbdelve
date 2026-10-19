//! Which file column feeds which table column, and the check of every row
//! against that choice. Pure state: nothing here touches a database.

use std::sync::Arc;

use super::{Cell, Format, Json, Options, Problem, Rule, Table, Value};
use super::{DateFormat, Decimal};
use crate::db::{Engine, Structure};
use crate::mql;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TargetColumn {
    pub(crate) name: String,
    pub(crate) data_type: String,
    pub(crate) nullable: bool,
    pub(crate) has_default: bool,
    pub(crate) generated: bool,
}

impl TargetColumn {
    pub(crate) fn from_structure(structure: &Structure, engine: Engine) -> Vec<Self> {
        structure
            .columns
            .iter()
            .map(|column| Self {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
                // A sampled field that was always present is not a NOT NULL constraint.
                nullable: column.nullable || engine.takes_new_fields(),
                has_default: column.default.is_some(),
                generated: engine.generates(column),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Target {
    Skip,
    Column(usize),
    NewField,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Mapping {
    pub(crate) target: Target,
    pub(crate) rule: Rule,
}

fn normalized(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) fn auto_map(
    columns: &[String],
    targets: &[TargetColumn],
    header: bool,
    engine: Engine,
) -> Vec<Mapping> {
    let mut claimed = vec![false; targets.len()];
    let mut new_fields: Vec<&str> = Vec::new();
    let mut by_position = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| !target.generated)
        .map(|(i, _)| i);
    columns
        .iter()
        .map(|name| {
            let target = if header {
                let wanted = normalized(name);
                let found = targets
                    .iter()
                    .position(|t| !t.generated && normalized(&t.name) == wanted);
                match found {
                    Some(i) if !claimed[i] => {
                        claimed[i] = true;
                        Target::Column(i)
                    }
                    // A repeat of a name already mapped must not become a second field of that name.
                    Some(_) => Target::Skip,
                    None if engine.takes_new_fields() && !new_fields.contains(&name.as_str()) => {
                        new_fields.push(name);
                        Target::NewField
                    }
                    None => Target::Skip,
                }
            } else {
                by_position.next().map_or(Target::Skip, Target::Column)
            };
            Mapping {
                target,
                rule: Rule::AsIs,
            }
        })
        .collect()
}

/// A target can be chosen once only: choosing a taken one moves it.
pub(crate) fn reassign(mappings: &mut [Mapping], file_column: usize, target: Target) {
    if let Target::Column(_) = target {
        for mapping in mappings.iter_mut().filter(|m| m.target == target) {
            mapping.target = Target::Skip;
        }
    }
    mappings[file_column].target = target;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Unmapped {
    UsesDefault,
    Null,
    Generated,
    LeftOut,
    Required,
}

pub(crate) fn unmapped(
    targets: &[TargetColumn],
    mappings: &[Mapping],
    engine: Engine,
) -> Vec<(usize, Unmapped)> {
    targets
        .iter()
        .enumerate()
        .filter(|(i, _)| !mappings.iter().any(|m| m.target == Target::Column(*i)))
        .map(|(i, target)| {
            let what = if engine.takes_new_fields() {
                Unmapped::LeftOut
            } else if target.generated {
                Unmapped::Generated
            } else if target.has_default {
                Unmapped::UsesDefault
            } else if target.nullable {
                Unmapped::Null
            } else {
                Unmapped::Required
            };
            (i, what)
        })
        .collect()
}

/// One rule per kind; the date format and the decimal separator are chosen
/// separately. `None` is a new field, which could hold anything.
pub(crate) fn offered_rules(target: Option<&TargetColumn>) -> Vec<Rule> {
    let kind = target.map(|t| t.data_type.to_lowercase());
    let has = |words: &[&str]| {
        kind.as_ref()
            .is_none_or(|k| words.iter().any(|w| k.contains(w)))
    };
    let mut rules = vec![Rule::AsIs];
    if has(&["date", "time"]) {
        rules.push(Rule::Date(DateFormat::Iso));
    }
    if has(&[
        "int", "long", "numeric", "decimal", "float", "double", "real", "number", "money", "serial",
    ]) && !kind
        .as_ref()
        .is_some_and(|k| k.contains("interval") || k.contains("point"))
    {
        rules.push(Rule::Number(Decimal::Dot));
    }
    if has(&["bool", "bit", "tinyint(1)"]) {
        rules.push(Rule::Boolean);
    }
    rules.push(Rule::Exact);
    rules
}

pub(crate) struct CheckedRow {
    pub(crate) line: usize,
    pub(crate) values: Vec<Option<Value>>,
}

/// `rows` is index-aligned with `Table::rows` (a row with a problem is kept,
/// its bad values `None`), which `Problem::row` and the preview rely on.
/// `columns` is `(name, data_type)` in insert order and `sources` the file
/// column behind each.
pub(crate) struct Checked {
    pub(crate) columns: Vec<(String, String)>,
    pub(crate) sources: Vec<usize>,
    pub(crate) rows: Vec<CheckedRow>,
    pub(crate) problems: Vec<Problem>,
}

fn has_operator(json: &Json) -> bool {
    match json {
        Json::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.starts_with('$') || has_operator(value)),
        Json::Array(items) => items.iter().any(has_operator),
        _ => false,
    }
}

pub(crate) fn check(
    table: &Table,
    targets: &[TargetColumn],
    mappings: &[Mapping],
    options: &Options,
    engine: Engine,
) -> Checked {
    let mut problems = table.problems.clone();
    let file_wide = |column: Option<usize>, message: String| Problem {
        row: None,
        line: None,
        column,
        message,
    };

    let mut columns = Vec::new();
    let mut sources = Vec::new();
    let mut rules = Vec::new();
    let mut nullable = Vec::new();
    for (source, mapping) in mappings.iter().enumerate() {
        let (name, data_type, null) = match mapping.target {
            Target::Skip => continue,
            Target::Column(i) => (&targets[i].name, &targets[i].data_type, targets[i].nullable),
            Target::NewField => (&table.columns[source], &String::new(), true),
        };
        columns.push((name.clone(), data_type.clone()));
        sources.push(source);
        rules.push(&mapping.rule);
        nullable.push(null);
    }

    if table.rows.is_empty() {
        problems.push(file_wide(None, "The file has no rows to import.".into()));
    }
    if columns.is_empty() {
        problems.push(file_wide(
            None,
            "No file column is mapped to a column.".into(),
        ));
    }
    for (i, what) in unmapped(targets, mappings, engine) {
        if what == Unmapped::Required {
            problems.push(file_wide(
                None,
                format!(
                    "`{}` is NOT NULL, has no default, and nothing maps to it",
                    targets[i].name
                ),
            ));
        }
    }
    for (k, (name, _)) in columns.iter().enumerate() {
        if engine.takes_new_fields() && !mql::writable_field(name) {
            problems.push(file_wide(Some(sources[k]), mql::unwritable_field(name)));
        }
        if columns[..k].iter().any(|(earlier, _)| earlier == name) {
            problems.push(file_wide(
                Some(sources[k]),
                format!("`{name}` is mapped from more than one file column"),
            ));
        }
    }

    let mut rows = Vec::with_capacity(table.rows.len());
    for (r, file_row) in table.rows.iter().enumerate() {
        let problem = |column: usize, message: String| Problem {
            row: Some(r),
            line: Some(file_row.line),
            column: Some(column),
            message,
        };
        let before = problems.len();
        let mut values = Vec::with_capacity(columns.len());
        for (k, &source) in sources.iter().enumerate() {
            let name = &columns[k].0;
            let cell = file_row.cells.get(source).unwrap_or(&Cell::Missing);
            let column = (!engine.takes_new_fields()).then_some(columns[k].1.as_str());
            let value = match super::value(cell, rules[k], options, column, engine) {
                Err(message) => {
                    problems.push(problem(source, message));
                    None
                }
                Ok(Some(Value::Null)) if !nullable[k] => {
                    problems.push(problem(source, format!("empty, but `{name}` is NOT NULL")));
                    None
                }
                Ok(Some(Value::Json(json))) if engine.takes_new_fields() && has_operator(&json) => {
                    problems.push(problem(
                        source,
                        format!("`{name}` holds a field starting with `$`, which is an operator, not a field DBDelve writes"),
                    ));
                    None
                }
                Ok(Some(value)) if engine.takes_new_fields() => match mql::imported(&value) {
                    Err(message) => {
                        problems.push(problem(source, message));
                        None
                    }
                    Ok(_) => Some(value),
                },
                Ok(None) => {
                    let required = matches!(mappings[source].target, Target::Column(i)
                        if !targets[i].nullable && !targets[i].has_default);
                    if required {
                        problems.push(problem(
                            source,
                            format!("missing, but `{name}` is NOT NULL"),
                        ));
                    }
                    None
                }
                Ok(value) => value,
            };
            values.push(value);
        }
        // A row already flagged for a bad value would only be flagged twice.
        if !columns.is_empty() && problems.len() == before && values.iter().all(Option::is_none) {
            problems.push(Problem {
                row: Some(r),
                line: Some(file_row.line),
                column: None,
                message: "This row has no values to import.".into(),
            });
        }
        rows.push(CheckedRow {
            line: file_row.line,
            values,
        });
    }
    Checked {
        columns,
        sources,
        rows,
        problems,
    }
}

pub(crate) struct Prepared {
    pub(crate) columns: Vec<String>,
    pub(crate) samples: Vec<Option<String>>,
    pub(crate) delimiter: Option<char>,
    pub(crate) mappings: Vec<Mapping>,
    pub(crate) checked: Arc<Checked>,
}

// ponytail: re-reads and re-checks the whole file on every option change; keep the Table between checks if large files lag
pub(crate) fn prepare(
    bytes: &[u8],
    options: &Options,
    targets: &[TargetColumn],
    previous: Option<(&[String], &[Mapping])>,
    engine: Engine,
) -> Result<Prepared, String> {
    let table = super::read(bytes, options)?;
    let mappings = match previous {
        Some((columns, mappings))
            if columns == table.columns
                && !targets.is_empty()
                && mappings
                    .iter()
                    .all(|m| !matches!(m.target, Target::Column(i) if i >= targets.len())) =>
        {
            mappings.to_vec()
        }
        _ => auto_map(
            &table.columns,
            targets,
            options.header || options.format == Format::Json,
            engine,
        ),
    };
    let samples = (0..table.columns.len())
        .map(|column| {
            table
                .rows
                .iter()
                .find_map(|row| match row.cells.get(column)? {
                    Cell::Text { text, .. } if !text.is_empty() => Some(text.clone()),
                    Cell::Json(json) if *json != Json::Null => Some(json.to_string()),
                    _ => None,
                })
        })
        .collect();
    let checked = Arc::new(check(&table, targets, &mappings, options, engine));
    Ok(Prepared {
        columns: table.columns,
        samples,
        delimiter: table.delimiter,
        mappings,
        checked,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::import_rows::{DateFormat, Decimal, read};

    fn col(
        name: &str,
        data_type: &str,
        nullable: bool,
        has_default: bool,
        generated: bool,
    ) -> TargetColumn {
        TargetColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            has_default,
            generated,
        }
    }

    fn opts(path: &str, bytes: &[u8]) -> Options {
        Options::detected(Path::new(path), bytes)
    }

    fn targets_of(maps: &[Mapping]) -> Vec<Target> {
        maps.iter().map(|m| m.target).collect()
    }

    fn whole(problem: &str) -> Problem {
        Problem {
            row: None,
            line: None,
            column: None,
            message: problem.into(),
        }
    }

    #[test]
    fn names_match_ignoring_case_underscores_dashes_and_spaces() {
        let targets = [
            col("user_id", "int", false, false, false),
            col("email", "text", false, false, false),
            col("name", "text", true, false, false),
            col("created_at", "timestamp", false, true, false),
        ];
        let file = ["User ID", "e-mail", "Name", "extra"].map(String::from);
        let got = |e| targets_of(&auto_map(&file, &targets, true, e));
        assert_eq!(
            got(Engine::Postgres),
            [
                Target::Column(0),
                Target::Column(1),
                Target::Column(2),
                Target::Skip
            ]
        );
        assert_eq!(got(Engine::MongoDb)[3], Target::NewField);
    }

    #[test]
    fn without_a_header_columns_map_by_position_past_generated_ones() {
        let targets = [
            col("id", "int", false, true, true),
            col("a", "text", true, false, false),
            col("b", "text", true, false, false),
        ];
        let file = ["Column 1".to_string(), "Column 2".into()];
        assert_eq!(
            targets_of(&auto_map(&file, &targets, false, Engine::Postgres)),
            [Target::Column(1), Target::Column(2)]
        );
    }

    #[test]
    fn a_repeated_header_maps_once() {
        let targets = [col("id", "int", false, false, false)];
        let file = ["id".to_string(), "Column 2".into(), "id".into()];
        assert_eq!(
            targets_of(&auto_map(&file, &targets, true, Engine::Postgres)),
            [Target::Column(0), Target::Skip, Target::Skip]
        );
        assert_eq!(
            targets_of(&auto_map(&file, &targets, true, Engine::MongoDb)),
            [Target::Column(0), Target::NewField, Target::Skip]
        );
    }

    #[test]
    fn choosing_a_taken_target_moves_it() {
        let mut m = vec![
            Mapping {
                target: Target::Column(0),
                rule: Rule::AsIs,
            },
            Mapping {
                target: Target::Column(1),
                rule: Rule::AsIs,
            },
        ];
        reassign(&mut m, 1, Target::Column(0));
        assert_eq!(targets_of(&m), [Target::Skip, Target::Column(0)]);
    }

    #[test]
    fn unmapped_columns_say_what_they_will_get() {
        let targets = [
            col("id", "int", false, true, true),
            col("a", "timestamp", false, true, false),
            col("b", "text", true, false, false),
            col("c", "text", false, false, false),
        ];
        assert_eq!(
            unmapped(&targets, &[], Engine::Postgres),
            [
                (0, Unmapped::Generated),
                (1, Unmapped::UsesDefault),
                (2, Unmapped::Null),
                (3, Unmapped::Required)
            ]
        );
        assert_eq!(
            unmapped(
                &[col("f", "string", true, false, false)],
                &[],
                Engine::MongoDb
            ),
            [(0, Unmapped::LeftOut)]
        );
    }

    #[test]
    fn the_parse_rules_offered_follow_the_column_type() {
        let offered = |t: &str| offered_rules(Some(&col("c", t, true, false, false)));
        let date = offered("date");
        assert!(date.contains(&Rule::Date(DateFormat::Iso)));
        assert!(!date.contains(&Rule::Boolean));
        assert!(offered("numeric(10,2)").contains(&Rule::Number(Decimal::Dot)));
        assert!(offered("boolean").contains(&Rule::Boolean));
        assert!(offered("bit").contains(&Rule::Boolean));
        assert!(offered("long").contains(&Rule::Number(Decimal::Dot)));
        assert!(!offered("interval").contains(&Rule::Number(Decimal::Dot)));
        for t in ["date", "text", "boolean"] {
            assert!(offered(t).contains(&Rule::AsIs) && offered(t).contains(&Rule::Exact));
        }
        assert_eq!(
            offered_rules(None),
            [
                Rule::AsIs,
                Rule::Date(DateFormat::Iso),
                Rule::Number(Decimal::Dot),
                Rule::Boolean,
                Rule::Exact
            ]
        );
    }

    fn maps(rules: &[Rule]) -> Vec<Mapping> {
        rules
            .iter()
            .enumerate()
            .map(|(i, rule)| Mapping {
                target: Target::Column(i),
                rule: rule.clone(),
            })
            .collect()
    }

    #[test]
    fn check_flags_every_bad_row_with_its_line() {
        let bytes = b"id,born,email\n1,31/02/2024,a@x\n2,01/01/2024,\n";
        let options = opts("a.csv", bytes);
        let table = read(bytes, &options).unwrap();
        let targets = [
            col("id", "int", false, false, false),
            col("born", "date", true, false, false),
            col("email", "text", false, false, false),
        ];
        let born = Rule::Date(DateFormat::Calendar {
            order: crate::import_rows::Order::Dmy,
            separator: '/',
            time: false,
        });
        let mappings = maps(&[Rule::AsIs, born, Rule::AsIs]);
        let checked = check(&table, &targets, &mappings, &options, Engine::Postgres);
        assert_eq!(
            checked.problems,
            [
                Problem {
                    row: Some(0),
                    line: Some(2),
                    column: Some(1),
                    message: "`31/02/2024` is not a DD/MM/YYYY date".into()
                },
                Problem {
                    row: Some(1),
                    line: Some(3),
                    column: Some(2),
                    message: "empty, but `email` is NOT NULL".into()
                }
            ]
        );
        assert_eq!(
            checked.rows[1].values[1],
            Some(Value::Date {
                text: "2024-01-01".into(),
                millis: 1_704_067_200_000
            })
        );
        assert_eq!(checked.rows.len(), table.rows.len());
    }

    #[test]
    fn an_unmapped_required_column_blocks_the_import() {
        let bytes = b"a\nx\n";
        let options = opts("a.csv", bytes);
        let targets = [
            col("a", "text", true, false, false),
            col("c", "text", false, false, false),
        ];
        let table = read(bytes, &options).unwrap();
        let mappings = [Mapping {
            target: Target::Column(0),
            rule: Rule::AsIs,
        }];
        let checked = check(&table, &targets, &mappings, &options, Engine::Postgres);
        assert_eq!(
            checked.problems,
            [whole(
                "`c` is NOT NULL, has no default, and nothing maps to it"
            )]
        );
    }

    #[test]
    fn a_header_with_no_rows_is_a_problem() {
        let bytes = b"id\n";
        let targets = [col("id", "int", false, false, false)];
        let prepared = prepare(
            bytes,
            &opts("a.csv", bytes),
            &targets,
            None,
            Engine::Postgres,
        )
        .unwrap();
        assert_eq!(
            prepared.checked.problems,
            [whole("The file has no rows to import.")]
        );
    }

    #[test]
    fn mongo_field_names_the_gate_would_refuse_are_problems() {
        let bytes = b"a.b,$x,ok\n1,2,3\n";
        let prepared = prepare(bytes, &opts("a.csv", bytes), &[], None, Engine::MongoDb).unwrap();
        let refused = |field: &str, column| Problem {
            row: None,
            line: None,
            column: Some(column),
            message: format!(
                "`{field}` is a field name DBDelve does not write: a `.` or a leading `$` would make it name another field or an operator."
            ),
        };
        assert_eq!(
            prepared.checked.problems,
            [refused("a.b", 0), refused("$x", 1)]
        );

        let bytes = br#"[{"n": 1234567890123456789012345678901234567}]"#;
        let prepared = prepare(bytes, &opts("a.json", bytes), &[], None, Engine::MongoDb).unwrap();
        assert_eq!(
            prepared.checked.problems,
            [Problem {
                row: Some(0),
                line: Some(1),
                column: Some(0),
                message: "`1234567890123456789012345678901234567` has more significant digits than a Decimal128 holds (34)".into()
            }]
        );

        let bytes = br#"[{"ok": {"$gt": 1}}]"#;
        let prepared = prepare(bytes, &opts("a.json", bytes), &[], None, Engine::MongoDb).unwrap();
        assert_eq!(
            prepared.checked.problems,
            [Problem {
                row: Some(0),
                line: Some(1),
                column: Some(0),
                message: "`ok` holds a field starting with `$`, which is an operator, not a field DBDelve writes".into()
            }]
        );
    }

    #[test]
    fn a_row_with_no_present_values_is_a_problem() {
        let bytes = br#"[{"a": 1}, {"b": 2}]"#;
        let options = opts("a.json", bytes);
        let targets = [col("a", "int", true, false, false)];
        let mut mappings = auto_map(&["a".into(), "b".into()], &targets, true, Engine::Postgres);
        mappings[1].target = Target::Skip;
        let table = read(bytes, &options).unwrap();
        let checked = check(&table, &targets, &mappings, &options, Engine::Postgres);
        assert_eq!(
            checked.problems,
            [Problem {
                row: Some(1),
                line: Some(1),
                column: None,
                message: "This row has no values to import.".into()
            }]
        );
    }

    #[test]
    fn a_sampled_mongo_field_is_never_not_null() {
        use crate::db::ColumnDefinition;
        let structure = Structure {
            columns: ["_id", "email"]
                .map(|name| ColumnDefinition {
                    name: name.into(),
                    data_type: "string".into(),
                    nullable: false,
                    default: None,
                })
                .to_vec(),
            ..Default::default()
        };
        let targets = TargetColumn::from_structure(&structure, Engine::MongoDb);
        let bytes = br#"[{"email": null}, {"email": "a"}]"#;
        let options = opts("a.json", bytes);
        let prepared = prepare(bytes, &options, &targets, None, Engine::MongoDb).unwrap();
        assert_eq!(prepared.checked.problems, []);
    }

    #[test]
    fn prepare_remaps_when_the_kept_mapping_no_longer_fits_the_targets() {
        let csv = b"id\n1\n";
        let options = opts("a.csv", csv);
        let columns = vec!["id".to_string()];
        let stale = [Mapping {
            target: Target::Column(5),
            rule: Rule::AsIs,
        }];
        let targets = [col("id", "int", false, false, false)];
        let got = prepare(
            csv,
            &options,
            &targets,
            Some((&columns, &stale)),
            Engine::Postgres,
        )
        .unwrap();
        assert_eq!(targets_of(&got.mappings), [Target::Column(0)]);
    }

    #[test]
    fn prepare_keeps_the_mapping_while_the_file_columns_stay_the_same() {
        let csv = b"id,name\n1,a\n";
        let options = opts("a.csv", csv);
        let targets = [
            col("id", "int", false, false, false),
            col("name", "text", true, false, false),
        ];
        let first = prepare(csv, &options, &targets, None, Engine::Postgres).unwrap();
        let mut mappings = first.mappings.clone();
        reassign(&mut mappings, 0, Target::Skip);

        let trimmed = Options {
            trim: true,
            ..options.clone()
        };
        let kept = prepare(
            csv,
            &trimmed,
            &targets,
            Some((&first.columns, &mappings)),
            Engine::Postgres,
        )
        .unwrap();
        assert_eq!(kept.mappings, mappings);

        let headerless = Options {
            header: false,
            ..options
        };
        let remapped = prepare(
            csv,
            &headerless,
            &targets,
            Some((&first.columns, &mappings)),
            Engine::Postgres,
        )
        .unwrap();
        assert_ne!(remapped.mappings, mappings);
    }
}

//! Which table the rows a plugin read are rows of, worked out on the host from
//! the statement and the plugin's `structure`, since a driver says nothing
//! about where a result column came from.
//!
//! Getting a name wrong here writes to the wrong table, so every doubt
//! refuses: a name the session would resolve through a search path DBDelve
//! cannot see, a name that folds to two relations, a key DBDelve cannot match
//! by its text.

use std::collections::HashSet;

use dbdelve_plugin_protocol::Structure as WireStructure;

use crate::db::{Column, DbError, EditTarget, Engine};
use crate::sql::{Projected, Quoting, TableRead, Unread, table_read_quoted};

const UNQUALIFIED: &str = "Name the table with its schema, as schema.table, to edit these rows \
                           through a plugin driver: DBDelve cannot tell which schema the session \
                           reads a bare name from.";

const TWICE: &str = "These rows hold a column twice, so they cannot be edited.";

const UNMAPPED: &str = "DBDelve could not tell which column of the table each result column is, \
                        so these rows cannot be edited.";

/// The edit target of `columns`, the result of `sql`, or why there is none.
/// `exact` says which of `columns` the plugin marked as reading back exactly,
/// and `describe` asks the plugin for one relation's structure; a relation
/// with no columns is one that does not exist. A lookup that fails answers
/// neither, since the statement has already succeeded.
pub(super) fn target(
    engine: Engine,
    sql: &str,
    columns: &[Column],
    exact: &[bool],
    describe: impl Fn(&str, &str) -> Result<WireStructure, DbError>,
) -> (Option<EditTarget>, Option<String>) {
    if columns.is_empty() {
        return (None, None);
    }
    let refused = |reason: String| (None, Some(reason));
    let (read, shadows, quoting) = match table_read_quoted(engine, sql) {
        Ok(read) => read,
        Err(why) => return refused(why.reason().into()),
    };
    let ([schema, table], [schema_quoted, table_quoted]) =
        (read.table.as_slice(), quoting.table.as_slice())
    else {
        return refused(match read.table.len() {
            1 => UNQUALIFIED.into(),
            _ => Unread::NotATable.reason().into(),
        });
    };
    // Before any lookup, since every preview's count is one of these.
    if read
        .columns
        .iter()
        .any(|projected| matches!(projected, Projected::Computed))
    {
        return refused(Unread::Projection.reason().into());
    }

    // The statement succeeded, so the server found the table under one of the
    // names it may have folded this one to: where only one exists, that is it.
    let mut found = Vec::new();
    let mut tried = HashSet::new();
    for fold in folds(engine) {
        let candidate = (
            fold.apply(schema, *schema_quoted),
            fold.apply(table, *table_quoted),
        );
        if !tried.insert(candidate.clone()) {
            continue;
        }
        match describe(&candidate.0, &candidate.1) {
            Ok(structure) if structure.columns.is_empty() => {}
            Ok(structure) => found.push((candidate, structure)),
            Err(_) => return (None, None),
        }
    }
    let ((schema, table), structure) = match found.len() {
        0 => {
            return refused(format!(
                "DBDelve found no table {schema}.{table}, so these rows cannot be edited."
            ));
        }
        1 => found.remove(0),
        _ => {
            let ((first_schema, first), (second_schema, second)) = (&found[0].0, &found[1].0);
            return refused(format!(
                "Both {first_schema}.{first} and {second_schema}.{second} exist, and DBDelve \
                 cannot tell which one the server read, so these rows cannot be edited. Quote \
                 the name exactly to say which."
            ));
        }
    };
    if structure.primary_key.is_empty() {
        return refused(format!(
            "{schema}.{table} has no primary key, so DBDelve cannot name one of its rows to \
             change it. New rows can still be inserted."
        ));
    }

    let origins = match origins(
        engine, &read, &quoting, &shadows, columns, &structure, &table,
    ) {
        Ok(origins) => origins,
        Err(reason) => return refused(reason),
    };
    let mut keys = Vec::with_capacity(structure.primary_key.len());
    for key in &structure.primary_key {
        let Some(position) = origins.iter().position(|origin| origin == key) else {
            return refused(format!(
                "The key column {key} is not among these rows' columns, so DBDelve cannot name \
                 a row to change it."
            ));
        };
        if !exact.get(position).copied().unwrap_or(false) {
            return refused(format!(
                "The driver does not read the key column {key} back exactly, so DBDelve cannot \
                 name a row by it and these rows cannot be edited."
            ));
        }
        // A name the server read as something else, `user` as the session's
        // user, would name another row. Only a key is held to its type: a
        // driver spells types its own way in a result and in its catalog
        // (psqlODBC's `timestamp without time zone` and `timestamp`, `unknown`
        // and `json`), and any other column misread is still written where
        // its header says, on the right row.
        let read_as = columns[position].data_type.as_deref().unwrap_or_default();
        let typed = structure
            .columns
            .iter()
            .find(|column| column.name == *key)
            .map(|column| column.data_type.as_str())
            .unwrap_or_default();
        if !base_type(read_as).eq_ignore_ascii_case(base_type(typed)) {
            return refused(format!(
                "The key column {key} is not read as it is typed, so DBDelve cannot tell it is \
                 the column and these rows cannot be edited."
            ));
        }
        keys.push(position);
    }
    let target = EditTarget {
        schema,
        table,
        columns: origins.into_iter().map(Some).collect(),
        keys,
        ..EditTarget::default()
    };
    (Some(target), None)
}

/// The table column behind each of `columns`, in the catalog's own case, or
/// why the rows cannot be written back. A `*` stands for as many result
/// columns as the rest of the list leaves, named by the driver as the table
/// names them; a name in the list is resolved as the server folds it.
///
/// A name the list reads as something else too (an alias, the table's
/// qualifier) refuses the lot when a column has it, whatever its case, since
/// the server may read it either way.
fn origins(
    engine: Engine,
    read: &TableRead,
    quoting: &Quoting,
    shadows: &[String],
    columns: &[Column],
    structure: &WireStructure,
    table: &str,
) -> Result<Vec<String>, String> {
    let known = |name: &str| structure.columns.iter().find(|column| column.name == name);
    if let Some(shadow) = shadows.iter().find(|shadow| {
        structure
            .columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(shadow))
    }) {
        return Err(format!(
            "{shadow} is a column's name and is read here as something else too, so DBDelve \
             cannot tell which one each value is."
        ));
    }
    let stars = read
        .columns
        .iter()
        .filter(|projected| matches!(projected, Projected::Every))
        .count();
    if stars > 1 {
        return Err(TWICE.into());
    }
    let spread = columns
        .len()
        .checked_sub(read.columns.len() - stars)
        .ok_or_else(|| UNMAPPED.to_string())?;
    let mut origins = Vec::with_capacity(columns.len());
    for (projected, quoted) in read.columns.iter().zip(&quoting.columns) {
        match projected {
            Projected::Every => {
                let at = origins.len();
                origins.extend(columns[at..at + spread].iter().map(|c| c.name.clone()));
            }
            Projected::Column(written) => {
                let mut matched: Vec<String> = Vec::new();
                for fold in folds(engine) {
                    let name = fold.apply(written, *quoted);
                    if known(&name).is_some() && !matched.contains(&name) {
                        matched.push(name);
                    }
                }
                match matched.as_slice() {
                    [] => {
                        return Err(format!(
                            "{written} is not a column of {table}, so these rows cannot be edited."
                        ));
                    }
                    [one] => origins.push(one.clone()),
                    [first, second, ..] => {
                        return Err(format!(
                            "{table} has both a column {first} and a column {second}, and DBDelve \
                             cannot tell which one {written} is, so these rows cannot be edited. \
                             Quote the name exactly to say which."
                        ));
                    }
                }
            }
            Projected::Computed => return Err(Unread::Projection.reason().into()),
        }
    }
    if origins.len() != columns.len() {
        return Err(UNMAPPED.into());
    }
    if let Some(missing) = origins.iter().find(|origin| known(origin).is_none()) {
        return Err(format!(
            "{missing} is not a column of {table}, so these rows cannot be edited."
        ));
    }
    let mut seen = HashSet::new();
    if !origins.iter().all(|origin| seen.insert(origin)) {
        return Err(TWICE.into());
    }
    Ok(origins)
}

/// A type's name without the size or precision a catalog adds to it, which a
/// result's description leaves out: `VARCHAR(20)` is `VARCHAR`.
fn base_type(name: &str) -> &str {
    name.split('(').next().unwrap_or_default().trim()
}

#[derive(Clone, Copy)]
enum Fold {
    Upper,
    Lower,
}

impl Fold {
    /// `name` as the server stores it under this fold: as written when quoted.
    fn apply(self, name: &str, quoted: bool) -> String {
        match (quoted, self) {
            (true, _) => name.to_owned(),
            (false, Self::Upper) => name.to_uppercase(),
            (false, Self::Lower) => name.to_lowercase(),
        }
    }
}

/// How `engine`'s server may store an unquoted name. Postgres folds it to
/// lower case. A server spoken to as Generic SQL may be either kind: Db2 and
/// Oracle fold up, as the standard does, and most others down, so both are
/// looked up and a name both find is refused.
fn folds(engine: Engine) -> &'static [Fold] {
    match engine {
        Engine::Postgres => &[Fold::Lower],
        _ => &[Fold::Upper, Fold::Lower],
    }
}

#[cfg(test)]
mod tests {
    use dbdelve_plugin_protocol::Column as WireColumn;

    use super::*;

    fn structure(columns: &[(&str, &str)], key: &[&str]) -> WireStructure {
        WireStructure {
            columns: columns
                .iter()
                .map(|(name, data_type)| WireColumn {
                    name: (*name).into(),
                    data_type: (*data_type).into(),
                    nullable: true,
                    default: None,
                })
                .collect(),
            primary_key: key.iter().map(|name| (*name).into()).collect(),
            ..WireStructure::default()
        }
    }

    fn result(columns: &[(&str, &str)]) -> Vec<Column> {
        columns
            .iter()
            .map(|(name, data_type)| Column {
                name: (*name).into(),
                data_type: Some((*data_type).into()),
            })
            .collect()
    }

    /// A catalog of `tables`, each `(schema, table, structure)`, answering
    /// an empty structure for anything else, as the ODBC bridge does.
    fn catalog(
        tables: Vec<(&'static str, &'static str, WireStructure)>,
    ) -> impl Fn(&str, &str) -> Result<WireStructure, DbError> {
        move |schema, table| {
            Ok(tables
                .iter()
                .find(|(s, t, _)| *s == schema && *t == table)
                .map(|(_, _, structure)| structure.clone())
                .unwrap_or_default())
        }
    }

    fn accounts() -> WireStructure {
        structure(&[("id", "int4"), ("label", "varchar(20)")], &["id"])
    }

    #[test]
    fn a_qualified_select_of_a_keyed_table_names_its_rows_by_the_key() {
        // psqlODBC's result says `unknown` of a type its catalog names.
        let columns = result(&[("id", "INT4"), ("label", "unknown")]);
        let describe = catalog(vec![("app", "accounts", accounts())]);
        for sql in [
            "SELECT * FROM app.accounts",
            "SELECT id, label FROM app.accounts WHERE id > 1",
            "SELECT \"id\", label FROM \"app\".\"accounts\"",
            "SELECT accounts.id, label FROM app.accounts",
        ] {
            let (target, reason) =
                target(Engine::Postgres, sql, &columns, &[true, false], &describe);
            let target = target.unwrap_or_else(|| panic!("{sql}: {reason:?}"));
            assert_eq!(
                (target.schema.as_str(), target.table.as_str()),
                ("app", "accounts")
            );
            assert_eq!(target.columns, [Some("id".into()), Some("label".into())]);
            assert_eq!(target.keys, [0]);
        }
    }

    fn refusal(engine: Engine, sql: &str, columns: &[Column], exact: &[bool]) -> String {
        let describe = catalog(vec![
            ("app", "accounts", accounts()),
            ("app", "keyless", structure(&[("id", "int4")], &[])),
        ]);
        let (target, reason) = target(engine, sql, columns, exact, describe);
        assert_eq!(target, None, "{sql}");
        reason.unwrap_or_else(|| panic!("{sql}: no reason"))
    }

    #[test]
    fn every_doubt_about_the_rows_refuses_in_its_own_words() {
        let both = result(&[("id", "int4"), ("label", "varchar")]);
        let cases = [
            (
                "SELECT * FROM accounts",
                &both,
                &[true, true][..],
                UNQUALIFIED.to_string(),
            ),
            (
                "SELECT * FROM app.missing",
                &both,
                &[true, true],
                "DBDelve found no table app.missing, so these rows cannot be edited.".into(),
            ),
            (
                "SELECT id FROM app.keyless",
                &result(&[("id", "int4")]),
                &[true],
                "app.keyless has no primary key, so DBDelve cannot name one of its rows to \
                 change it. New rows can still be inserted."
                    .into(),
            ),
            (
                "SELECT label FROM app.accounts",
                &result(&[("label", "varchar")]),
                &[true],
                "The key column id is not among these rows' columns, so DBDelve cannot name a \
                 row to change it."
                    .into(),
            ),
            (
                "SELECT * FROM app.accounts",
                &both,
                &[false, true],
                "The driver does not read the key column id back exactly, so DBDelve cannot \
                 name a row by it and these rows cannot be edited."
                    .into(),
            ),
            (
                "SELECT id, upper(label) FROM app.accounts",
                &both,
                &[true, true],
                Unread::Projection.reason().into(),
            ),
            (
                "SELECT id, id FROM app.accounts",
                &result(&[("id", "int4"), ("id", "int4")]),
                &[true, true],
                TWICE.into(),
            ),
            (
                "SELECT label AS id, id AS label FROM app.accounts",
                &both,
                &[true, true],
                "id is a column's name and is read here as something else too, so DBDelve \
                 cannot tell which one each value is."
                    .into(),
            ),
            (
                "SELECT id, label FROM app.accounts",
                &result(&[("id", "name"), ("label", "varchar")]),
                &[true, true],
                "The key column id is not read as it is typed, so DBDelve cannot tell it is the \
                 column and these rows cannot be edited."
                    .into(),
            ),
            (
                "SELECT id, \"Label\" FROM app.accounts",
                &both,
                &[true, true],
                "Label is not a column of accounts, so these rows cannot be edited.".into(),
            ),
            (
                "SELECT a.id FROM app.accounts a JOIN app.keyless k ON a.id = k.id",
                &result(&[("id", "int4")]),
                &[true],
                Unread::Join.reason().into(),
            ),
        ];
        for (sql, columns, exact, reason) in cases {
            assert_eq!(
                refusal(Engine::Postgres, sql, columns, exact),
                reason,
                "{sql}"
            );
        }
    }

    #[test]
    fn a_statement_with_no_rows_or_a_lookup_that_fails_says_nothing() {
        let failing = |_: &str, _: &str| {
            Err(DbError {
                message: "gone".into(),
                position: None,
            })
        };
        let columns = result(&[("id", "int4")]);
        let sql = "SELECT id FROM app.accounts";
        assert_eq!(
            target(Engine::Postgres, sql, &columns, &[true], failing),
            (None, None)
        );
        let describe = catalog(vec![]);
        assert_eq!(
            target(
                Engine::Postgres,
                "UPDATE app.accounts SET id = 1",
                &[],
                &[],
                describe
            ),
            (None, None)
        );
    }

    #[test]
    fn postgres_folds_an_unquoted_name_down_and_keeps_a_quoted_one() {
        let columns = result(&[("id", "int4"), ("label", "varchar")]);
        let upper = structure(&[("ID", "int4"), ("label", "varchar")], &["ID"]);
        let describe = catalog(vec![
            ("app", "accounts", accounts()),
            ("APP", "ACCOUNTS", upper),
        ]);
        let (lower, _) = target(
            Engine::Postgres,
            "SELECT * FROM APP.Accounts",
            &columns,
            &[true, true],
            &describe,
        );
        assert_eq!(lower.unwrap().schema, "app");
        let (exact, _) = target(
            Engine::Postgres,
            "SELECT \"ID\", label FROM \"APP\".\"ACCOUNTS\"",
            &columns,
            &[true, true],
            &describe,
        );
        let exact = exact.unwrap();
        assert_eq!(
            (exact.schema.as_str(), exact.table.as_str()),
            ("APP", "ACCOUNTS")
        );
        assert_eq!(exact.columns[0].as_deref(), Some("ID"));
    }

    #[test]
    fn generic_takes_the_one_fold_that_exists_and_refuses_when_both_do() {
        let columns = result(&[("ID", "INTEGER"), ("LABEL", "VARCHAR")]);
        let items = structure(&[("ID", "INTEGER"), ("LABEL", "VARCHAR(20)")], &["ID"]);
        let describe = catalog(vec![
            ("DB2INST1", "ITEMS", items.clone()),
            ("db2inst1", "both", items.clone()),
            ("DB2INST1", "BOTH", items),
        ]);
        let generic = |sql| target(Engine::Generic, sql, &columns, &[true, true], &describe);

        let (upper, reason) = generic("SELECT id, label FROM db2inst1.items");
        let upper = upper.unwrap_or_else(|| panic!("{reason:?}"));
        assert_eq!(
            (upper.schema.as_str(), upper.table.as_str()),
            ("DB2INST1", "ITEMS")
        );
        assert_eq!(upper.columns, [Some("ID".into()), Some("LABEL".into())]);

        let (none, reason) = generic("SELECT * FROM db2inst1.both");
        assert_eq!(none, None);
        assert_eq!(
            reason.unwrap(),
            "Both DB2INST1.BOTH and db2inst1.both exist, and DBDelve cannot tell which one the \
             server read, so these rows cannot be edited. Quote the name exactly to say which."
        );

        let (quoted, _) = generic("SELECT * FROM \"db2inst1\".\"both\"");
        assert_eq!(quoted.unwrap().table, "both");
    }

    #[test]
    fn generic_refuses_a_column_both_folds_find() {
        let mixed = structure(
            &[
                ("id", "INTEGER"),
                ("label", "VARCHAR"),
                ("LABEL", "VARCHAR"),
            ],
            &["id"],
        );
        let describe = catalog(vec![("app", "mixed", mixed)]);
        let columns = result(&[("id", "INTEGER"), ("label", "VARCHAR")]);
        let sql = "SELECT id, label FROM app.mixed";
        let (none, reason) = target(Engine::Generic, sql, &columns, &[true, true], &describe);
        assert_eq!(none, None);
        assert_eq!(
            reason.unwrap(),
            "mixed has both a column LABEL and a column label, and DBDelve cannot tell which one \
             label is, so these rows cannot be edited. Quote the name exactly to say which."
        );
        let (postgres, _) = target(Engine::Postgres, sql, &columns, &[true, true], &describe);
        assert_eq!(postgres.unwrap().columns[1].as_deref(), Some("label"));
    }
}

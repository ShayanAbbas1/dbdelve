use super::{TYPES, VALUES, check, home, statement};
use crate::{
    db::{ColumnDefinition, Engine},
    explorer::{PREVIEW_ROW_LIMIT, count_sql, preview_sql},
    filter::{Conjunction, FilterBar, Operator, derived_filter, filter_predicate},
};

fn value(case: &str) -> &'static str {
    VALUES.iter().find(|(name, _)| *name == case).unwrap().1
}

fn composed(engine: Engine, predicate: Option<&str>) -> Option<String> {
    let predicate = predicate?;
    let (schema, relation) = home(engine);
    Some(preview_sql(
        engine,
        schema,
        relation,
        predicate,
        Some(PREVIEW_ROW_LIMIT),
        0,
    ))
}

#[test]
fn every_offered_operator_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let columns: [&str; 2] = if engine == Engine::Redis {
            ["key", "type"]
        } else {
            ["Name", "we\"ird"]
        };
        for column in columns {
            for operator in Operator::ALL {
                if !operator.offered(engine, Some(column)) {
                    continue;
                }
                let values: Vec<(String, String)> = match operator {
                    Operator::IsNull
                    | Operator::IsNotNull
                    | Operator::IsEmpty
                    | Operator::IsNotEmpty => vec![("empty".into(), String::new())],
                    Operator::InList | Operator::NotInList => vec![
                        ("list".into(), "a, it's, , b".into()),
                        ("blank list".into(), "  ".into()),
                    ],
                    Operator::Between => vec![
                        ("range".into(), "1..9".into()),
                        ("dated range".into(), " 2024-01-01 .. 2024-12-31 ".into()),
                        ("half range".into(), "1..".into()),
                    ],
                    _ => [
                        "plain",
                        "single quote",
                        "backslash before quote",
                        "odd backslashes then quote",
                        "unicode",
                        "like metacharacters",
                        "glob metacharacters",
                        "empty",
                    ]
                    .map(|case| (case.to_owned(), value(case).to_owned()))
                    .to_vec(),
                };
                for (case, text) in values {
                    let predicate = filter_predicate(engine, column, None, operator, &text);
                    let record = composed(engine, predicate.as_deref());
                    statement(
                        &mut out,
                        engine,
                        &format!("{column} {operator:?} · {case}"),
                        record.as_deref(),
                    );
                }
            }
        }
    }
    check("filters_operators", &out);
}

const MONGO_TYPED: [(Option<&str>, &[&str]); 21] = [
    (
        Some("objectId"),
        &[
            "507f1f77bcf86cd799439011",
            "ObjectId('507f1f77bcf86cd799439011')",
            "ObjectId(\"507f1f77bcf86cd799439011\")",
            "ObjectId( 507f1f77bcf86cd799439011 )",
            "507F1F77BCF86CD799439011",
            "507f1f77bcf86cd79943901",
            "507f1f77bcf86cd7994390112",
            "zzzzzzzzzzzzzzzzzzzzzzzz",
            "ObjectId('')",
            "",
        ],
    ),
    (
        Some("date"),
        &[
            "2024-01-02T03:04:05Z",
            "2024-01-02T03:04:05+05:30",
            "2024-01-02T03:04:05.123-08:00",
            "2024-01-02",
            "2024-01-02 03:04:05",
            "yesterday",
            "2024-13-45",
            "",
            "2024-01-02T25:00:00Z",
        ],
    ),
    (
        Some("bool"),
        &["true", "false", " true ", "TRUE", "1", "yes", ""],
    ),
    (
        Some("int"),
        &[
            "42",
            "-7",
            " 42 ",
            "2147483647",
            "2147483648",
            "-2147483649",
            "9223372036854775807",
            "9223372036854775808",
            "1.5",
            "1e3",
            "007",
            "abc",
            "NaN",
            "inf",
            "1e999",
            "",
        ],
    ),
    (Some("long"), &["42", "2147483648", "-9223372036854775808"]),
    (Some("double"), &["1.5", "3", "1e400"]),
    (
        Some("decimal"),
        &[
            "1.5",
            "0.1",
            "-0.0",
            "123456789012345678901234567890.123456789",
            "12345678901234567890123456789012345678",
            "1e3",
            "abc",
            "NaN",
            "",
        ],
    ),
    (Some("int|null"), &["42", "abc"]),
    (Some("int|long"), &["42", "2147483648"]),
    (Some("int|decimal"), &["1.5"]),
    (Some("int|string"), &["42"]),
    (Some("objectId|null"), &["507f1f77bcf86cd799439011"]),
    (Some("objectId|string"), &["507f1f77bcf86cd799439011"]),
    (Some("date|null"), &["2024-01-02T03:04:05Z"]),
    (Some("null"), &["42", "abc"]),
    (Some("string"), &["42", "true"]),
    (Some(""), &["42", "true", "2024-01-02T03:04:05Z"]),
    (None, &["42", "true", "2024-01-02T03:04:05Z"]),
    (Some("objectId, date"), &["507f1f77bcf86cd799439011"]),
    (Some("date|string"), &["2024-01-02T03:04:05Z"]),
    (Some("bool|null"), &["true", "TRUE"]),
];

#[test]
fn typed_predicates_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        if matches!(engine, Engine::MongoDb | Engine::Redis) {
            continue;
        }
        for operator in [
            Operator::Equals,
            Operator::NotEquals,
            Operator::Contains,
            Operator::InList,
            Operator::Between,
            Operator::IsEmpty,
        ] {
            if !operator.offered(engine, Some("Name")) {
                continue;
            }
            let texts: &[&str] = match operator {
                Operator::InList => &["1, 2"],
                Operator::Between => &["1..9"],
                Operator::IsEmpty => &[""],
                _ => &[
                    value("plain"),
                    value("unicode"),
                    value("hex"),
                    value("timestamp"),
                    value("number"),
                ],
            };
            for ty in TYPES {
                for text in texts {
                    let predicate = filter_predicate(engine, "Name", ty, operator, text);
                    let record = composed(engine, predicate.as_deref());
                    statement(
                        &mut out,
                        engine,
                        &format!("{operator:?} {} · {text}", ty.unwrap_or("none")),
                        record.as_deref(),
                    );
                }
            }
        }
    }
    let engine = Engine::MongoDb;
    for (data_type, texts) in MONGO_TYPED {
        for operator in [
            Operator::Equals,
            Operator::NotEquals,
            Operator::InList,
            Operator::Between,
        ] {
            if !operator.offered(engine, Some("field")) {
                continue;
            }
            for text in texts {
                let text = match operator {
                    Operator::InList => format!("{text}, {text}"),
                    Operator::Between => format!("{text}..{text}"),
                    _ => (*text).to_owned(),
                };
                let predicate = filter_predicate(engine, "field", data_type, operator, &text);
                let record = composed(engine, predicate.as_deref());
                statement(
                    &mut out,
                    engine,
                    &format!(
                        "mongo {} {operator:?} · {text}",
                        data_type.map_or("(no type)".to_owned(), |ty| format!("{ty:?}"))
                    ),
                    record.as_deref(),
                );
            }
        }
    }
    check("filters_typed", &out);
}

fn bar(
    column: Option<&str>,
    operator: Operator,
    conjunction: Conjunction,
    value: &str,
) -> FilterBar {
    FilterBar {
        column: column.map(str::to_owned),
        operator,
        conjunction,
        value: value.to_owned(),
        ..FilterBar::default()
    }
}

#[test]
fn bars_fold_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let (name, number) = if engine == Engine::Redis {
            ("key", "type")
        } else {
            ("Name", "n")
        };
        let columns: Vec<ColumnDefinition> = [(name, "nvarchar(50)"), (number, "int")]
            .map(|(name, data_type)| ColumnDefinition {
                name: name.to_owned(),
                data_type: data_type.to_owned(),
                nullable: true,
                default: None,
            })
            .to_vec();
        let raw_text = match engine {
            Engine::MongoDb => "{\"n\": {\"$gt\": 1}}",
            Engine::Redis => "a*",
            _ => "n > 1",
        };
        let raw = |conjunction| FilterBar {
            raw: true,
            conjunction,
            value: raw_text.to_owned(),
            ..FilterBar::default()
        };
        let equals = |conjunction, text| bar(Some(name), Operator::Equals, conjunction, text);
        // Redis offers no comparison on `type`, and a bar it skips would leave
        // its folded sets with nothing to fold.
        let second = |conjunction| {
            if engine == Engine::Redis {
                bar(Some(name), Operator::Contains, conjunction, "5")
            } else {
                bar(Some(number), Operator::Greater, conjunction, "5")
            }
        };
        let skipped = if Operator::Regex.offered(engine, Some(name)) {
            bar(None, Operator::Equals, Conjunction::And, "x")
        } else {
            bar(Some(name), Operator::Regex, Conjunction::And, "x")
        };
        let sets: [(&str, Vec<FilterBar>); 8] = [
            ("no bars", vec![]),
            ("one equals", vec![equals(Conjunction::And, "it's")]),
            (
                "and",
                vec![equals(Conjunction::And, "it's"), second(Conjunction::And)],
            ),
            (
                "or",
                vec![equals(Conjunction::And, "it's"), second(Conjunction::Or)],
            ),
            ("raw alone", vec![raw(Conjunction::And)]),
            (
                "raw or equals",
                vec![raw(Conjunction::And), equals(Conjunction::Or, "ok")],
            ),
            (
                "and then or",
                vec![
                    equals(Conjunction::And, "it's"),
                    second(Conjunction::And),
                    bar(Some(name), Operator::Contains, Conjunction::Or, "z"),
                ],
            ),
            (
                "skipped bar",
                vec![equals(Conjunction::And, "it's"), skipped],
            ),
        ];
        let (schema, relation) = home(engine);
        for (case, bars) in sets {
            let filter = derived_filter(engine, &bars, &columns);
            let preview = composed(engine, Some(&filter));
            statement(
                &mut out,
                engine,
                &format!("{case} · preview"),
                preview.as_deref(),
            );
            let count = count_sql(engine, schema, relation, &filter);
            statement(&mut out, engine, &format!("{case} · count"), Some(&count));
        }
    }
    check("filters_bars", &out);
}

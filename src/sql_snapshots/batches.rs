use super::{
    KEY, RELATIONS, check, column_types, fragment, home, mongo_types, outcome, refusal, set,
    statement_with,
};
use crate::{
    db::Engine,
    import_rows::{Json, Value},
    redis_cmd::edit::{INTEGER, STRING},
    result_grid::{NewValue, PendingRow},
    sql,
};

fn pending(
    schema: &str,
    table: &str,
    sets: &[(&str, NewValue)],
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> PendingRow {
    PendingRow {
        schema: schema.to_owned(),
        table: table.to_owned(),
        sets: sets
            .iter()
            .map(|(column, value)| ((*column).to_owned(), value.clone()))
            .collect(),
        keys: keys
            .iter()
            .map(|(column, value)| ((*column).to_owned(), (*value).to_owned()))
            .collect(),
        types: types.to_vec(),
        held: Vec::new(),
    }
}

#[test]
fn update_batches_per_engine() {
    let mut out = String::new();
    let record = |out: &mut String, engine: Engine, case: &str, rows: &[PendingRow], ok| {
        outcome(out, engine, case, sql::update_batch(engine, rows), ok);
    };
    for engine in Engine::ALL {
        let (schema, table) = home(engine);
        match engine {
            Engine::MongoDb => {
                let types = mongo_types();
                let a = "65f0c0ffee0000000000abcd";
                let b = "65f0c0ffee0000000000abce";
                let one = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
                    [pending(schema, table, sets, keys, &types)]
                };
                let name = [("Name", set("it's"))];
                record(
                    &mut out,
                    engine,
                    "one row",
                    &one(&name, &[("_id", a)]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "two rows",
                    &[
                        pending(schema, table, &name, &[("_id", a)], &types),
                        pending(
                            schema,
                            table,
                            &[("Name", NewValue::Null)],
                            &[("_id", b)],
                            &types,
                        ),
                    ],
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "several sets",
                    &one(
                        &[
                            ("Name", set("it's")),
                            ("flag", set("true")),
                            ("id", set("42")),
                            ("price", set("19.99")),
                        ],
                        &[("_id", a)],
                    ),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "typed sets",
                    &one(
                        &[
                            ("created", set("2024-01-02T03:04:05.123Z")),
                            ("big", set("9007199254740993")),
                            ("ref", set("507f1f77bcf86cd799439011")),
                            ("score", set("1.5")),
                        ],
                        &[("_id", a)],
                    ),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "nullable typed set",
                    &one(&[("maybe", set("7"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "mixed typed set",
                    &one(&[("mixed", set("42"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "integer key",
                    &one(&name, &[("id", "7")]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "composite key",
                    &one(&name, &[("_id", a), ("flag", "true")]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "quoted key field",
                    &one(&name, &[("we\"ird", "1")]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "default",
                    &one(&[("Name", NewValue::Default)], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "invalid typed set",
                    &one(&[("id", set("abc"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "invalid typed key",
                    &one(&name, &[("_id", "xyz")]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "empty field name",
                    &one(&[("", set("v"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "dotted field name",
                    &one(&[("a.b", set("v"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "operator field name",
                    &one(&[("$set", set("v"))], &[("_id", a)]),
                    false,
                );
                record(
                    &mut out,
                    engine,
                    "operator value",
                    &one(&[("x", set("{\"$gt\": 1}"))], &[("_id", a)]),
                    false,
                );
                record(&mut out, engine, "empty", &[], false);
                record(&mut out, engine, "keyless", &one(&name, &[]), false);
            }
            Engine::Redis => {
                let types: Vec<(String, String)> =
                    [("key", STRING), ("ttl", INTEGER), ("value", STRING)]
                        .map(|(column, tag)| (column.to_owned(), tag.to_owned()))
                        .to_vec();
                let one = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
                    [pending(schema, table, sets, keys, &types)]
                };
                record(
                    &mut out,
                    engine,
                    "keys grid edit",
                    &one(
                        &[
                            ("key", set("user:2 \"new\"")),
                            ("value", set("a\nb é")),
                            ("ttl", set("60")),
                        ],
                        &[("key", "user:1")],
                    ),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "persist from null",
                    &one(&[("ttl", NewValue::Null)], &[("key", "k")]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "persist from empty",
                    &one(&[("ttl", set(""))], &[("key", "k")]),
                    true,
                );
                record(&mut out, engine, "empty", &[], false);
                record(
                    &mut out,
                    engine,
                    "keyless",
                    &one(&[("value", set("v"))], &[]),
                    false,
                );
            }
            _ => {
                let types = column_types();
                let name = [("Name", set("it's"))];
                let one = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
                    [pending(schema, table, sets, keys, &types)]
                };
                record(
                    &mut out,
                    engine,
                    "one row",
                    &one(&name, &[("id", "7")]),
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "two rows",
                    &[
                        pending(schema, table, &name, &[("id", "7")], &types),
                        pending(
                            schema,
                            table,
                            &[("Name", NewValue::Null)],
                            &[("id", "8")],
                            &types,
                        ),
                    ],
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "composite keys",
                    &[
                        pending(schema, table, &name, &KEY, &types),
                        pending(
                            schema,
                            table,
                            &name,
                            &[("id", "8"), ("Region", "us")],
                            &types,
                        ),
                    ],
                    true,
                );
                record(
                    &mut out,
                    engine,
                    "default",
                    &one(&[("Name", NewValue::Default)], &[("id", "7")]),
                    true,
                );
                record(&mut out, engine, "empty", &[], false);
                record(&mut out, engine, "keyless", &one(&name, &[]), false);
            }
        }
    }
    check("batches_update", &out);
}

fn expect_by_engine(engine: Engine, skip: bool) -> bool {
    match engine {
        Engine::Snowflake | Engine::SqlServer => !skip,
        Engine::Redis => false,
        _ => true,
    }
}

type Rows = Vec<Vec<Option<Value>>>;

fn import_case(
    out: &mut String,
    engine: Engine,
    case: &str,
    (schema, table): (&str, &str),
    columns: &[(&str, &str)],
    rows: &Rows,
    expect: fn(Engine, bool) -> bool,
) {
    let borrowed: Vec<Vec<Option<&Value>>> = rows
        .iter()
        .map(|row| row.iter().map(Option::as_ref).collect())
        .collect();
    for skip in [false, true] {
        let text = sql::insert_rows(engine, schema, table, columns, &borrowed, skip);
        outcome(
            out,
            engine,
            &format!("{case} · skip={skip}"),
            text,
            expect(engine, skip),
        );
    }
}

fn some(values: Vec<Value>) -> Vec<Option<Value>> {
    values.into_iter().map(Some).collect()
}

fn mongo_only_false(engine: Engine, skip: bool) -> bool {
    engine != Engine::MongoDb && expect_by_engine(engine, skip)
}

#[test]
fn import_chunks_per_engine() {
    let mut out = String::new();
    let base_columns = [
        ("id", "integer"),
        ("Name", "text"),
        ("flag", "boolean"),
        ("at", "timestamp"),
    ];
    let base_rows: Rows = vec![
        some(vec![
            Value::Number("1".into()),
            Value::Text("it's \\ é".into()),
            Value::Bool(true),
            Value::Date {
                text: "2024-01-02T03:04:05.123Z".into(),
                millis: 1_704_164_645_123,
            },
        ]),
        some(vec![
            Value::Number("-2.5".into()),
            Value::Null,
            Value::Bool(false),
            Value::Text(String::new()),
        ]),
    ];
    let json = |text: &str| Value::Json(serde_json::from_str::<Json>(text).unwrap());
    let doc_columns = [("id", "integer"), ("doc", "json")];
    let one_text = |text: &str| -> Rows { vec![some(vec![Value::Text(text.into())])] };
    let numbers: Rows = [
        "9223372036854775807",
        "9223372036854775808",
        "0.1",
        "12345678901234567890",
    ]
    .iter()
    .map(|n| some(vec![Value::Number((*n).into())]))
    .collect();
    for engine in Engine::ALL {
        let home = home(engine);
        let mut run = |case: &str,
                       relation: (&str, &str),
                       columns: &[(&str, &str)],
                       rows: &Rows,
                       expect: fn(Engine, bool) -> bool| {
            import_case(&mut out, engine, case, relation, columns, rows, expect);
        };
        run("base", home, &base_columns, &base_rows, expect_by_engine);
        run(
            "json cell",
            home,
            &doc_columns,
            &vec![some(vec![
                Value::Number("1".into()),
                json(r#"{"a":[1,"it's",null,{"b":true}],"c":"x\\\\y"}"#),
            ])],
            expect_by_engine,
        );
        run(
            "json null and scalar",
            home,
            &[("doc", "json")],
            &vec![
                some(vec![json("null")]),
                some(vec![json("\"text\"")]),
                some(vec![json("7")]),
            ],
            expect_by_engine,
        );
        run(
            "missing cell",
            home,
            &[("id", "integer"), ("Name", "text")],
            &vec![vec![Some(Value::Number("1".into())), None]],
            |engine, _| engine == Engine::MongoDb,
        );
        run("empty columns", home, &[], &vec![vec![]], |engine, _| {
            engine == Engine::MongoDb
        });
        run("empty rows", home, &base_columns, &vec![], |_, _| false);
        run(
            "big numbers",
            home,
            &[("n", "numeric")],
            &numbers,
            expect_by_engine,
        );
        if engine == Engine::MongoDb {
            run(
                "35 digit number",
                home,
                &[("n", "numeric")],
                &vec![some(vec![Value::Number("1".repeat(35))])],
                |_, _| false,
            );
        }
        for name in ["we\"ird", "back`tick", "it's", "[x]", "a.b", ""] {
            let rejected = matches!(name, "a.b" | "");
            run(
                &format!("conflict column {name}"),
                home,
                &[(name, "text")],
                &one_text("v"),
                if rejected {
                    mongo_only_false
                } else {
                    expect_by_engine
                },
            );
        }
        run(
            "operator column",
            home,
            &[("$set", "text")],
            &one_text("v"),
            mongo_only_false,
        );
        for (case, schema, relation) in RELATIONS {
            run(
                &format!("relation {case}"),
                (schema, relation),
                &base_columns,
                &base_rows,
                expect_by_engine,
            );
        }
    }
    check("batches_import", &out);
}

#[test]
fn transaction_brackets_per_engine() {
    let mut out = String::new();
    let bracket = |text: &str| format!(" bracket={}", sql::is_transaction_bracket(text));
    for engine in Engine::ALL {
        match engine.all_or_nothing() {
            Ok(text) => statement_with(
                &mut out,
                engine,
                "all_or_nothing",
                Some(text),
                &bracket(text),
            ),
            Err(message) => refusal(&mut out, engine, "all_or_nothing", message),
        }
        let start = engine.transaction_start();
        let extra = start.map(bracket).unwrap_or_default();
        statement_with(&mut out, engine, "transaction_start", start, &extra);
        for text in ["COMMIT", "ROLLBACK"] {
            statement_with(&mut out, engine, text, Some(text), &bracket(text));
        }
    }
    check("batches_brackets", &out);
}

#[test]
fn appended_statements_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let types = if engine == Engine::MongoDb {
            mongo_types()
        } else {
            column_types()
        };
        let generated = if engine == Engine::Redis {
            "UNLINK k".to_owned()
        } else {
            sql::delete_row(engine, "public", "accounts", &[("id", "7")], &types).unwrap()
        };
        let mut buffers = vec![
            "",
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1 -- note",
            "SELECT 1\n/* c */\n",
        ];
        match engine {
            Engine::MongoDb => buffers.push(r#"db.getCollection("a").find({})"#),
            Engine::Redis => buffers.push("GET k"),
            _ => {}
        }
        for buffer in buffers {
            let appended = sql::appended_statement(engine, buffer, &generated);
            fragment(
                &mut out,
                engine,
                &format!("buffer {buffer:?}"),
                Some(&appended),
            );
        }
    }
    check("batches_appended", &out);
}

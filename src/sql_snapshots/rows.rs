use super::{
    KEY, NAMES, RELATIONS, VALUES, check, column_types_of, fragment, long_text, mongo_types,
    outcome, set, statement, statement_with,
};
use crate::{
    db::{ColumnDefinition, Engine, Syntax},
    redis_cmd::{
        browse::KEYS,
        edit::{HASH, LIST, SET, STRING, ZSET},
    },
    result_grid::NewValue,
    sql,
};

fn sql_engines() -> impl Iterator<Item = Engine> {
    Engine::ALL
        .into_iter()
        .filter(|engine| engine.syntax() == Syntax::Sql)
}

#[test]
fn update_rows_per_engine() {
    type Case<'a> = (
        String,
        &'a str,
        &'a str,
        Vec<(&'a str, NewValue)>,
        Vec<(&'a str, &'a str)>,
        bool,
    );
    let long = long_text();
    let mut cases: Vec<Case> = Vec::new();
    let id = vec![("id", "7")];
    for (case, value) in VALUES {
        cases.push((
            format!("plain · {case}"),
            "public",
            "accounts",
            vec![("Name", set(value))],
            id.clone(),
            true,
        ));
    }
    cases.push((
        "plain · long".into(),
        "public",
        "accounts",
        vec![("Name", set(&long))],
        id.clone(),
        true,
    ));
    cases.push((
        "null".into(),
        "public",
        "accounts",
        vec![("Name", NewValue::Null)],
        id.clone(),
        true,
    ));
    cases.push((
        "default".into(),
        "public",
        "accounts",
        vec![("Name", NewValue::Default)],
        id.clone(),
        true,
    ));
    cases.push((
        "every name".into(),
        "public",
        "accounts",
        NAMES.iter().map(|name| (*name, set("v"))).collect(),
        id.clone(),
        true,
    ));
    cases.push((
        "composite key".into(),
        "public",
        "accounts",
        vec![("Name", set("v"))],
        KEY.to_vec(),
        true,
    ));
    cases.push((
        "binary key".into(),
        "public",
        "accounts",
        vec![("Name", set("v"))],
        vec![("payload", "0xDEADbeef")],
        true,
    ));
    for (case, schema, relation) in RELATIONS {
        cases.push((
            format!("relation {case}"),
            schema,
            relation,
            vec![("Name", set("v"))],
            id.clone(),
            true,
        ));
    }
    cases.push((
        "empty sets".into(),
        "public",
        "accounts",
        vec![],
        id.clone(),
        false,
    ));
    cases.push((
        "empty keys".into(),
        "public",
        "accounts",
        vec![("Name", set("v"))],
        vec![],
        false,
    ));
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, schema, table, sets, keys, expected) in &cases {
            let text = sql::update_row(engine, schema, table, sets, keys, &column_types_of(engine));
            assert_eq!(text.is_some(), *expected, "[{}] {case}", engine.as_str());
            statement(&mut out, engine, case, text.as_deref());
        }
    }
    check("rows_update", &out);
}

type Columns<'a> = Vec<(&'a str, Option<&'a str>)>;
type InsertCase<'a> = (String, &'a str, Columns<'a>, &'a [(String, String)], bool);
type Keys<'a> = Vec<(&'a str, &'a str)>;
type DeleteCase<'a> = (String, &'a str, &'a str, Keys<'a>, bool);

fn four_columns() -> Columns<'static> {
    vec![
        ("id", Some("42")),
        ("Name", Some("it's")),
        ("flag", Some("true")),
        ("created", Some("2024-01-02 03:04:05.123")),
    ]
}

#[test]
fn single_inserts_per_engine() {
    let mut out = String::new();
    let long = long_text();
    for engine in sql_engines() {
        let types = column_types_of(engine);
        let mut cases: Vec<(String, &str, &str, Columns, bool)> = vec![
            (
                "four columns".into(),
                "public",
                "accounts",
                four_columns(),
                true,
            ),
            (
                "every name".into(),
                "public",
                "accounts",
                NAMES.iter().map(|name| (*name, Some("v"))).collect(),
                true,
            ),
            (
                "null".into(),
                "public",
                "accounts",
                vec![("Name", None)],
                true,
            ),
            (
                "long".into(),
                "public",
                "accounts",
                vec![("Name", Some(long.as_str()))],
                true,
            ),
            ("no columns".into(), "public", "accounts", vec![], false),
        ];
        for (case, value) in VALUES {
            cases.push((
                format!("value {case}"),
                "public",
                "accounts",
                vec![("Name", Some(value))],
                true,
            ));
        }
        for (case, schema, relation) in RELATIONS {
            cases.push((
                format!("relation {case}"),
                schema,
                relation,
                vec![("Name", Some("v"))],
                true,
            ));
        }
        for (case, schema, table, columns, ok) in cases {
            let text = sql::insert_row(engine, schema, table, &columns, &types);
            outcome(&mut out, engine, &case, text, ok);
        }
    }

    let engine = Engine::MongoDb;
    let types = mongo_types();
    let untyped: Vec<(String, String)> = vec![];
    let mut cases: Vec<InsertCase> = vec![
        (
            "four columns".into(),
            "accounts",
            four_columns(),
            &types,
            true,
        ),
        (
            "null alone".into(),
            "accounts",
            vec![("Name", None)],
            &types,
            true,
        ),
        (
            "lone empty".into(),
            "accounts",
            vec![("Name", Some(""))],
            &types,
            false,
        ),
        (
            "only empty values".into(),
            "accounts",
            vec![("Name", Some("")), ("flag", Some(""))],
            &types,
            false,
        ),
        ("no columns".into(), "accounts", vec![], &types, false),
        (
            "operator field name".into(),
            "accounts",
            vec![("$set", Some("v"))],
            &untyped,
            false,
        ),
        (
            "operator value".into(),
            "accounts",
            vec![("x", Some("{\"$gt\": 1}"))],
            &untyped,
            false,
        ),
    ];
    for name in NAMES.iter().filter(|name| !name.is_empty()) {
        let value = match *name {
            "id" => "42",
            "_id" => "507f1f77bcf86cd799439011",
            _ => "v",
        };
        cases.push((
            format!("name {name}"),
            "accounts",
            vec![(name, Some(value))],
            &types,
            true,
        ));
    }
    for (case, value) in VALUES {
        cases.push((
            format!("value {case}"),
            "accounts",
            vec![("id", Some("1")), ("Name", Some(value))],
            &types,
            true,
        ));
    }
    for (case, _, relation) in RELATIONS {
        cases.push((
            format!("relation {case}"),
            relation,
            vec![("Name", Some("v"))],
            &types,
            true,
        ));
    }
    let valid = [
        ("id", "42"),
        ("flag", "true"),
        ("created", "2024-01-02T03:04:05.123Z"),
        ("big", "9007199254740993"),
        ("price", "19.99"),
        ("ref", "507f1f77bcf86cd799439011"),
        ("score", "1.5"),
        ("mixed", "42"),
        ("maybe", "7"),
    ];
    for (name, value) in valid {
        cases.push((
            format!("typed {name}"),
            "accounts",
            vec![(name, Some(value))],
            &types,
            true,
        ));
    }
    cases.push((
        "typed all".into(),
        "accounts",
        valid
            .iter()
            .map(|&(name, value)| (name, Some(value)))
            .collect(),
        &types,
        true,
    ));
    for (name, value) in [
        ("id", "abc"),
        ("flag", "maybe"),
        ("created", "yesterday"),
        ("big", "1.5"),
        ("price", "abc"),
        ("ref", "xyz"),
    ] {
        cases.push((
            format!("invalid {name}"),
            "accounts",
            vec![(name, Some(value))],
            &types,
            false,
        ));
    }
    for (case, table, columns, types, ok) in cases {
        let text = sql::insert_row(engine, "dbdelve_dev", table, &columns, types);
        outcome(&mut out, engine, &format!("mongo {case}"), text, ok);
    }

    let engine = Engine::Redis;
    let redis: [(&str, &str, &str, Columns, bool); 15] = [
        (
            "string, empty value",
            "db0",
            KEYS,
            vec![
                ("key", Some("a b")),
                ("value", Some("")),
                ("type", Some("")),
            ],
            true,
        ),
        (
            "string with ttl",
            "db0",
            KEYS,
            vec![
                ("key", Some("k")),
                ("value", Some("v")),
                ("ttl", Some("30")),
            ],
            true,
        ),
        (
            "hash",
            HASH,
            "h",
            vec![("field", Some("f")), ("value", Some("v"))],
            true,
        ),
        (
            "list",
            LIST,
            "l",
            vec![("index", Some("")), ("value", Some("v"))],
            true,
        ),
        ("set", SET, "s", vec![("member", Some("m"))], true),
        (
            "zset",
            ZSET,
            "z",
            vec![("member", Some("m")), ("score", Some("-inf"))],
            true,
        ),
        ("no value", "db0", KEYS, vec![("key", Some("k"))], false),
        (
            "null value",
            "db0",
            KEYS,
            vec![("key", Some("k")), ("value", None)],
            false,
        ),
        (
            "typed",
            "db0",
            KEYS,
            vec![
                ("key", Some("k")),
                ("value", Some("v")),
                ("type", Some("hash")),
            ],
            false,
        ),
        (
            "ttl zero",
            "db0",
            KEYS,
            vec![("key", Some("k")), ("value", Some("v")), ("ttl", Some("0"))],
            false,
        ),
        (
            "other table",
            "db0",
            "other",
            vec![("key", Some("k")), ("value", Some("v"))],
            false,
        ),
        (
            "list index",
            LIST,
            "l",
            vec![("index", Some("0")), ("value", Some("v"))],
            false,
        ),
        (
            "string family",
            STRING,
            "s",
            vec![("value", Some("v"))],
            false,
        ),
        (
            "zset no score",
            ZSET,
            "z",
            vec![("member", Some("m"))],
            false,
        ),
        (
            "two words",
            HASH,
            "two words",
            vec![("field", Some("f")), ("value", Some("v"))],
            false,
        ),
    ];
    for (case, schema, table, columns, ok) in redis {
        let text = sql::insert_row(engine, schema, table, &columns, &[]);
        outcome(&mut out, engine, &format!("redis {case}"), text, ok);
    }
    check("rows_insert", &out);
}

fn column(name: &str, default: Option<&str>) -> ColumnDefinition {
    ColumnDefinition {
        name: name.to_owned(),
        data_type: "text".to_owned(),
        nullable: true,
        default: default.map(str::to_owned),
    }
}

#[test]
fn insert_templates_per_engine() {
    let sets: [(&str, Vec<ColumnDefinition>); 11] = [
        (
            "ordinary",
            vec![
                column("id", Some("nextval('accounts_id_seq'::regclass)")),
                column("Name", None),
                column("total", Some("GENERATED ALWAYS AS (a + b) STORED")),
                column("seq", Some("AUTO_INCREMENT")),
                column("rid", Some("IDENTITY(1,1)")),
                column("it's", None),
            ],
        ),
        (
            "postgres generated",
            vec![
                column("a", None),
                column("b", Some("GENERATED ALWAYS AS (a * 2) STORED")),
                column("c", Some("GENERATED ALWAYS AS IDENTITY")),
            ],
        ),
        (
            "mysql generated",
            vec![
                column("a", None),
                column("b", Some("GENERATED ALWAYS AS (a * 2)")),
                column("c", Some("AUTO_INCREMENT")),
                column("d", Some("auto_increment")),
            ],
        ),
        (
            "sql server computed",
            vec![
                column("a", None),
                column("total", Some("AS ([a]+[b])")),
                column("flat", Some("AS (1)")),
                column("ident", Some("IDENTITY(1,1)")),
                column("dflt", Some("((0))")),
            ],
        ),
        (
            "look-alikes",
            vec![
                column("a", Some("ASCII")),
                column("b", Some("as ([x])")),
                column("c", Some("GENERATED ALWAYS AS(x)")),
                column("d", Some("IDENTITY")),
            ],
        ),
        (
            "all generated, sql server",
            vec![
                column("total", Some("AS ([a]+[b])")),
                column("ident", Some("IDENTITY(1,1)")),
                column("g", Some("GENERATED ALWAYS AS (1)")),
                column("s", Some("AUTO_INCREMENT")),
            ],
        ),
        (
            "all dropped, postgres",
            vec![
                column("b", Some("GENERATED ALWAYS AS (a * 2) STORED")),
                column("c", Some("GENERATED ALWAYS AS IDENTITY")),
            ],
        ),
        (
            "all dropped, mysql",
            vec![
                column("b", Some("GENERATED ALWAYS AS (a * 2)")),
                column("c", Some("AUTO_INCREMENT")),
            ],
        ),
        (
            "only a sql server computed column",
            vec![column("total", Some("AS ([a]+[b])"))],
        ),
        ("empty", vec![]),
        (
            "awkward names",
            ["we\"ird", "back`tick", "with space", "ümlaut", "[x]", ""]
                .map(|name| column(name, None))
                .to_vec(),
        ),
    ];
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, columns) in &sets {
            let template = sql::insert_template(engine, "public", "accounts", columns);
            fragment(&mut out, engine, case, template.as_deref());
        }
    }
    check("rows_template", &out);
}

fn matches_key(engine: Engine, text: &str, keys: &[(&str, &str)]) -> String {
    let names: Vec<&str> = keys.iter().map(|(name, _)| *name).collect();
    let mut superset = names.clone();
    superset.push("extra");
    let subset = &names[..names.len().saturating_sub(1)];
    let read = |list: &[&str]| sql::delete_matches_key_on(engine, text, list);
    format!(
        " matches_key=exact:{} superset:{} subset:{} other:{}",
        read(&names),
        read(&superset),
        read(subset),
        read(&["zzz"]),
    )
}

#[test]
fn deletes_per_engine() {
    let mut out = String::new();
    let record = |out: &mut String,
                  engine: Engine,
                  case: &str,
                  schema: &str,
                  table: &str,
                  keys: &[(&str, &str)],
                  types: &[(String, String)],
                  expected: bool| {
        let text = sql::delete_row(engine, schema, table, keys, types);
        assert_eq!(text.is_some(), expected, "[{}] {case}", engine.as_str());
        match &text {
            Some(text) => statement_with(
                out,
                engine,
                case,
                Some(text),
                &matches_key(engine, text, keys),
            ),
            None => statement(out, engine, case, None),
        }
    };
    for engine in sql_engines() {
        let types = column_types_of(engine);
        let mut cases: Vec<DeleteCase> = vec![
            ("id".into(), "public", "accounts", vec![("id", "7")], true),
            ("composite".into(), "public", "accounts", KEY.to_vec(), true),
            (
                "awkward column".into(),
                "public",
                "accounts",
                vec![("we\"ird", "1")],
                true,
            ),
            ("empty keys".into(), "public", "accounts", vec![], false),
        ];
        for (case, value) in [
            ("single quote", "it's"),
            ("backslash before quote", r"\'"),
            ("unicode", "李小龍 ünï 😀"),
        ] {
            cases.push((
                format!("value {case}"),
                "public",
                "accounts",
                vec![("Name", value)],
                true,
            ));
        }
        cases.push((
            "binary".into(),
            "public",
            "accounts",
            vec![("payload", "0xDEADbeef")],
            true,
        ));
        for (case, schema, relation) in RELATIONS {
            cases.push((
                format!("relation {case}"),
                schema,
                relation,
                vec![("id", "7")],
                true,
            ));
        }
        for (case, schema, table, keys, expected) in cases {
            record(
                &mut out, engine, &case, schema, table, &keys, &types, expected,
            );
        }
    }

    let engine = Engine::MongoDb;
    let types = mongo_types();
    let objectid = "65f0c0ffee0000000000abcd";
    let cases: [(&str, Keys, bool); 7] = [
        ("objectid", vec![("_id", objectid)], true),
        ("bad objectid", vec![("_id", "xyz")], false),
        ("int", vec![("id", "7")], true),
        ("bad int", vec![("id", "abc")], false),
        ("composite", vec![("_id", objectid), ("flag", "true")], true),
        ("quoted field", vec![("we\"ird", "1")], true),
        ("empty keys", vec![], false),
    ];
    for (case, keys, expected) in cases {
        record(
            &mut out,
            engine,
            &format!("mongo {case}"),
            "dbdelve_dev",
            "accounts",
            &keys,
            &types,
            expected,
        );
    }

    let engine = Engine::Redis;
    let types = vec![("key".to_owned(), STRING.to_owned())];
    record(
        &mut out,
        engine,
        "redis keys grid",
        "db0",
        KEYS,
        &[("key", "k")],
        &types,
        true,
    );
    record(
        &mut out,
        engine,
        "redis other table",
        "db0",
        "other",
        &[("key", "k")],
        &types,
        false,
    );
    check("rows_delete", &out);
}

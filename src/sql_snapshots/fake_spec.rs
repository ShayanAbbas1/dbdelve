use crate::{
    db::{
        Abilities, Engine, EngineSpec, Fields, Hints, Hooks, Paging, RelationKind, SqlSpec, Syntax,
    },
    explorer,
    filter::{self, Operator},
    sql,
};

const FAKE: EngineSpec = EngineSpec {
    id: "fake",
    label: "Fake",
    schemes: &[],
    file_extensions: &[],
    logo: "",
    brand: 0,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec::ANSI,
    abilities: Abilities::NONE,
    hooks: Hooks::NONE,
};

#[test]
fn a_fake_engine_spec_writes_sql_through_the_shared_strategies() {
    let preview = explorer::sql_preview(&FAKE, "s", "t", "", Some(100), 200);
    assert_eq!(preview, r#"SELECT * FROM "s"."t" LIMIT 100 OFFSET 200"#);
    assert_eq!(
        explorer::sql_count(&FAKE, "s", "t", r#""a" = 'x'"#),
        r#"SELECT COUNT(*) FROM "s"."t" WHERE "a" = 'x'"#
    );
    assert_eq!(
        sql::sql_paged(&FAKE, &preview, &["id".into()]).as_deref(),
        Some(preview.as_str())
    );

    const FETCH: EngineSpec = EngineSpec {
        sql: SqlSpec {
            paging: Paging::OffsetFetch,
            ..SqlSpec::ANSI
        },
        ..FAKE
    };
    assert_eq!(
        sql::sql_paged(&FETCH, &preview, &["id".into()]).as_deref(),
        Some(
            r#"SELECT * FROM "s"."t" ORDER BY (SELECT NULL), "id" OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"#
        )
    );

    let filter = filter::sql_predicate(&FAKE, "a", None, Operator::Contains, "50%").unwrap();
    assert_eq!(filter, r#""a" LIKE '%50\%%'"#);
    assert_eq!(
        filter::sql_predicate(&FAKE, "a", None, Operator::Regex, "x"),
        None
    );

    let insert =
        sql::sql_insert_row(&FAKE, "s", "t", &[("a", Some("it's")), ("b", None)], &[]).unwrap();
    assert_eq!(
        insert,
        r#"INSERT INTO "s"."t" ("a", "b") VALUES ('it''s', NULL)"#
    );
    assert!(sql::generated_write(&insert, FAKE.sql.backslash_escapes));
    // Only the gate's tree-sitter arm runs for a SQL engine, and it reads no spec field.
    assert!(sql::is_generated_select(
        Engine::Postgres,
        &explorer::sql_preview(&FAKE, "s", "t", &filter, Some(100), 0)
    ));
}

#[test]
fn an_engine_in_the_read_only_tier_edits_and_inserts_nothing() {
    assert!(!FAKE.writes_back(["text"]));
    assert!(!FAKE.writes_back([]), "a cell no driver typed");
    for kind in [RelationKind::Table, RelationKind::View] {
        assert!(!FAKE.takes_inserts(kind), "{kind:?}");
    }

    const WRITES: EngineSpec = EngineSpec {
        abilities: Abilities {
            edits: true,
            inserts: true,
            ..Abilities::NONE
        },
        ..FAKE
    };
    assert!(WRITES.writes_back(["text"]) && WRITES.writes_back([]));
    assert!(!WRITES.writes_back(["text", "bytea"]));
    assert!(WRITES.takes_inserts(RelationKind::Table));
    assert!(!WRITES.takes_inserts(RelationKind::View));
}

#[test]
fn engine_spec_is_static_data() {
    for engine in Engine::ALL {
        let first: &'static EngineSpec = engine.spec();
        assert!(std::ptr::eq(first, engine.spec()));
    }
}

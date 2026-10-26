use crate::{
    db::{
        Abilities, EditTarget, Engine, EngineSpec, Fields, Hints, Hooks, IdentifierQuote, Paging,
        ParserDialect, ReadOnlySession, RelationKind, RowMatch, SqlSpec, Syntax, UnreadableFloor,
        UpdateSpelling,
    },
    explorer,
    filter::{self, Operator},
    result_grid::{NewValue, PendingRow},
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
    numeric_types: &[],
    type_wrappers: &[],
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
    assert!(sql::generated_write(&FAKE.sql, &insert));
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
fn identifier_backslashes_are_doubled_only_where_the_spec_says() {
    const BACKSLASHED: EngineSpec = EngineSpec {
        sql: SqlSpec {
            identifier_quote: IdentifierQuote::Backtick,
            identifier_backslash_escapes: true,
            ..SqlSpec::ANSI
        },
        ..FAKE
    };
    assert_eq!(BACKSLASHED.quote_identifier(r"a\b"), r"`a\\b`");
    assert_eq!(BACKSLASHED.quote_identifier("a`b"), "`a``b`");
    for name in [r"a\b", r"\", r"a\`b", r"`\", "plain", ""] {
        assert_eq!(
            BACKSLASHED.unquote_identifier(&BACKSLASHED.quote_identifier(name)),
            name
        );
    }
    assert_eq!(FAKE.quote_identifier(r"a\b"), r#""a\b""#);
    assert_eq!(FAKE.unquote_identifier(r#""a\\b""#), r"a\\b");
}

#[test]
fn a_per_request_hold_is_a_hold_with_no_session_statement() {
    const PER_REQUEST: EngineSpec = EngineSpec {
        sql: SqlSpec {
            read_only_session: ReadOnlySession::PerRequest,
            unreadable_floor: UnreadableFloor::FirstKeyword,
            parser_dialect: ParserDialect::ClickHouse,
            ..SqlSpec::ANSI
        },
        ..FAKE
    };
    assert_ne!(PER_REQUEST.sql.read_only_session, ReadOnlySession::None);
    assert_eq!(FAKE.sql.unreadable_floor, UnreadableFloor::None);
}

#[test]
fn engine_spec_is_static_data() {
    for engine in Engine::ALL {
        let first: &'static EngineSpec = engine.spec();
        assert!(std::ptr::eq(first, engine.spec()));
    }
}

fn matched(column: &str, value: Option<&str>, data_type: &str) -> Option<String> {
    let column = FAKE.quote_identifier(column);
    match (value, data_type) {
        (_, "opaque") => None,
        (None, _) => Some(format!("{column} IS NULL")),
        (Some(value), "float") => Some(format!("toString({column}) = '{value}'")),
        (Some(value), _) => Some(format!("{column} = '{value}'")),
    }
}

const EVERY_VALUE: EngineSpec = EngineSpec {
    hooks: Hooks {
        match_value: Some(matched),
        ..Hooks::NONE
    },
    sql: SqlSpec {
        transactions: false,
        row_terms: true,
        ..SqlSpec::ANSI
    },
    ..FAKE
};

fn pending_row(update: UpdateSpelling) -> PendingRow {
    PendingRow {
        schema: "s".into(),
        table: "t".into(),
        sets: vec![("a".into(), NewValue::Value("new".into()))],
        keys: vec![("k".into(), "1".into()), ("f".into(), "1.5".into())],
        null_keys: vec!["n".into()],
        update,
        types: vec![("f".into(), "float".into())],
        held: Vec::new(),
    }
}

#[test]
fn a_row_named_by_every_value_is_matched_through_the_hook_and_is_null() {
    let row = pending_row(UpdateSpelling::Update);
    assert_eq!(
        sql::sql_update_batch(&EVERY_VALUE, &[row]).unwrap(),
        r#"UPDATE "s"."t" SET "a" = 'new' WHERE "k" = '1' AND toString("f") = '1.5' AND "n" IS NULL;"#
    );
    let keys = [("k", "1")];
    assert_eq!(
        sql::sql_delete_row(
            &EVERY_VALUE,
            "s",
            "t",
            sql::Match {
                keys: &keys,
                nulls: &["n"]
            },
            &[]
        )
        .as_deref(),
        Some(r#"DELETE FROM "s"."t" WHERE "k" = '1' AND "n" IS NULL"#)
    );
}

#[test]
fn a_value_the_hook_cannot_spell_leaves_the_row_unwritten() {
    let types = [("k".to_string(), "opaque".to_string())];
    let keys = [("k", "1")];
    assert_eq!(
        sql::sql_delete_row(
            &EVERY_VALUE,
            "s",
            "t",
            sql::Match {
                keys: &keys,
                nulls: &[]
            },
            &types
        ),
        None
    );
    assert_eq!(
        sql::sql_delete_row(
            &EVERY_VALUE,
            "s",
            "t",
            sql::Match {
                keys: &[],
                nulls: &[]
            },
            &[]
        ),
        None
    );
}

#[test]
fn without_the_hook_a_null_key_is_still_matched_with_is_null() {
    let keys = [("k", "1")];
    assert_eq!(
        sql::sql_delete_row(
            &FAKE,
            "s",
            "t",
            sql::Match {
                keys: &keys,
                nulls: &["n"]
            },
            &[]
        )
        .as_deref(),
        Some(r#"DELETE FROM "s"."t" WHERE "k" = '1' AND "n" IS NULL"#)
    );
}

#[test]
fn an_alter_update_batch_waits_for_every_replica() {
    let row = pending_row(UpdateSpelling::AlterUpdate);
    assert_eq!(
        sql::sql_update_batch(&EVERY_VALUE, &[row]).unwrap(),
        r#"ALTER TABLE "s"."t" UPDATE "a" = 'new' WHERE "k" = '1' AND toString("f") = '1.5' AND "n" IS NULL SETTINGS mutations_sync = 2;"#
    );
}

#[test]
fn a_spec_without_transactions_brackets_no_batch() {
    let rows = [
        pending_row(UpdateSpelling::Update),
        pending_row(UpdateSpelling::Update),
    ];
    assert_eq!(EVERY_VALUE.sql.transaction_opening(), None);
    assert!(
        !sql::sql_update_batch(&EVERY_VALUE, &rows)
            .unwrap()
            .contains("BEGIN")
    );
    assert!(
        sql::sql_update_batch(&FAKE, &rows)
            .unwrap()
            .starts_with("BEGIN;")
    );
}

#[test]
fn an_edit_target_stored_before_row_match_reads_as_a_key() {
    let old = r#"{"schema":"s","table":"t","columns":["id",null],"keys":[0]}"#;
    let target: EditTarget = serde_json::from_str(old).unwrap();
    assert_eq!(target.row_match, RowMatch::Key);
    assert_eq!(target.update, UpdateSpelling::Update);
    let every = EditTarget {
        row_match: RowMatch::EveryColumn { fixed: vec![1] },
        update: UpdateSpelling::AlterUpdate,
        ..target
    };
    let back: EditTarget = serde_json::from_str(&serde_json::to_string(&every).unwrap()).unwrap();
    assert_eq!(back, every);
}

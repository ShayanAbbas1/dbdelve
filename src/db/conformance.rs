//! One set of scenarios every engine's live tests run unchanged, against the
//! small `conformance` fixture each dev database is seeded with.
//!
//! An engine's `tests` module builds a [`Subject`] (an open connection plus
//! the per-engine data a scenario cannot derive) and lists the scenarios it
//! takes in [`suite!`]. Engines are asked only through `Engine` and
//! `db::Connection`, so a scenario that fails is a finding about the engine,
//! not something to bend the scenario around.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::{
    CancelToken, Connection, Engine, Fetch, QueryResult, RelationKind, Stopped, is_cancel,
};
use crate::result_grid::{NewValue, PendingRow};
use crate::sql;

pub(super) struct Subject {
    pub connection: Connection,
    pub engine: Engine,
    pub schema: &'static str,
    pub relation: &'static str,
    pub key: &'static str,
    pub columns: &'static [&'static str],
    pub read: String,
    pub endless: &'static str,
    pub runaway: &'static str,
    pub read_only_refusal: &'static str,
    pub fixture: Option<Fixture>,
    /// Kept alive until the scenario ends, and dropped after `connection`.
    pub _guard: Option<Box<dyn std::any::Any>>,
}

pub(super) struct Fixture {
    pub types: &'static [&'static str],
    pub renders: &'static [(&'static str, usize, &'static str)],
    pub find: Box<dyn Fn(&str) -> String>,
}

/// Puts the fixture back when a scenario ends, panicking or not: whatever
/// transaction it left open is rolled back, the session is made writable
/// again and the ids it wrote are deleted. Errors are ignored, since the
/// scenario's own failure is the one worth reading.
struct Cleanup<'a> {
    connection: &'a Connection,
    statements: Vec<String>,
    rollback: bool,
    read_write: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let run = |sql: &str| {
            let _ = self
                .connection
                .query(sql, &CancelToken::default(), Fetch::default());
        };
        if self.rollback {
            run("ROLLBACK");
        }
        if self.read_write {
            let _ = self.connection.set_read_only(false);
        }
        for statement in &self.statements {
            run(statement);
        }
    }
}

pub(super) const COLUMNS: [&str; 9] = [
    "id", "label", "whole", "fraction", "ratio", "flag", "day", "moment", "bytes",
];

pub(super) const EXPECTED: [[Option<&str>; 9]; 3] = [
    [
        Some("1"),
        Some("alpha"),
        Some("9007199254740993"),
        Some("12.50"),
        Some("0.5"),
        Some("1"),
        Some("2024-02-29"),
        Some("2024-02-29 12:34:56"),
        Some("0x00FF"),
    ],
    [
        Some("2"),
        Some("Zoë 李 🐉 'quoted'"),
        Some("-42"),
        Some("-0.01"),
        Some("-1.25"),
        Some("0"),
        Some("1999-12-31"),
        Some("1999-12-31 23:59:59"),
        Some("0xDEADBEEF"),
    ],
    [
        Some("3"),
        Some("nulls"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ],
];

const WITHIN: Duration = Duration::from_secs(10);

pub(super) fn sql_read(engine: Engine, schema: &str) -> String {
    let columns: Vec<String> = COLUMNS
        .iter()
        .map(|column| engine.quote_identifier(column))
        .collect();
    format!(
        "SELECT {} FROM {} WHERE {} < 900 ORDER BY {}",
        columns.join(", "),
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
        engine.quote_identifier("id"),
    )
}

pub(super) fn sql_find(engine: Engine, schema: &str, key: &str) -> String {
    format!(
        "SELECT {} FROM {} WHERE {} = {key}",
        engine.quote_identifier("label"),
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
    )
}

fn sql_insert(engine: Engine, schema: &str, id: u32, label: &str) -> String {
    format!(
        "INSERT INTO {} ({}, {}) VALUES ({id}, {})",
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
        engine.quote_identifier("label"),
        engine.quote_literal(label),
    )
}

fn sql_delete(engine: Engine, schema: &str, id: u32) -> String {
    format!(
        "DELETE FROM {} WHERE {} = {id}",
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
    )
}

macro_rules! suite {
    ($module:ident, $why:literal, $subject:ident: $($scenario:ident),+ $(,)?) => {
        mod $module {
            $(
                #[test]
                #[ignore = $why]
                fn $scenario() {
                    crate::db::conformance::$scenario(&super::$subject());
                }
            )+
        }
    };
}
pub(super) use suite;

impl Subject {
    fn run(&self, scenario: &str, sql: &str, fetch: Fetch) -> QueryResult {
        self.connection
            .query(sql, &CancelToken::default(), fetch)
            .unwrap_or_else(|error| {
                panic!("{scenario} on {:?}: `{sql}` failed: {error}", self.engine)
            })
    }

    fn read(&self, scenario: &str) -> QueryResult {
        self.run(scenario, &self.read, Fetch::default())
    }

    fn fixture(&self, scenario: &str) -> &Fixture {
        self.fixture
            .as_ref()
            .unwrap_or_else(|| panic!("{scenario} on {:?} needs a fixture", self.engine))
    }

    fn label_of(&self, scenario: &str, key: &str) -> Vec<Option<String>> {
        let find = (self.fixture(scenario).find)(key);
        self.run(scenario, &find, Fetch::default())
            .rows
            .iter()
            .map(|row| row.get(0).map(str::to_string))
            .collect()
    }
}

fn names(result: &QueryResult) -> Vec<&str> {
    result.columns.iter().map(|c| c.name.as_str()).collect()
}

pub(super) fn connects(subject: &Subject) {
    assert!(
        !subject.connection.is_lost(),
        "connects on {:?}: the session is lost",
        subject.engine
    );
    let result = subject.read("connects");
    assert_eq!(
        names(&result),
        subject.columns,
        "connects on {:?}",
        subject.engine
    );
}

pub(super) fn query_round_trip(subject: &Subject) {
    let at = format!("query_round_trip on {:?}", subject.engine);
    let result = subject.read("query_round_trip");
    assert_eq!(names(&result), subject.columns, "{at}");
    assert_eq!(result.rows.len(), 3, "{at}");
    for row in &result.rows[..2] {
        assert!(row.iter().all(|cell| cell.is_some()), "{at}: {row:?}");
    }
    let nulls = &result.rows[2];
    assert!(nulls.get(0).is_some() && nulls.get(1).is_some(), "{at}");
    assert!((2..9).all(|col| nulls.get(col).is_none()), "{at}");
}

pub(super) fn catalog_round_trip(subject: &Subject) {
    let at = format!("catalog_round_trip on {:?}", subject.engine);
    let catalog = subject
        .connection
        .catalog()
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let schema = catalog
        .schemas
        .iter()
        .find(|schema| schema.name == subject.schema)
        .unwrap_or_else(|| panic!("{at}: no schema {}", subject.schema));
    let relation = schema
        .relations
        .iter()
        .find(|relation| relation.name == subject.relation)
        .unwrap_or_else(|| panic!("{at}: no relation {}", subject.relation));
    assert_eq!(relation.kind, RelationKind::Table, "{at}");
    if subject.engine.copies_ddl() {
        let ddl = subject
            .connection
            .ddl(subject.schema, subject.relation, RelationKind::Table)
            .unwrap_or_else(|error| panic!("{at}: ddl: {error}"));
        assert!(ddl.contains(subject.relation), "{at}: {ddl}");
    }
}

pub(super) fn structure_round_trip(subject: &Subject) {
    let at = format!("structure_round_trip on {:?}", subject.engine);
    let structure = subject
        .connection
        .structure(subject.schema, subject.relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let columns: Vec<&str> = structure.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, subject.columns, "{at}");
    let key = structure
        .columns
        .iter()
        .find(|column| column.name == subject.key)
        .unwrap_or_else(|| panic!("{at}: no key column"));
    assert!(!key.nullable, "{at}: the key column is nullable");
    assert_eq!(structure.primary_key(), [subject.key], "{at}");
}

pub(super) fn types_render_as_expected(subject: &Subject) {
    let at = format!("types_render_as_expected on {:?}", subject.engine);
    let fixture = subject.fixture("types_render_as_expected");
    let result = subject.read("types_render_as_expected");
    let types: Vec<Option<&str>> = result
        .columns
        .iter()
        .map(|column| column.data_type.as_deref())
        .collect();
    let wanted: Vec<Option<&str>> = fixture.types.iter().map(|t| Some(*t)).collect();
    assert_eq!(types, wanted, "{at}: column types");
    assert_eq!(result.rows.len(), 3, "{at}");
    for (row, expected) in EXPECTED.iter().enumerate() {
        for (col, base) in expected.iter().enumerate() {
            let name = subject.columns[col];
            let over = fixture
                .renders
                .iter()
                .find(|(column, at_row, _)| *column == name && *at_row == row)
                .map(|(_, _, text)| *text);
            assert_eq!(
                result.rows[row].get(col),
                over.or(*base),
                "{at}: row {row}, column {name}"
            );
        }
    }
}

pub(super) fn cancel_stops_a_long_query(subject: &Subject) {
    let at = format!("cancel_stops_a_long_query on {:?}", subject.engine);
    let token = CancelToken::default();
    let (sender, receiver) = mpsc::channel();
    let connection = subject.connection.clone();
    let (sql, run_token) = (subject.runaway, token.clone());
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = sender.send(connection.query(sql, &run_token, Fetch::default()));
    });
    let outcome = loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(outcome) => break outcome,
            Err(_) if started.elapsed() >= WITHIN => panic!("{at}: still running after {WITHIN:?}"),
            Err(_) => {
                let _ = subject.connection.cancel(&token);
            }
        }
    };
    let error = outcome.expect_err(&format!("{at}: the run should have been cancelled"));
    assert!(is_cancel(subject.engine, &error), "{at}: {}", error.message);
    assert!(started.elapsed() < WITHIN, "{at}");
    subject.read("cancel_stops_a_long_query");
}

pub(super) fn row_limit_stops_an_eligible_read(subject: &Subject) {
    let at = format!("row_limit_stops_an_eligible_read on {:?}", subject.engine);
    assert!(sql::stoppable(subject.engine, subject.endless), "{at}");
    let started = Instant::now();
    let result = subject.run(
        "row_limit_stops_an_eligible_read",
        subject.endless,
        Fetch {
            limit: Some(3),
            reads_only: true,
            ..Fetch::default()
        },
    );
    assert_eq!(result.stopped, Some(Stopped::AtLimit), "{at}");
    assert_eq!(result.capped_from, None, "{at}");
    assert_eq!(result.rows.len(), 3, "{at}");
    assert!(started.elapsed() < WITHIN, "{at}");
}

pub(super) fn row_limit_drains_an_ineligible_read(subject: &Subject) {
    let at = format!(
        "row_limit_drains_an_ineligible_read on {:?}",
        subject.engine
    );
    let full = subject.read("row_limit_drains_an_ineligible_read");
    let capped = subject.run(
        "row_limit_drains_an_ineligible_read",
        &subject.read,
        Fetch {
            limit: Some(2),
            reads_only: false,
            ..Fetch::default()
        },
    );
    assert_eq!(capped.stopped, None, "{at}");
    assert_eq!(capped.capped_from, Some(3), "{at}");
    assert_eq!(capped.rows, full.rows[..2], "{at}");
    assert_eq!(capped.columns, full.columns, "{at}");
}

pub(super) fn capped_read_keeps_the_users_transaction(subject: &Subject) {
    let scenario = "capped_read_keeps_the_users_transaction";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let delete = sql_delete(engine, subject.schema, 950);
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![delete.clone()],
        rollback: true,
        read_write: false,
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    let begin = engine.transaction_start().unwrap_or("BEGIN");
    let capped_read_in_a_transaction = || {
        run(&delete);
        run(begin);
        run(&sql_insert(engine, subject.schema, 950, "in transaction"));
        let capped = subject.run(
            scenario,
            &subject.read,
            Fetch {
                limit: Some(2),
                reads_only: sql::stoppable(engine, &subject.read),
                ..Fetch::default()
            },
        );
        assert_eq!(capped.rows.len(), 2, "{at}");
        assert!(
            (capped.stopped == Some(Stopped::AtLimit) && capped.capped_from.is_none())
                || (capped.stopped.is_none() && capped.capped_from == Some(3)),
            "{at}: stopped {:?}, capped_from {:?}",
            capped.stopped,
            capped.capped_from
        );
        assert_eq!(
            subject.label_of(scenario, "950"),
            [Some("in transaction".to_string())],
            "{at}: the open transaction lost its own write"
        );
    };
    let seen_apart = || {
        let apart = subject
            .connection
            .apart()
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        let find = (subject.fixture(scenario).find)("950");
        let seen = apart
            .query(&find, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        seen.rows
            .iter()
            .map(|row| row.get(0).map(str::to_string))
            .collect::<Vec<_>>()
    };

    // A read that had quietly committed would leave nothing for ROLLBACK to
    // undo, so the row surviving it is the proof the transaction was cut short.
    capped_read_in_a_transaction();
    run("ROLLBACK");
    assert!(
        subject.label_of(scenario, "950").is_empty(),
        "{at}: ROLLBACK left the row, so the capped read ended the transaction"
    );
    assert!(
        seen_apart().is_empty(),
        "{at}: another session sees the row"
    );

    capped_read_in_a_transaction();
    run("COMMIT");
    assert_eq!(
        seen_apart(),
        [Some("in transaction".to_string())],
        "{at}: a fresh session does not see the committed row"
    );
}

pub(super) fn read_only_session_refuses_a_write(subject: &Subject) {
    let scenario = "read_only_session_refuses_a_write";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    if !engine.holds_read_only() {
        subject
            .connection
            .set_read_only(true)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        subject
            .connection
            .set_read_only(false)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        return;
    }
    let delete = sql_delete(engine, subject.schema, 960);
    let insert = sql_insert(engine, subject.schema, 960, "read-only");
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![delete.clone()],
        rollback: false,
        read_write: true,
    };
    subject.run(scenario, &delete, Fetch::default());
    subject
        .connection
        .set_read_only(true)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let refused = subject
        .connection
        .query(&insert, &CancelToken::default(), Fetch::default());
    subject
        .connection
        .set_read_only(false)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let error = refused.expect_err(&format!("{at}: a read-only session took a write"));
    assert!(
        error.message.contains(subject.read_only_refusal),
        "{at}: refused for another reason: {}",
        error.message
    );
    subject.run(scenario, &insert, Fetch::default());
}

pub(super) fn generated_writes_round_trip(subject: &Subject) {
    let scenario = "generated_writes_round_trip";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let structure = subject
        .connection
        .structure(schema, relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let types: Vec<(String, String)> = structure
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.data_type.clone()))
        .collect();
    let generated = |sql: &str| {
        subject
            .connection
            .generated(sql, &CancelToken::default(), None, None)
            .unwrap_or_else(|error| panic!("{at}: `{sql}` failed: {error}"))
    };

    let cleanup = sql::delete_row(engine, schema, relation, &[(key, "901")], &types);
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: cleanup.iter().cloned().collect(),
        rollback: false,
        read_write: false,
    };
    if let Some(delete) = &cleanup {
        generated(delete);
    }

    if engine.takes_inserts(RelationKind::Table) {
        let insert = sql::insert_row(
            engine,
            schema,
            relation,
            &[(key, Some("901")), ("label", Some("inserted"))],
            &types,
        )
        .unwrap_or_else(|error| panic!("{at}: insert_row: {error}"));
        assert!(
            sql::is_generated_write_on(engine, &insert),
            "{at}: {insert}"
        );
        generated(&insert);
        assert_eq!(
            subject.label_of(scenario, "901"),
            [Some("inserted".to_string())],
            "{at}"
        );
    }

    let label_type = types
        .iter()
        .find(|(name, _)| name == "label")
        .map(|(_, data_type)| data_type.as_str())
        .unwrap_or_else(|| panic!("{at}: no label column"));
    if engine.writes_back([label_type]) && !structure.primary_key().is_empty() {
        let row = PendingRow {
            schema: schema.to_string(),
            table: relation.to_string(),
            sets: vec![("label".into(), NewValue::Value("updated".into()))],
            keys: vec![(key.to_string(), "901".to_string())],
            types: types.clone(),
            held: vec![("label".into(), "inserted".into())],
        };
        let update = sql::update_batch(engine, &[row])
            .unwrap_or_else(|error| panic!("{at}: update_batch: {error}"));
        assert!(
            sql::is_generated_write_on(engine, &update),
            "{at}: {update}"
        );
        generated(&update);
        assert_eq!(
            subject.label_of(scenario, "901"),
            [Some("updated".to_string())],
            "{at}"
        );
    }

    let delete = sql::delete_row(engine, schema, relation, &[(key, "901")], &types)
        .unwrap_or_else(|| panic!("{at}: no delete for the key"));
    assert!(
        sql::is_generated_write_on(engine, &delete),
        "{at}: {delete}"
    );
    assert!(
        sql::delete_matches_key_on(engine, &delete, &[key]),
        "{at}: {delete}"
    );
    generated(&delete);
    assert!(subject.label_of(scenario, "901").is_empty(), "{at}");
}

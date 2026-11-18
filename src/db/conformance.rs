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
    ApplyOutcome, CancelToken, Connection, Drift, Driver, Engine, Fetch, FetchedCell, QueryResult,
    RelationKind, StepKind, Stopped, Target, VerifiedStep, is_cancel,
};
use crate::explorer;
use crate::filter::{self, Operator};
use crate::result_grid::{BinaryForm, NewValue, PendingRow, ResultGrid};
use crate::sql::{self, SortKey};

pub(super) struct Subject {
    pub connection: Connection,
    pub target: Target,
    pub schema: &'static str,
    pub relation: &'static str,
    pub key: &'static str,
    pub columns: &'static [&'static str],
    pub read: String,
    /// What `endless` counts to, on a subject that drains it instead of
    /// stopping at the limit.
    pub endless_rows: Option<usize>,
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
                panic!(
                    "{scenario} on {:?}: `{sql}` failed: {error}",
                    self.target.engine
                )
            })
    }

    fn read(&self, scenario: &str) -> QueryResult {
        self.run(scenario, &self.read, Fetch::default())
    }

    fn fixture(&self, scenario: &str) -> &Fixture {
        self.fixture
            .as_ref()
            .unwrap_or_else(|| panic!("{scenario} on {:?} needs a fixture", self.target.engine))
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
        subject.target.engine
    );
    let result = subject.read("connects");
    assert_eq!(
        names(&result),
        subject.columns,
        "connects on {:?}",
        subject.target.engine
    );
}

pub(super) fn query_round_trip(subject: &Subject) {
    let at = format!("query_round_trip on {:?}", subject.target.engine);
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
    let at = format!("catalog_round_trip on {:?}", subject.target.engine);
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
    if subject.target.copies_ddl() {
        let ddl = subject
            .connection
            .ddl(subject.schema, subject.relation, RelationKind::Table)
            .unwrap_or_else(|error| panic!("{at}: ddl: {error}"));
        assert!(ddl.contains(subject.relation), "{at}: {ddl}");
    }
}

pub(super) fn structure_round_trip(subject: &Subject) {
    let at = format!("structure_round_trip on {:?}", subject.target.engine);
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
    let at = format!("types_render_as_expected on {:?}", subject.target.engine);
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
    let at = format!("cancel_stops_a_long_query on {:?}", subject.target.engine);
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
    assert!(is_cancel(subject.target, &error), "{at}: {}", error.message);
    assert!(started.elapsed() < WITHIN, "{at}");
    subject.read("cancel_stops_a_long_query");
}

pub(super) fn row_limit_stops_an_eligible_read(subject: &Subject) {
    let at = format!(
        "row_limit_stops_an_eligible_read on {:?}",
        subject.target.engine
    );
    assert!(
        sql::stoppable(subject.target.engine, subject.endless),
        "{at}"
    );
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
    assert_eq!(result.rows.len(), 3, "{at}");
    if subject.connection.stops_at_limit() {
        assert_eq!(result.stopped, Some(Stopped::AtLimit), "{at}");
        assert_eq!(result.capped_from, None, "{at}");
        assert!(started.elapsed() < WITHIN, "{at}");
    } else {
        // A driver that cannot prove ending early keeps the user's
        // transaction drains the statement to its end instead, so the
        // subject's `endless` is finite there and counted whole.
        assert_eq!(result.stopped, None, "{at}");
        assert!(subject.endless_rows.is_some(), "{at}: no count to drain to");
        assert_eq!(result.capped_from, subject.endless_rows, "{at}");
    }
}

pub(super) fn row_limit_drains_an_ineligible_read(subject: &Subject) {
    let at = format!(
        "row_limit_drains_an_ineligible_read on {:?}",
        subject.target.engine
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
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
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
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
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
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let label = subject.columns[1];
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

    // A plugin's driver says nothing of where a column came from, so its rows
    // are editable only by DBDelve's own reading of the preview and the
    // relation's structure, which a built-in's driver reports itself.
    if let Driver::Plugin(_) = subject.target.driver {
        let preview = generated(&explorer::preview_sql(
            engine,
            schema,
            relation,
            "",
            Some(10),
            0,
        ));
        let edit = preview.edit.unwrap_or_else(|| {
            panic!(
                "{at}: the preview is read-only: {:?}",
                preview.read_only_reason
            )
        });
        assert_eq!(
            (edit.schema.as_str(), edit.table.as_str()),
            (schema, relation),
            "{at}"
        );
        let keys: Vec<_> = edit
            .keys
            .iter()
            .map(|&at| edit.columns[at].as_deref())
            .collect();
        assert_eq!(keys, [Some(key)], "{at}");
    }

    let cleanup = sql::delete_row(engine, schema, relation, &[(key, "901")], &types);
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: cleanup.iter().cloned().collect(),
        rollback: false,
        read_write: false,
    };
    // An engine that names rows by every value refuses a delete that matches none.
    if let Some(delete) = cleanup
        .as_ref()
        .filter(|_| !subject.label_of(scenario, "901").is_empty())
    {
        generated(delete);
    }

    if subject.target.takes_inserts(RelationKind::Table) {
        let insert = sql::insert_row(
            engine,
            schema,
            relation,
            &[(key, Some("901")), (label, Some("inserted"))],
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
        .find(|(name, _)| name == label)
        .map(|(_, data_type)| data_type.as_str())
        .unwrap_or_else(|| panic!("{at}: no label column"));
    if subject.target.edits_rows()
        && engine.writes_back([label_type])
        && !structure.primary_key().is_empty()
    {
        let row = PendingRow {
            schema: schema.to_string(),
            table: relation.to_string(),
            sets: vec![(label.into(), NewValue::Value("updated".into()))],
            keys: vec![(key.to_string(), "901".to_string())],
            types: types.clone(),
            held: vec![(label.into(), "inserted".into())],
            // The spelling is the preview's to say, as on an object tab.
            update: if engine == Engine::ClickHouse {
                generated(&format!(
                    "SELECT * FROM {}",
                    engine.qualified(schema, relation)
                ))
                .edit
                .map(|target| target.update)
                .unwrap_or_default()
            } else {
                Default::default()
            },
            ..Default::default()
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

/// Writes bytes into a throwaway table the way a grid edit does: read the
/// row, stage text in a binary column parsed as a form, generate the update,
/// pass it through the gate and run it, then read the bytes back exactly.
pub(super) fn binary_edits_round_trip(
    connection: &Connection,
    target: Target,
    schema: &str,
    binary_type: &str,
) {
    let engine = target.engine;
    let at = format!("binary_edits_round_trip on {engine:?}");
    let table = engine.qualified(schema, "dbdelve_binary_edits");
    let run = |sql: &str| {
        connection
            .query(sql, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{at}: `{sql}` failed: {error}"))
    };
    let drop = format!("DROP TABLE IF EXISTS {table}");
    run(&drop);
    let _cleanup = Cleanup {
        connection,
        statements: vec![drop],
        rollback: false,
        read_write: false,
    };
    run(&format!(
        "CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY, payload {binary_type})"
    ));
    run(&format!(
        "INSERT INTO {table} (id, payload) VALUES (1, NULL)"
    ));
    let select = format!("SELECT id, payload FROM {table}");

    let grid = |form: BinaryForm| {
        let mut grid = ResultGrid::new(run(&select), sql::Mode::ReadWrite).with_target(target);
        grid.set_binary_form(1, form);
        grid
    };
    let stored = || {
        let result = run(&select);
        let value = result.rows.first().and_then(|row| row.get(1));
        let value = value.unwrap_or_else(|| panic!("{at}: the payload is NULL"));
        engine
            .bytes_of(value, usize::MAX)
            .unwrap_or_else(|| panic!("{at}: `{value}` reads as no bytes"))
    };

    let ff00 = engine
        .bytes_text(&[0xff, 0])
        .expect("the engine spells bytes");
    for (form, text, bytes) in [
        (BinaryForm::Text, "hello", b"hello".to_vec()),
        (BinaryForm::Hex, ff00.as_str(), vec![0xff, 0]),
        (BinaryForm::Base64, "3q2+7w==", vec![0xde, 0xad, 0xbe, 0xef]),
        (BinaryForm::Hex, "", Vec::new()),
    ] {
        let mut grid = grid(form);
        assert!(grid.editable(0, 1), "{at}: {form:?} is read-only");
        let value = NewValue::Value(text.into());
        let Some(literal) = engine.binary_literal(&bytes) else {
            assert!(
                grid.staging_refusal(0, 1, &value).is_some(),
                "{at}: {bytes:?} has no literal and was staged"
            );
            continue;
        };
        assert!(grid.set_pending(0, 1, value), "{at}: `{text}` unstaged");
        let update = sql::update_batch(engine, &grid.pending_updates())
            .unwrap_or_else(|error| panic!("{at}: update_batch: {error}"));
        assert!(update.contains(&literal), "{at}: {update}");
        assert!(
            sql::is_generated_write_on(engine, &update),
            "{at}: {update}"
        );
        connection
            .generated(&update, &CancelToken::default(), None, None)
            .unwrap_or_else(|error| panic!("{at}: `{update}` failed: {error}"));
        assert_eq!(stored(), bytes, "{at}: {update}");
    }

    run(&format!(
        "UPDATE {table} SET payload = {}",
        engine.binary_literal(&[0xff, 0]).unwrap()
    ));
    let text = grid(BinaryForm::Text);
    assert!(
        !text.editable(0, 1),
        "{at}: non-UTF-8 bytes are editable as text"
    );
    assert!(text.bytes_refusal(0, 1).is_some(), "{at}");
    assert!(grid(BinaryForm::Hex).editable(0, 1), "{at}");
}

/// A mixed draft, written by `sql::draft_batch` and gated as the apply gates
/// it, run on the subject as one transaction: the deleted key is free for an
/// insert in the same batch, and a batch that fails part-way leaves what its
/// `DELETE` named in place.
pub(super) fn generated_draft_round_trip(subject: &Subject) {
    let scenario = "generated_draft_round_trip";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    if !subject.target.row_edits() {
        return;
    }
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let label = subject.columns[1];
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
    };
    let delete = |id: &str| sql::DraftDelete {
        schema: schema.into(),
        table: relation.into(),
        keys: vec![(key.into(), id.into())],
        null_keys: Vec::new(),
        types: types.clone(),
    };
    let insert = |id: &str, text: &str| sql::DraftInsert {
        schema: schema.into(),
        table: relation.into(),
        columns: vec![
            (key.into(), Some(id.into())),
            (label.into(), Some(text.into())),
        ],
        types: types.clone(),
    };
    let apply = |draft: &str| {
        assert!(sql::is_generated_write_on(engine, draft), "{at}: {draft}");
        generated(draft)
    };

    // Keys apart from `generated_writes_round_trip`'s 901 and the verified scenarios'
    // 911 to 914: suites on a shared server run in parallel.
    let cleanup: Vec<(&str, String)> = ["931", "932"]
        .into_iter()
        .filter_map(|id| {
            sql::delete_row(engine, schema, relation, &[(key, id)], &types)
                .map(|statement| (id, statement))
        })
        .collect();
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: cleanup
            .iter()
            .map(|(_, statement)| statement.clone())
            .collect(),
        rollback: true,
        read_write: false,
    };
    // An engine that names rows by every value refuses a delete that matches none.
    for (id, statement) in &cleanup {
        if !subject.label_of(scenario, id).is_empty() {
            generated(statement).unwrap_or_else(|error| panic!("{at}: `{statement}`: {error}"));
        }
    }

    let first = sql::insert_row(
        engine,
        schema,
        relation,
        &[(key, Some("931")), (label, Some("inserted"))],
        &types,
    )
    .unwrap_or_else(|error| panic!("{at}: insert_row: {error}"));
    generated(&first).unwrap_or_else(|error| panic!("{at}: `{first}`: {error}"));

    // The delete runs first, so the key it frees is the key an insert takes.
    let replace = sql::draft_batch(
        engine,
        &[delete("931")],
        &[],
        &[insert("931", "replaced"), insert("932", "added")],
    )
    .unwrap_or_else(|error| panic!("{at}: draft_batch: {error}"));
    apply(&replace).unwrap_or_else(|error| panic!("{at}: `{replace}`: {error}"));
    assert_eq!(
        subject.label_of(scenario, "931"),
        [Some("replaced".to_string())],
        "{at}"
    );
    assert_eq!(
        subject.label_of(scenario, "932"),
        [Some("added".to_string())],
        "{at}"
    );

    // An insert that meets a duplicate key fails the batch, and the bracket
    // takes the delete back with it.
    let collide = sql::draft_batch(engine, &[delete("931")], &[], &[insert("932", "again")])
        .unwrap_or_else(|error| panic!("{at}: draft_batch: {error}"));
    assert!(
        apply(&collide).is_err(),
        "{at}: the duplicate key was accepted: {collide}"
    );
    assert_eq!(
        subject.label_of(scenario, "931"),
        [Some("replaced".to_string())],
        "{at}: the failed draft's delete stayed applied"
    );
    assert_eq!(
        subject.label_of(scenario, "932"),
        [Some("added".to_string())],
        "{at}"
    );
}

/// The steps of a draft the way the workspace builds them: `batch`'s
/// statements cut by [`sql::generated_writes`], each paired with the re-read
/// [`sql::row_reread`] cuts from it. `rows` are the draft's statements in the
/// order [`sql::draft_batch`] writes them (deletes, updates, inserts), each
/// with the row it was fetched from.
pub(super) fn verified_steps(
    engine: Engine,
    batch: &str,
    rows: Vec<(StepKind, Vec<FetchedCell>)>,
) -> Vec<VerifiedStep> {
    let statements = sql::generated_writes(engine, batch)
        .unwrap_or_else(|| panic!("{engine:?}: the gate refuses `{batch}`"));
    assert_eq!(statements.len(), rows.len(), "{engine:?}: {batch}");
    statements
        .into_iter()
        .zip(rows)
        .map(|(statement, (kind, before))| VerifiedStep {
            reread: Some(
                sql::row_reread(engine, &statement)
                    .unwrap_or_else(|| panic!("{engine:?}: no re-read of `{statement}`")),
            ),
            sql: statement,
            kind,
            before,
            read_back: None,
            by_key: true,
        })
        .collect()
}

/// A draft applied through the verified primitive, on a subject whose target
/// has an arm for it: an unchanged draft applies, a row changed or deleted
/// since it was read applies nothing and is named, and a statement that
/// touches two rows is refused with both left as they were. Every step but the
/// one hand-written statement is built the way the workspace builds one: from
/// [`sql::draft_batch`], cut by [`sql::generated_writes`] and paired with
/// [`sql::row_reread`].
pub(super) fn verified_draft_round_trip(subject: &Subject) {
    let scenario = "verified_draft_round_trip";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    if !subject.target.verifies_applies() {
        return;
    }
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let label = subject.columns[1];
    let structure = subject
        .connection
        .structure(schema, relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let types: Vec<(String, String)> = structure
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.data_type.clone()))
        .collect();

    // Ids apart from `generated_draft_round_trip`'s, since suites on a shared
    // server run in parallel.
    let ids = [911_u32, 912, 913, 914];
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: ids
            .iter()
            .map(|id| sql_delete(engine, schema, *id))
            .collect(),
        rollback: true,
        read_write: false,
    };
    for id in ids {
        if !subject.label_of(scenario, &id.to_string()).is_empty() {
            subject.run(scenario, &sql_delete(engine, schema, id), Fetch::default());
        }
    }

    let table = engine.qualified(schema, relation);
    let by_key = |id: u32| format!("{} = {id}", engine.quote_identifier(key));
    let set_label = |text: &str, filter: &str| {
        format!(
            "UPDATE {table} SET {} = {} WHERE {filter}",
            engine.quote_identifier(label),
            engine.quote_literal(text)
        )
    };
    let put = |id: u32, text: &str| {
        subject.run(
            scenario,
            &sql_insert(engine, schema, id, text),
            Fetch::default(),
        );
    };
    // The row as the grid would hold it. `subject.read` stops short of these
    // ids, so the one row is read on its own.
    let fetch = |id: u32| -> Vec<FetchedCell> {
        let columns: Vec<String> = subject
            .columns
            .iter()
            .map(|column| engine.quote_identifier(column))
            .collect();
        let select = format!(
            "SELECT {} FROM {table} WHERE {}",
            columns.join(", "),
            by_key(id)
        );
        let result = subject.run(scenario, &select, Fetch::default());
        let row = result
            .rows
            .first()
            .unwrap_or_else(|| panic!("{at}: row {id} is not there to read"));
        result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| FetchedCell {
                column: column.name.clone(),
                text: row.get(index).map(str::to_string),
                cell_type: None,
                column_type: column.data_type.clone(),
            })
            .collect()
    };
    let delete = |id: &str| sql::DraftDelete {
        schema: schema.into(),
        table: relation.into(),
        keys: vec![(key.into(), id.into())],
        null_keys: Vec::new(),
        types: types.clone(),
    };
    let update = |id: &str, text: &str| PendingRow {
        schema: schema.into(),
        table: relation.into(),
        sets: vec![(label.into(), NewValue::Value(text.to_string().into()))],
        keys: vec![(key.into(), id.into())],
        types: types.clone(),
        ..PendingRow::default()
    };
    let steps =
        |batch: &str, rows: Vec<(StepKind, Vec<FetchedCell>)>| verified_steps(engine, batch, rows);
    let draft = |deletes: &[sql::DraftDelete], updates: &[PendingRow]| {
        sql::draft_batch(engine, deletes, updates, &[])
            .unwrap_or_else(|error| panic!("{at}: draft_batch: {error}"))
    };
    let apply = |steps: &[VerifiedStep]| {
        subject
            .connection
            .apply_verified(steps, &CancelToken::default())
    };

    // (a) A draft that updates one row and deletes another, both as read.
    put(911, "before");
    put(912, "doomed");
    let (first, second) = (fetch(911), fetch(912));
    let batch = draft(&[delete("912")], &[update("911", "after")]);
    let outcome = apply(&steps(
        &batch,
        vec![(StepKind::Delete, second), (StepKind::Update, first)],
    ));
    assert!(
        matches!(outcome, ApplyOutcome::Applied { affected: 2, .. }),
        "{at}: {outcome:?}"
    );
    assert_eq!(
        subject.label_of(scenario, "911"),
        [Some("after".to_string())],
        "{at}"
    );
    assert!(subject.label_of(scenario, "912").is_empty(), "{at}");

    // (b) Another statement changes the row after it was read: nothing is
    // written, and the other statement's value is still what the row holds.
    let held = fetch(911);
    subject.run(
        scenario,
        &set_label("theirs", &by_key(911)),
        Fetch::default(),
    );
    let batch = draft(&[], &[update("911", "mine")]);
    assert_eq!(
        apply(&steps(&batch, vec![(StepKind::Update, held)])),
        ApplyOutcome::Drifted(vec![(0, Drift::Changed)]),
        "{at}"
    );
    assert_eq!(
        subject.label_of(scenario, "911"),
        [Some("theirs".to_string())],
        "{at}: a drifted draft wrote"
    );

    // (c) A hand-written statement the gate admits whose `WHERE` matches two
    // rows. Its re-read is the one cut from it, which finds both and refuses
    // before any write.
    put(913, "twin");
    put(914, "twin");
    let held = fetch(913);
    let merge = set_label(
        "merged",
        &format!(
            "{} = {}",
            engine.quote_identifier(label),
            engine.quote_literal("twin")
        ),
    );
    assert!(sql::is_generated_write_on(engine, &merge), "{at}: {merge}");
    let keyed = draft(&[], &[update("913", "merged")]);
    let reread = steps(&keyed, vec![(StepKind::Update, held)]).remove(0);
    let twin = VerifiedStep {
        reread: sql::row_reread(engine, &merge),
        sql: merge,
        ..reread
    };
    assert_eq!(
        apply(&[twin]),
        ApplyOutcome::Drifted(vec![(0, Drift::Ambiguous(2))]),
        "{at}"
    );
    for id in ["913", "914"] {
        assert_eq!(
            subject.label_of(scenario, id),
            [Some("twin".to_string())],
            "{at}: a miscounted draft kept its write"
        );
    }

    // (d) The row is deleted after it was read.
    let held = fetch(911);
    subject.run(scenario, &sql_delete(engine, schema, 911), Fetch::default());
    let batch = draft(&[], &[update("911", "mine")]);
    assert_eq!(
        apply(&steps(&batch, vec![(StepKind::Update, held)])),
        ApplyOutcome::Drifted(vec![(0, Drift::Gone)]),
        "{at}"
    );
    assert!(subject.label_of(scenario, "911").is_empty(), "{at}");
}

/// A table with no primary key, read through the driver and written back by
/// every value it holds: its target names every column, a draft that updates a
/// row whose cell is NULL and deletes another applies with `IS NULL` in its
/// predicate, and two identical rows make an edit refuse before anything is
/// written, naming the step. The scratch table is the scenario's own, dropped
/// before it starts and when it ends. Steps are built the way the workspace
/// builds them, as in [`verified_draft_round_trip`].
pub(super) fn verified_keyless_round_trip(subject: &Subject) {
    let scenario = "verified_keyless_round_trip";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    if !subject.target.verifies_applies() {
        return;
    }
    let (schema, relation) = (subject.schema, "dbdelve_keyless_scratch");
    let table = engine.qualified(schema, relation);
    let drop = format!("DROP TABLE IF EXISTS {table}");
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![drop.clone()],
        rollback: true,
        read_write: false,
    };
    subject.run(scenario, &drop, Fetch::default());
    subject.run(
        scenario,
        &format!(
            "CREATE TABLE {table} ({} VARCHAR(32), {} INTEGER)",
            engine.quote_identifier("label"),
            engine.quote_identifier("n")
        ),
        Fetch::default(),
    );
    let types = vec![
        ("label".to_string(), "varchar".to_string()),
        ("n".to_string(), "integer".to_string()),
    ];
    for (label, n) in [
        ("alpha", None),
        ("beta", Some("2")),
        ("dup", Some("3")),
        ("dup", Some("3")),
    ] {
        let insert = sql::insert_row(
            engine,
            schema,
            relation,
            &[("label", Some(label)), ("n", n)],
            &types,
        )
        .unwrap_or_else(|error| panic!("{at}: insert_row: {error}"));
        assert!(
            sql::is_generated_write_on(engine, &insert),
            "{at}: {insert}"
        );
        subject.run(scenario, &insert, Fetch::default());
    }

    let read = || {
        subject.run(
            scenario,
            &format!("SELECT * FROM {table}"),
            Fetch::default(),
        )
    };
    let held = |result: &QueryResult, row: usize| -> Vec<FetchedCell> {
        result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| FetchedCell {
                column: column.name.clone(),
                text: result.rows[row].get(index).map(str::to_string),
                cell_type: None,
                column_type: column.data_type.clone(),
            })
            .collect()
    };
    let rows_now = || -> Vec<(String, Option<String>)> {
        let mut rows: Vec<_> = read()
            .rows
            .iter()
            .map(|row| {
                (
                    row.get(0).unwrap_or_default().to_string(),
                    row.get(1).map(str::to_string),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    let first = read();
    let target = first
        .edit
        .clone()
        .unwrap_or_else(|| panic!("{at}: no edit target: {:?}", first.read_only_reason));
    assert_eq!(
        target.row_match,
        crate::db::RowMatch::EveryColumn { fixed: vec![] },
        "{at}"
    );
    assert_eq!(target.keys, [0, 1], "{at}");
    assert_eq!(
        target.columns,
        [Some("label".to_string()), Some("n".to_string())],
        "{at}"
    );
    // The row at `result`'s index `row`, named the way the grid names it: the
    // cells it has by value, the NULL ones apart.
    let named = |result: &QueryResult, row: usize| -> (Vec<(String, String)>, Vec<String>) {
        let mut keys = Vec::new();
        let mut nulls = Vec::new();
        for (index, name) in ["label", "n"].into_iter().enumerate() {
            match result.rows[row].get(index) {
                Some(value) => keys.push((name.to_string(), value.to_string())),
                None => nulls.push(name.to_string()),
            }
        }
        (keys, nulls)
    };
    let index_of = |result: &QueryResult, label: &str| {
        result
            .rows
            .iter()
            .position(|row| row.get(0) == Some(label))
            .unwrap_or_else(|| panic!("{at}: no row {label}"))
    };
    let delete = |result: &QueryResult, row: usize| {
        let (keys, null_keys) = named(result, row);
        sql::DraftDelete {
            schema: schema.into(),
            table: relation.into(),
            keys,
            null_keys,
            types: types.clone(),
        }
    };
    let update = |result: &QueryResult, row: usize, text: &str| {
        let (keys, null_keys) = named(result, row);
        PendingRow {
            schema: schema.into(),
            table: relation.into(),
            sets: vec![("label".into(), NewValue::Value(text.to_string().into()))],
            keys,
            null_keys,
            types: types.clone(),
            ..PendingRow::default()
        }
    };
    let apply = |steps: &[VerifiedStep]| {
        subject
            .connection
            .apply_verified(steps, &CancelToken::default())
    };

    // (a) One row whose `n` is NULL is updated and another is deleted, each
    // named by every value it was read with.
    let (alpha, beta) = (index_of(&first, "alpha"), index_of(&first, "beta"));
    let batch = sql::draft_batch(
        engine,
        &[delete(&first, beta)],
        &[update(&first, alpha, "gamma")],
        &[],
    )
    .unwrap_or_else(|error| panic!("{at}: draft_batch: {error}"));
    let steps = verified_steps(
        engine,
        &batch,
        vec![
            (StepKind::Delete, held(&first, beta)),
            (StepKind::Update, held(&first, alpha)),
        ],
    );
    assert!(
        steps.iter().any(|step| step.sql.contains("IS NULL")),
        "{at}: the NULL cell is not matched with IS NULL: {batch}"
    );
    let outcome = apply(&steps);
    assert!(
        matches!(outcome, ApplyOutcome::Applied { affected: 2, .. }),
        "{at}: {outcome:?}"
    );
    let dup = || ("dup".to_string(), Some("3".to_string()));
    assert_eq!(
        rows_now(),
        [dup(), dup(), ("gamma".to_string(), None)],
        "{at}"
    );

    // (b) Two rows alike in every value: the edit is refused before any write
    // and names the step, and both rows are as they were.
    let second = read();
    let twin = index_of(&second, "dup");
    let batch = sql::draft_batch(engine, &[], &[update(&second, twin, "merged")], &[])
        .unwrap_or_else(|error| panic!("{at}: draft_batch: {error}"));
    let steps = verified_steps(
        engine,
        &batch,
        vec![(StepKind::Update, held(&second, twin))],
    );
    assert_eq!(
        apply(&steps),
        ApplyOutcome::Drifted(vec![(0, Drift::Ambiguous(2))]),
        "{at}"
    );
    assert_eq!(
        rows_now(),
        [dup(), dup(), ("gamma".to_string(), None)],
        "{at}: a refused draft wrote"
    );
}

/// What DBDelve writes for a browsing surface, run on the subject: a paged
/// preview, a header sort spliced into it, a count, and every filter operator
/// the engine offers, each passing the gate and narrowing the fixture to the
/// rows it should.
pub(super) fn generated_reads_run(subject: &Subject) {
    let scenario = "generated_reads_run";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let (label, whole) = (subject.columns[1], subject.columns[2]);
    let structure = subject
        .connection
        .structure(schema, relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let type_of = |column: &str| {
        structure
            .columns
            .iter()
            .find(|candidate| candidate.name == column)
            .map(|candidate| candidate.data_type.clone())
    };
    let run = |sql: &str| {
        assert!(
            sql::is_generated_select(engine, sql),
            "{at}: the gate refuses `{sql}`"
        );
        subject
            .connection
            .generated(sql, &CancelToken::default(), None, None)
            .unwrap_or_else(|error| panic!("{at}: `{sql}` failed: {error}"))
    };
    let predicate = |column: &str, operator: Operator, value: &str| {
        filter::filter_predicate(engine, column, type_of(column).as_deref(), operator, value)
            .unwrap_or_else(|| panic!("{at}: no predicate for {operator:?} `{value}`"))
    };
    let ids = |result: &QueryResult| -> Vec<String> {
        let column = result
            .columns
            .iter()
            .position(|column| column.name == key)
            .unwrap_or_else(|| panic!("{at}: no key column in the preview"));
        result
            .rows
            .iter()
            .map(|row| row.get(column).unwrap_or_default().to_string())
            .collect()
    };
    let fixture_only = predicate(key, Operator::Less, "900");
    let filtered = |column: &str, operator: Operator, value: &str| {
        let filter = format!("{fixture_only} AND {}", predicate(column, operator, value));
        let sql = format!(
            "{} ORDER BY {}",
            explorer::preview_sql(engine, schema, relation, &filter, None, 0),
            engine.quote_identifier(key)
        );
        (ids(&run(&sql)), sql)
    };

    let cases = [
        (label, Operator::Equals, "alpha", &["1"][..]),
        (label, Operator::NotEquals, "alpha", &["2", "3"]),
        (label, Operator::Contains, "lph", &["1"]),
        (label, Operator::NotContains, "lph", &["2", "3"]),
        (label, Operator::StartsWith, "nul", &["3"]),
        (label, Operator::EndsWith, "pha", &["1"]),
        (key, Operator::Greater, "1", &["2", "3"]),
        (key, Operator::GreaterOrEqual, "2", &["2", "3"]),
        (key, Operator::Less, "2", &["1"]),
        (key, Operator::LessOrEqual, "2", &["1", "2"]),
        (whole, Operator::IsNull, "", &["3"]),
        (whole, Operator::IsNotNull, "", &["1", "2"]),
        (label, Operator::IsEmpty, "", &[]),
        (label, Operator::IsNotEmpty, "", &["1", "2", "3"]),
        (key, Operator::InList, "1, 3", &["1", "3"]),
        (key, Operator::NotInList, "1, 3", &["2"]),
        (key, Operator::Between, "1 .. 2", &["1", "2"]),
        (label, Operator::Regex, "^al", &["1"]),
    ];
    for operator in Operator::ALL
        .into_iter()
        .filter(|operator| operator.on(engine))
    {
        let (column, _, value, wanted) = cases
            .iter()
            .find(|case| case.1 == operator)
            .unwrap_or_else(|| panic!("{at}: no case for {operator:?}"));
        let (found, sql) = filtered(column, operator, value);
        assert_eq!(found, *wanted, "{at}: {operator:?} `{value}`: {sql}");
    }

    // The cells of the fixture's first two rows as the grid shows them, so
    // each column type's literal has to reach the server and match.
    for column in [3, 5, 6, 7] {
        for (row, id) in [(0, "1"), (1, "2")] {
            let value = EXPECTED[row][column].expect("the first two rows hold no NULL");
            let (found, sql) = filtered(subject.columns[column], Operator::Equals, value);
            assert_eq!(found, [id], "{at}: `{value}`: {sql}");
        }
    }

    let count = run(&explorer::count_sql(
        engine,
        schema,
        relation,
        &fixture_only,
    ));
    assert_eq!(count.rows[0].get(0), Some("3"), "{at}: count");

    let page = explorer::preview_sql(engine, schema, relation, &fixture_only, Some(2), 1);
    let sorted = sql::with_order_by(
        engine,
        &page,
        &[SortKey {
            expression: engine.quote_identifier(key),
            ascending: false,
        }],
    )
    .unwrap_or_else(|| panic!("{at}: no ORDER BY spliced into `{page}`"));
    assert_eq!(ids(&run(&sorted)), ["2", "1"], "{at}: {sorted}");
}

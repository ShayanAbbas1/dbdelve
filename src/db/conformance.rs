//! One set of scenarios every engine's live tests run unchanged, against the
//! small `conformance` fixture each dev database is seeded with.
//!
//! An engine's `tests` module builds a [`Subject`] (an open connection plus
//! the per-engine data a scenario cannot derive) and lists the scenarios it
//! takes in [`suite!`]. Engines are asked only through `Engine` and
//! `db::Connection`, so a scenario that fails is a finding about the engine,
//! not something to bend the scenario around.
//!
//! The scenarios expect a connection with privilege enough to create and drop
//! roles, sequences, tables and large objects, which on the compose Postgres
//! means a superuser, so a failure on a less privileged account is about the
//! account and not the engine. The database is shared and several scenarios
//! may run against it at once, which is why every object a scenario creates is
//! named for that scenario. Each scenario removes its own objects once before
//! it starts as well as when it ends, so a run stopped by a signal costs a
//! re-run and not a manual cleanup.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::{
    ApplyOutcome, ApplyStage, CancelToken, Connection, Drift, Driver, Engine, Fetch, FetchedCell,
    QueryResult, RelationKind, StepKind, Stopped, Target, VerifiedStep, is_cancel,
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
        .catalog(&crate::db::Shown::everything())
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

/// The classifier's new verdicts against what the server does, as a pair per
/// case: the statement is rated above a read, and the server really does what
/// that rating claims. Each is matched by a control that must not move, a
/// statement whose effect on the hold is none, which is rated a read and
/// leaves the canary write refused in the same session -- without it a server
/// that refused everything would pass.
pub(super) fn denied_functions_agree_with_the_server(subject: &Subject) {
    let scenario = "denied_functions_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    let schema = subject.schema;
    // Row 970, one lock key, one large object and one sequence, all named for
    // this scenario: CI runs four scenarios at a time against one server.
    let canary = 970;
    let sequence = engine.qualified(schema, "dbdelve_denied_970");
    let insert = sql_insert(engine, schema, canary, "denied function");
    let rated = |sql: &str| sql::classify(engine, sql).mode;
    let above_a_read = |sql: &str| {
        assert!(
            rated(sql) > sql::Mode::ReadOnly,
            "{at}: `{sql}` is rated {:?}, which a Read-only profile would run",
            rated(sql)
        );
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let cell = |result: &QueryResult| {
        result
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: no value came back"))
            .to_string()
    };

    let mut unwind = vec![
        sql_delete(engine, schema, canary),
        format!("DROP SEQUENCE IF EXISTS {sequence}"),
    ];
    let (held_calls, control, advance, current): (Vec<(&str, &str)>, &str, String, String) =
        match engine {
            Engine::Postgres => {
                unwind.push("SELECT pg_advisory_unlock_all()".to_string());
                unwind.push("SELECT lo_unlink(970970)".to_string());
                // Queued before anything creates it, so a panic part-way
                // through the write cases still removes it.
                unwind.push("SELECT lo_unlink(970971)".to_string());
                unwind.push("SELECT lo_unlink(970972)".to_string());
                unwind.push("SELECT lo_unlink(970974)".to_string());
                (
                    vec![
                        ("an advisory lock", "SELECT pg_advisory_lock(970970)"),
                        (
                            "a notification",
                            "SELECT pg_notify('dbdelve_denied_970', 'x')",
                        ),
                        ("a large object", "SELECT lo_create(970970)"),
                    ],
                    "SELECT current_setting('search_path')",
                    format!("SELECT nextval('{schema}.dbdelve_denied_970')"),
                    format!("SELECT currval('{schema}.dbdelve_denied_970')"),
                )
            }
            Engine::MySql | Engine::MariaDb => {
                unwind.push("SELECT RELEASE_ALL_LOCKS()".to_string());
                (
                    vec![("a named lock", "SELECT GET_LOCK('dbdelve_denied_970', 0)")],
                    "SELECT version()",
                    format!("SELECT NEXTVAL({sequence})"),
                    format!("SELECT LASTVAL({sequence})"),
                )
            }
            _ => (
                Vec::new(),
                "SELECT version()",
                format!("SELECT nextval('{schema}.dbdelve_denied_970')"),
                format!("SELECT currval('{schema}.dbdelve_denied_970')"),
            ),
        };
    // `Cleanup` runs on a panic but not on a kill, so a run stopped by a
    // signal, an out-of-memory kill or a CI timeout leaves the sequence, the
    // large objects and the canary row behind, and the first create below then
    // fails as though the classifier were wrong. Run the unwind list once
    // before anything is created: it is the scenario's own list rather than a
    // second one, so an entry added to the cleanup is pre-cleaned without
    // anyone remembering to. Errors are the normal case here, since on a clean
    // server every one of these statements has nothing to remove, so they go
    // through `query` and not `subject.run`, which panics.
    for statement in &unwind {
        let _ = subject
            .connection
            .query(statement, &CancelToken::default(), Fetch::default());
    }
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: unwind,
        rollback: false,
        read_write: engine.holds_read_only(),
    };

    // An engine with no hold has nothing to differ on here: both directions
    // of the switch must still be accepted, and the sequence case below runs.
    if !engine.holds_read_only() {
        hold(true);
        hold(false);
    } else {
        let refused = || {
            let error = subject
                .connection
                .query(&insert, &CancelToken::default(), Fetch::default())
                .expect_err(&format!("{at}: the hold took a write it should refuse"));
            assert!(
                error.message.contains(subject.read_only_refusal),
                "{at}: refused for another reason: {}",
                error.message
            );
        };
        // The control: rated a read, runs on the held session, and the hold
        // is still armed after it.
        let controlled = || {
            assert_eq!(
                rated(control),
                sql::Mode::ReadOnly,
                "{at}: control `{control}`"
            );
            hold(true);
            subject.run(scenario, control, Fetch::default());
            refused();
            hold(false);
        };

        for (case, call) in &held_calls {
            above_a_read(call);
            hold(true);
            // The call itself succeeds with the hold armed, which is why the
            // verdict has to be the one to stop it.
            subject.run(&format!("{scenario}: {case}"), call, Fetch::default());
            hold(false);
            controlled();
        }

        if engine == Engine::Postgres {
            // The server decodes `\006f` to an `o` where the lexer reports the
            // raw text, so the second spelling is the same call the first is.
            for disarm in [
                "SELECT set_config('default_transaction_read_only', 'off', false)",
                r#"SELECT U&"set_c\006fnfig"('default_transaction_read_only', 'off', false)"#,
                // The denied call is inside a string, so the name scan sees
                // the carrier and never the payload.
                "SELECT query_to_xml('select set_config(''default_transaction_read_only'', ''off'', false)', true, false, '')",
                // Table position and a dollar-quoted payload: the rule does
                // not depend on the scalar position or the single-quoted
                // spelling.
                "SELECT count(*) FROM ts_stat($q$select to_tsvector(set_config('default_transaction_read_only', 'off', false))$q$)",
                // This carrier's text argument has no name in the catalog,
                // which is why the shape probe below cannot find it.
                "SELECT ts_rewrite('a & b'::tsquery, 'select set_config(''default_transaction_read_only'', ''off'', false)::tsquery, ''c''::tsquery')",
            ] {
                above_a_read(disarm);
                hold(true);
                refused();
                subject.run(scenario, disarm, Fetch::default());
                // The write that was refused a moment ago now goes through.
                subject.run(scenario, &insert, Fetch::default());
                hold(false);
                controlled();
                // The canary is the same row for every spelling.
                subject.run(
                    scenario,
                    &sql_delete(engine, schema, canary),
                    Fetch::default(),
                );
            }

            // The three calls that wrote under the hold when the family was
            // reviewed, in order, each rated above a read first. `lo_creat`,
            // `lowrite` and `lo_truncate64` take the same path and would need
            // their own oid bookkeeping for no new evidence; the corpus covers
            // their classification.
            let object = "970971";
            let read_back = format!("SELECT convert_from(lo_get({object}), 'UTF8')");
            assert_eq!(
                rated(&read_back),
                sql::Mode::ReadOnly,
                "{at}: `{read_back}`"
            );
            hold(true);
            for write in [
                format!("SELECT lo_from_bytea({object}, 'abc'::bytea)"),
                format!("SELECT lo_put({object}, 0, 'xyz'::bytea)"),
                format!("SELECT lo_truncate(lo_open({object}, 131072), 1)"),
            ] {
                above_a_read(&write);
                subject.run(scenario, &write, Fetch::default());
            }
            // The server wrote while the hold was armed: that is the evidence.
            assert_eq!(
                cell(&subject.run(scenario, &read_back, Fetch::default())),
                "x",
                "{at}: the object was not rewritten and truncated under the hold"
            );
            hold(false);
            controlled();

            // The same evidence reached through a string literal: the server
            // took a write while the hold was armed, and the only thing the
            // classifier can see is the carrier's name. This is the wider half
            // of the exposure, since nothing was released to do it.
            let carried = "970972";
            let carried_write = format!(
                "SELECT query_to_xml('select lo_from_bytea({carried}, ''abc''::bytea)', true, false, '')"
            );
            let carried_read = format!("SELECT convert_from(lo_get({carried}), 'UTF8')");
            above_a_read(&carried_write);
            hold(true);
            subject.run(scenario, &carried_write, Fetch::default());
            assert_eq!(
                cell(&subject.run(scenario, &carried_read, Fetch::default())),
                "abc",
                "{at}: the object was not created through the carrier under the hold"
            );
            hold(false);
            controlled();

            // The same write through a carrier whose text argument the catalog
            // does not name: the verdict is the only thing between the
            // statement and the server here too.
            let unnamed = "970974";
            let unnamed_write = format!(
                "SELECT ts_rewrite('a & b'::tsquery, 'select lo_from_bytea({unnamed}, ''abc''::bytea)::text::tsquery, ''c''::tsquery')"
            );
            let unnamed_read = format!("SELECT convert_from(lo_get({unnamed}), 'UTF8')");
            above_a_read(&unnamed_write);
            hold(true);
            subject.run(scenario, &unnamed_write, Fetch::default());
            assert_eq!(
                cell(&subject.run(scenario, &unnamed_read, Fetch::default())),
                "abc",
                "{at}: the object was not created through `ts_rewrite` under the hold"
            );
            hold(false);
            controlled();

            // The catalog is the authority on the family. A name neither list
            // knows fails here by name, which is the point rather than a
            // flake: a later server version added a function, and what it does
            // is a decision. The namespace keeps an extension's function out,
            // and the escaped underscore keeps the prefix from widening to
            // every name that begins with `lo`.
            let reads = [
                "lo_get",
                "loread",
                "lo_open",
                "lo_close",
                "lo_lseek",
                "lo_lseek64",
                "lo_tell",
                "lo_tell64",
            ];
            let family = subject.run(
                scenario,
                "SELECT DISTINCT p.proname FROM pg_catalog.pg_proc p \
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = 'pg_catalog' \
                 AND (p.proname LIKE 'lo\\_%' OR p.proname IN ('loread', 'lowrite')) \
                 ORDER BY 1",
                Fetch::default(),
            );
            let listed = engine.spec().sql.denied_functions;
            let unlisted: Vec<_> = family
                .rows
                .iter()
                .filter_map(|row| row.get(0))
                .filter(|name| !listed.contains(name) && !reads.contains(name))
                .collect();
            assert!(
                unlisted.is_empty(),
                "{at}: large-object functions on neither the deny-list nor the reads: {unlisted:?}"
            );

            // The executors found so far are found by shape, not by name: what
            // they share is an argument that is SQL text, which the server
            // records as one named `query` or `sqlquery`. A carrier whose text
            // argument the catalog does not name has no such shape, so this
            // probe cannot see it; the pin below holds those by name. Measured
            // against Postgres 15.13 (Debian 15.13-1.pgdg110+1): this query
            // returns 5 distinct names, one of them carved out below, while
            // widening it to every `pg_catalog` function taking a `text`
            // argument the catalog leaves unnamed returns 222. The rule was to
            // widen only when that population is no larger than the deny-list
            // (47 entries then), and 222 is not: a carve-out that size would
            // be a second deny-list with no evidence behind its entries. The
            // lists were checked against that version, so a later version's
            // addition fails here by name only when the probe server is that
            // later version. Extension carriers, such as `tablefunc.crosstab`
            // and `xml2.xpath_table`, live outside `pg_catalog` and so outside
            // this probe; they are listed, and pinned against their own
            // extension's catalog by
            // `live_extension_carriers_agree_with_the_server` in
            // `src/db/postgres.rs`. A name here neither list knows is
            // a function a later version added, and whether it runs its text
            // is a decision: one that does goes on the deny-list, one that
            // does not goes in the array below with what its `query` actually
            // is.
            let not_run = [
                // `query` is an OUT column here, the text of the statement a
                // backend is running; nothing takes it as an argument.
                "pg_stat_get_activity",
            ];
            let executors = subject.run(
                scenario,
                "SELECT DISTINCT p.proname FROM pg_catalog.pg_proc p \
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = 'pg_catalog' \
                 AND (p.proargnames @> ARRAY['query'] OR p.proargnames @> ARRAY['sqlquery']) \
                 ORDER BY 1",
                Fetch::default(),
            );
            let found: Vec<_> = executors.rows.iter().filter_map(|row| row.get(0)).collect();
            let unknown: Vec<_> = found
                .iter()
                .filter(|name| !listed.contains(name) && !not_run.contains(name))
                .collect();
            assert!(
                unknown.is_empty(),
                "{at}: functions taking SQL text on neither the deny-list nor the carve-out: {unknown:?}"
            );
            // An empty answer is a failed probe: a server built without these
            // functions has not been checked, and would otherwise pass here.
            for executor in [
                "query_to_xml",
                "query_to_xml_and_xmlschema",
                "query_to_xmlschema",
                "ts_stat",
            ] {
                assert!(
                    found.contains(&executor),
                    "{at}: `{executor}` is missing from the server's catalog, so the executors were not probed (found {found:?})"
                );
            }

            // Carriers the catalog gives no argument names. The probe above
            // finds carriers by the shape the catalog records, and one whose
            // arguments the catalog does not name has no shape to find, so it
            // is held by name instead: removing one from the deny-list fails
            // here.
            let unnamed_carriers = ["ts_rewrite"];
            for unnamed in unnamed_carriers {
                assert!(
                    listed.contains(&unnamed),
                    "{at}: `{unnamed}` takes SQL text the catalog does not name, and is not on the deny-list"
                );
            }
        }
    }

    if engine.spec().sql.denied_functions.contains(&"nextval") {
        subject.run(
            scenario,
            &format!("CREATE SEQUENCE {sequence}"),
            Fetch::default(),
        );
        above_a_read(&advance);
        assert_eq!(rated(&current), sql::Mode::ReadOnly, "{at}: `{current}`");
        let first: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        let second: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            second,
            first + 1,
            "{at}: the call did not advance the sequence"
        );
        // The control reads where the sequence stands and leaves it there.
        let read: i64 = cell(&subject.run(scenario, &current, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            read, second,
            "{at}: the read-only spelling moved the sequence"
        );
        let third: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            third,
            second + 1,
            "{at}: the control disturbed the sequence"
        );

        // DuckDB holds no session read-only, so the verdict is the whole
        // boundary there and the counter is the canary. Observed on the
        // embedded `v2.0.0-dev88614`: `query` and `json_execute_serialized_sql`
        // run the text they are handed, `query_table` resolves a table name and
        // runs none. Asserted as that answer, not as whatever the server says,
        // so a DuckDB that changes it fails here by name.
        if engine == Engine::DuckDb {
            let inner = format!("select nextval(''{schema}.dbdelve_denied_970'')");
            let counter = || -> i64 {
                cell(&subject.run(scenario, &current, Fetch::default()))
                    .parse()
                    .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"))
            };
            for carrier in [
                format!("SELECT * FROM query('{inner}')"),
                format!("SELECT * FROM json_execute_serialized_sql(json_serialize_sql('{inner}'))"),
            ] {
                above_a_read(&carrier);
                let before = counter();
                subject.run(scenario, &carrier, Fetch::default());
                assert_eq!(
                    counter(),
                    before + 1,
                    "{at}: `{carrier}` did not run its text, so it is not a carrier and should not be listed"
                );
            }
            let by_name = format!("SELECT * FROM query_table('{inner}')");
            assert_eq!(
                rated(&by_name),
                sql::Mode::ReadOnly,
                "{at}: `{by_name}` is rated above a read, but query_table runs no text"
            );
            let before = counter();
            let refused = subject
                .connection
                .query(&by_name, &CancelToken::default(), Fetch::default())
                .expect_err(&format!(
                    "{at}: query_table ran `{by_name}`, so it is a carrier and should be listed"
                ));
            assert!(
                refused.message.contains("does not exist"),
                "{at}: query_table refused `{by_name}` for another reason: {}",
                refused.message
            );
            assert_eq!(
                counter(),
                before,
                "{at}: query_table moved the sequence through `{by_name}`"
            );
        }
    }
}

/// The classifier's verdicts on the statements that undo a Read-only hold,
/// against what the server does with them. Each statement is rated above a
/// read, and with the hold armed the canary write the server refused a moment
/// ago goes through after it. The one statement that is not a disarm is the
/// identity change: the hold stays armed, so that case asserts on the session's
/// reported user instead, and on the hold still being on.
///
/// Every positive is matched by a control from `plain_settings`, rated a read,
/// that leaves the same canary write refused in the same session. Without the
/// controls a server that refused everything would pass.
pub(super) fn session_settings_agree_with_the_server(subject: &Subject) {
    let scenario = "session_settings_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    let schema = subject.schema;
    let rated = |sql: &str| sql::classify(engine, sql).mode;
    let above_a_read = |sql: &str| {
        assert!(
            rated(sql) > sql::Mode::ReadOnly,
            "{at}: `{sql}` is rated {:?}, which a Read-only profile would run",
            rated(sql)
        );
    };

    // The statement an engine sends to release its own hold is the one the
    // classifier must never let a Read-only profile run. Pure, so it covers
    // every engine and not only the live one.
    for each in Engine::ALL {
        if let Some(release) = super::read_only_statement(each, false) {
            assert!(
                sql::classify(each, release).mode > sql::Mode::ReadOnly,
                "{at}: the statement {each:?} sends to release its hold, `{release}`, is rated {:?}",
                sql::classify(each, release).mode
            );
        }
    }
    if !engine.holds_read_only() {
        return;
    }

    // Row 971, named for this scenario: CI runs four scenarios at a time
    // against one server.
    let canary = 971;
    let insert = sql_insert(engine, schema, canary, "session setting");
    let delete = sql_delete(engine, schema, canary);
    let postgres = engine == Engine::Postgres;
    // Named for this scenario like the canary row. Queued for removal here,
    // before the identity-change case creates it, so an interrupted run
    // removes it and the pre-clean below removes a leftover one.
    let identity = "dbdelve_session_971";
    let unwind = if postgres {
        vec![
            "RESET ROLE".to_string(),
            "RESET SESSION AUTHORIZATION".to_string(),
            delete.clone(),
            format!("DROP ROLE IF EXISTS {identity}"),
        ]
    } else {
        vec![delete.clone()]
    };
    // `Cleanup` runs on a panic but not on a kill, so a stopped run leaves the
    // canary row behind and the insert below then fails on a duplicate key as
    // though the classifier were wrong. The unwind list is the cleanup's own,
    // read once here before anything is written; errors are ignored because
    // on a clean server there is nothing to remove.
    for statement in &unwind {
        let _ = subject
            .connection
            .query(statement, &CancelToken::default(), Fetch::default());
    }
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: unwind,
        rollback: true,
        read_write: true,
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    let cell = |sql: &str| {
        run(sql)
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: `{sql}` answered nothing"))
            .to_string()
    };
    let refused = || {
        let error = subject
            .connection
            .query(&insert, &CancelToken::default(), Fetch::default())
            .expect_err(&format!("{at}: the hold took a write it should refuse"));
        assert!(
            error.message.contains(subject.read_only_refusal),
            "{at}: refused for another reason: {}",
            error.message
        );
    };
    // Back to a clean session: nothing open, the hold lifted, no canary row.
    let reset = || {
        run("ROLLBACK");
        hold(false);
        run(&delete);
    };
    // Rated above a read, and the write refused with the hold armed goes
    // through after it, with the hold still nominally on.
    let disarms = |case: &str, before: &[&str], statement: &str| {
        above_a_read(statement);
        hold(true);
        refused();
        for sql in before {
            run(sql);
        }
        subject.run(&format!("{scenario}: {case}"), statement, Fetch::default());
        subject.run(&format!("{scenario}: {case}"), &insert, Fetch::default());
        reset();
    };
    // Rated a read, and the same write is still refused afterwards.
    let leaves_the_hold = |case: &str, before: &[&str], statement: &str| {
        assert_eq!(
            rated(statement),
            sql::Mode::ReadOnly,
            "{at}: control `{statement}`"
        );
        hold(true);
        for sql in before {
            run(sql);
        }
        subject.run(&format!("{scenario}: {case}"), statement, Fetch::default());
        refused();
        reset();
    };

    let release = super::read_only_statement(engine, false)
        .unwrap_or_else(|| panic!("{at}: a holding engine has a statement that releases it"));
    disarms("the hold's own release", &[], release);

    if postgres {
        disarms("a write transaction", &[], "BEGIN READ WRITE");
        disarms(
            "a session write access mode",
            &[],
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        );
        disarms(
            "a transaction write access mode",
            &["BEGIN"],
            "SET TRANSACTION READ WRITE",
        );
        disarms(
            "the transaction read-only setting",
            &["BEGIN"],
            "SET transaction_read_only = off",
        );
        disarms(
            "the function form of the setting",
            &[],
            "SELECT set_config('default_transaction_read_only', 'off', false)",
        );

        // An identity change leaves the hold armed, so there is no canary to
        // ask: the session's own answer to who it is has to move instead. The
        // identity is created rather than found: the server's own catalog
        // answers with whichever role sorts first, typically a built-in one
        // that needs superuser or membership to assume, so the case was
        // asserting the classifier's verdict against an identity nobody
        // chose. `NOLOGIN` and granted nothing: it exists to be switched to.
        let other = engine.quote_identifier(identity);
        run(&format!("CREATE ROLE {other} NOLOGIN"));
        for (case, statement, undo) in [
            ("a role change", format!("SET ROLE {other}"), "RESET ROLE"),
            (
                "an authorization change",
                format!("SET SESSION AUTHORIZATION {other}"),
                "RESET SESSION AUTHORIZATION",
            ),
        ] {
            above_a_read(&statement);
            hold(true);
            let who = cell("SELECT current_user");
            subject.run(&format!("{scenario}: {case}"), &statement, Fetch::default());
            assert_ne!(
                cell("SELECT current_user"),
                who,
                "{at}: {case} did not change the session's user"
            );
            assert_eq!(
                cell("SELECT current_setting('default_transaction_read_only')"),
                "on",
                "{at}: {case} disarmed the hold, which it is rated for not doing"
            );
            run(undo);
            reset();
        }

        leaves_the_hold(
            "a search-path change",
            &[],
            &format!("SET search_path TO {schema}"),
        );
        leaves_the_hold(
            "a read access mode",
            &["BEGIN"],
            "SET TRANSACTION READ ONLY",
        );
    } else {
        disarms("a write transaction", &[], "START TRANSACTION READ WRITE");
        disarms(
            "a write access mode for the next transaction",
            &[],
            "SET TRANSACTION READ WRITE",
        );
        disarms(
            "the transaction read-only setting",
            &[],
            "SET SESSION transaction_read_only = 0",
        );
        disarms(
            "the transaction read-only setting, doubled-at",
            &[],
            "SET @@session.transaction_read_only = OFF",
        );
        if engine == Engine::MariaDb {
            disarms("its older spelling", &[], "SET tx_read_only = 0");
        } else {
            // Removed in 8.0.3, so the server refuses the name outright. It is
            // still rated above a read, and the hold is still armed.
            above_a_read("SET tx_read_only = 0");
            hold(true);
            let error = subject
                .connection
                .query(
                    "SET tx_read_only = 0",
                    &CancelToken::default(),
                    Fetch::default(),
                )
                .expect_err(&format!("{at}: the server accepted a removed variable"));
            assert!(
                error.message.contains("Unknown system variable"),
                "{at}: refused for another reason: {}",
                error.message
            );
            refused();
            reset();
        }

        leaves_the_hold("a comfort setting", &[], "SET SESSION wait_timeout = 28800");
        leaves_the_hold("a read access mode", &[], "SET TRANSACTION READ ONLY");

        // The server-wide scope of a transaction setting, which the parser reads
        // as no scope at all. The access-mode spelling is rated and never sent:
        // it would change the default for every later session on a server these
        // scenarios share. The isolation-level spelling is sent set to the value
        // already in force, so a server that accepts it is left as it was, and
        // one that refuses it for want of the privilege has said it is not a
        // read. Either answer is asserted, so neither is a skip.
        above_a_read("SET GLOBAL TRANSACTION READ ONLY");
        let level = cell("SELECT @@global.transaction_isolation");
        let spelled = level.replace('-', " ");
        let server_wide = format!("SET GLOBAL TRANSACTION ISOLATION LEVEL {spelled}");
        above_a_read(&server_wide);
        hold(true);
        match subject
            .connection
            .query(&server_wide, &CancelToken::default(), Fetch::default())
        {
            Err(error) => assert!(
                error.message.contains("SUPER"),
                "{at}: refused for another reason: {}",
                error.message
            ),
            Ok(_) => assert_eq!(
                cell("SELECT @@global.transaction_isolation"),
                level,
                "{at}: the server-wide isolation level moved"
            ),
        }
        refused();
        reset();
        // The same level for this session alone is a read and leaves the same
        // canary refused.
        leaves_the_hold(
            "a session isolation level",
            &[],
            &format!("SET SESSION TRANSACTION ISOLATION LEVEL {spelled}"),
        );
    }

    // The fixture is as it was found.
    assert_eq!(
        subject.read(scenario).rows.len(),
        EXPECTED.len(),
        "{at}: the fixture does not hold its original rows"
    );
}

/// The two servers of the MySQL dialect disagree about a MariaDB-only
/// executable comment (`/*M!100100 … */`): one runs what it holds, the other
/// reads a comment. The classifier cannot know which it is talking to, so it
/// rates what the executing one would run, and on MySQL also calls the
/// statement unreadable. This scenario pins both halves: the executing server
/// really does the destructive thing and really does disarm the hold, and the
/// other really does neither.
///
/// Which one this is is asked of the server, not of the engine: `SELECT 2
/// /*M!100100 +1 */` answers 3 from one and 2 from the other. Every positive
/// has a control spelled as a plain comment that leaves the same object, or the
/// same refused write, untouched on both; it is rated a read on MariaDB and
/// unreadable on MySQL.
pub(super) fn executable_comments_agree_with_the_server(subject: &Subject) {
    let scenario = "executable_comments_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    let schema = subject.schema;
    // Row 972 and one helper table, both named for this scenario: CI runs four
    // scenarios at a time against one server. The shared fixture table is never
    // the target of a statement that drops anything.
    let canary = 972;
    let table = engine.qualified(schema, "dbdelve_comments_972");
    let create = format!("CREATE TABLE {table} (id INT)");
    let delete = sql_delete(engine, schema, canary);
    let insert = sql_insert(engine, schema, canary, "executable comment");
    let unwind = vec![delete.clone(), format!("DROP TABLE IF EXISTS {table}")];
    // `Cleanup` runs on a panic but not on a kill, so a stopped run leaves the
    // helper table and the canary row behind, and the `CREATE TABLE` below
    // then fails on a duplicate name as though the classifier were wrong. The
    // unwind list is the cleanup's own, read once here before anything is
    // created; errors are ignored because on a clean server there is nothing
    // to remove.
    for statement in &unwind {
        let _ = subject
            .connection
            .query(statement, &CancelToken::default(), Fetch::default());
    }
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: unwind,
        rollback: true,
        read_write: true,
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    // The effect is what is asserted on, and a server that ignores a comment
    // answers a submission of nothing else with an error of its own.
    let send = |sql: &str| {
        let _ = subject
            .connection
            .query(sql, &CancelToken::default(), Fetch::default());
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let cell = |sql: &str| {
        run(sql)
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: `{sql}` answered nothing"))
            .to_string()
    };
    let table_exists = || {
        let found = cell(&format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = {} AND table_name = 'dbdelve_comments_972'",
            engine.quote_literal(schema)
        ));
        found == "1"
    };
    let refused = || {
        let error = subject
            .connection
            .query(&insert, &CancelToken::default(), Fetch::default())
            .expect_err(&format!("{at}: the hold took a write it should refuse"));
        assert!(
            error.message.contains(subject.read_only_refusal),
            "{at}: refused for another reason: {}",
            error.message
        );
    };
    let executes = match cell("SELECT 2 /*M!100100 +1 */").as_str() {
        "3" => true,
        "2" => false,
        other => panic!("{at}: the probe answered {other}, which neither server gives"),
    };

    // On MySQL, which never runs the marker, the classifier also rates every
    // `/*M!` statement unreadable: what it holds and what MySQL runs without it
    // differ, so the human sees the statement before it goes.
    let unreadable = sql::Destructive::Unreadable;
    let verdict = |sql: &str| sql::classify(engine, sql);
    let stopped = |sql: &str| {
        let rated = verdict(sql);
        assert!(
            rated.mode > sql::Mode::ReadOnly && rated.destructive.contains(&unreadable) != executes,
            "{at}: `{sql}` is rated {rated:?}, which a Read-only profile would run"
        );
    };
    let a_read = |sql: &str| {
        let rated = verdict(sql);
        if executes {
            assert_eq!(
                rated,
                sql::Verdict::READ,
                "{at}: control `{sql}` is not rated a plain read"
            );
        } else {
            assert!(
                rated.destructive == [unreadable],
                "{at}: control `{sql}` is rated {rated:?}, not unreadable"
            );
        }
    };

    // The destructive form: rated by the kind it holds on both servers, and the
    // object is gone afterwards on the one that runs it and still there on the
    // other.
    let drop = format!("/*M!100100 DROP TABLE {table} */");
    let held = if executes {
        vec![sql::Destructive::Drop]
    } else {
        vec![sql::Destructive::Drop, unreadable]
    };
    assert_eq!(verdict(&drop).destructive, held, "{at}: `{drop}`");
    run(&create);
    send(&drop);
    assert_eq!(
        table_exists(),
        !executes,
        "{at}: the server that {} the marker {} the canary",
        if executes { "runs" } else { "ignores" },
        if executes { "left" } else { "dropped" }
    );
    if executes {
        run(&create);
    }
    // The control: four digits are no version, a plain comment to both.
    let plain = format!("/*M!1234 DROP TABLE {table} */");
    a_read(&plain);
    send(&plain);
    assert!(table_exists(), "{at}: a plain comment dropped the canary");
    run(&format!("DROP TABLE {table}"));

    // The hole this closes: with the hold armed, a comment holding the release
    // is a write the server has refused until now.
    let release = super::read_only_statement(engine, false)
        .unwrap_or_else(|| panic!("{at}: a holding engine has a statement that releases it"));
    let disarm = format!("/*M!100100 {release} */");
    stopped(&disarm);
    stopped(&format!("SELECT 1; {disarm}"));
    hold(true);
    refused();
    send(&disarm);
    if executes {
        // The write that was refused a moment ago now goes through.
        run(&insert);
    } else {
        refused();
    }
    run("ROLLBACK");
    hold(false);
    run(&delete);

    let plain = format!("/*M!1234 {release} */");
    a_read(&plain);
    hold(true);
    send(&plain);
    refused();
    run("ROLLBACK");
    hold(false);

    // The fixture is as it was found.
    assert_eq!(
        subject.read(scenario).rows.len(),
        EXPECTED.len(),
        "{at}: the fixture does not hold its original rows"
    );
}

/// The audit D-11 asks for: for each comment form, does the classifier ever
/// skip as a comment what the server runs? The one direction that matters is
/// asserted, and only it: **if the server ran a write, a Read-only profile's
/// gate must have stopped the submission.** The reverse is harmless. A
/// classifier that stops a submission the server ignores has over-classified,
/// which is the safe side, and the flat reading of a nested block comment is
/// expected to do exactly that.
///
/// Each form is submitted two ways, with a write to a table this scenario
/// creates for itself: the write *inside* the comment, which only a server that
/// reads the form as code will run, and the write *after* a comment that holds
/// nothing, which every server runs and so shows the classifier reads past the
/// comment to the statement. A form that is no comment on an engine, such as a
/// hash on Postgres, is a syntax error there and asserts nothing.
///
/// RESULT (2026-10-26, against the compose Postgres 15, MySQL 8.4, MariaDB 11.4
/// and SQL Server): no exploitable gap beyond the executable comment plan 01-04
/// closed. Every submission the server ran was stopped by a Read-only gate. The
/// only writes the server ran from inside a comment were the executable
/// comments, `/*!50000 ... */` on MySQL and MariaDB and `/*M!100100 ... */` on
/// MariaDB, and the classifier rates both by what they hold. With that fix
/// reverted this scenario fails on MariaDB at `/*M!100100 INSERT ... */`, so it
/// finds the one real hole. Where the server reads a form as no comment at all,
/// the classifier stops it though the server ran nothing: a `--` with no space
/// and a nested block comment on MySQL and MariaDB, and a hash on Postgres and
/// SQL Server. That is the over-classification the research predicted. The
/// batch splitter and the grammar pins are untouched on this evidence.
pub(super) fn comment_forms_agree_with_the_server(subject: &Subject) {
    let scenario = "comment_forms_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.target.engine);
    let engine = subject.target.engine;
    // One helper table named for this scenario: CI runs four scenarios at a
    // time against one server, and the shared fixture is never written.
    let table = engine.qualified(subject.schema, "dbdelve_forms_973");
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![format!("DROP TABLE IF EXISTS {table}")],
        rollback: false,
        read_write: false,
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    // A form the server does not read as a comment answers with an error of its
    // own; the effect on the table is what is asserted on.
    let send = |sql: &str| {
        let _ = subject
            .connection
            .query(sql, &CancelToken::default(), Fetch::default());
    };
    let rows = || -> u32 {
        run(&format!("SELECT COUNT(*) FROM {table}"))
            .rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("{at}: the canary table answered no count"))
    };
    run(&format!("DROP TABLE IF EXISTS {table}"));
    run(&format!("CREATE TABLE {table} (id INT)"));
    let write = format!("INSERT INTO {table} VALUES (1)");

    // How a form carries a body: the text of the comment around it.
    type Comment = fn(&str) -> String;
    let forms: [(&str, Comment); 8] = [
        ("a line comment with a space", |body| format!("-- {body}\n")),
        ("a line comment without one", |body| format!("--{body}\n")),
        ("a hash line comment", |body| format!("# {body}\n")),
        ("a block comment", |body| format!("/* {body} */")),
        ("a nested block comment", |body| {
            format!("/* /* {body} */ */")
        }),
        ("a comment both servers execute", |body| {
            format!("/*!50000 {body} */")
        }),
        ("a comment only MariaDB executes", |body| {
            format!("/*M!100100 {body} */")
        }),
        ("a comment with a version too short to run", |body| {
            format!("/*M!1234 {body} */")
        }),
    ];
    let mut observed_a_run = false;
    for (name, comment) in forms {
        for (shape, sql) in [
            ("write inside", comment(&write)),
            ("write after", format!("{}{write}", comment("a note"))),
        ] {
            send(&sql);
            let ran = rows() > 0;
            run(&format!("DELETE FROM {table}"));
            if !ran {
                continue;
            }
            observed_a_run = true;
            let verdict = sql::classify(engine, &sql);
            assert!(
                sql::gate(&verdict, sql::Mode::ReadOnly, &[]).is_some(),
                "{at}: the server ran `{sql}` ({name}, {shape}), and a Read-only profile \
                 would have run it too: rated {verdict:?}"
            );
        }
    }
    // A scenario in which nothing ever ran asserted nothing.
    assert!(observed_a_run, "{at}: no submission wrote to the canary");
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

/// A keyless draft over `schema.queue (pos, name)`, holding `(1, 'a')`,
/// `(2, 'b')` and `(3, 'b')`, that deletes position 1 and then renames the
/// row read at position 2. Where a trigger closes the gap the delete leaves,
/// the row read at 3 takes exactly the values the rename names, and the apply
/// must refuse the draft before it writes anything
/// ([`super::KEYLESS_AFTER_A_WRITE`]); on the table alone it applies.
/// `create` builds the table, first, and its trigger in the engine's own
/// words.
pub(super) fn keyless_edit_after_a_compacting_delete(
    connection: &Connection,
    engine: Engine,
    schema: &str,
    create: &[&str],
) {
    let table = engine.qualified(schema, "queue");
    let run = |sql: &str| {
        connection
            .query(sql, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{engine:?}: `{sql}` failed: {error}"))
    };
    let _ = connection.query(
        &format!("DROP TABLE {table}"),
        &CancelToken::default(),
        Fetch::default(),
    );
    for sql in create {
        run(sql);
    }
    run(&format!(
        "INSERT INTO {table} (pos, name) VALUES (1, 'a'), (2, 'b'), (3, 'b')"
    ));
    let (pos, name) = (
        engine.quote_identifier("pos"),
        engine.quote_identifier("name"),
    );
    let step = |kind, sql: String, at: &str, was: &str| VerifiedStep {
        reread: Some(
            sql::row_reread(engine, &sql)
                .unwrap_or_else(|| panic!("{engine:?}: no re-read of `{sql}`")),
        ),
        sql,
        kind,
        before: [("pos", at), ("name", was)]
            .iter()
            .map(|(column, text)| FetchedCell {
                column: (*column).into(),
                text: Some((*text).into()),
                cell_type: None,
                column_type: None,
            })
            .collect(),
        read_back: None,
        by_key: false,
    };
    let steps = [
        step(
            StepKind::Delete,
            format!("DELETE FROM {table} WHERE {pos} = '1' AND {name} = 'a'"),
            "1",
            "a",
        ),
        step(
            StepKind::Update,
            format!("UPDATE {table} SET {name} = 'c' WHERE {pos} = '2' AND {name} = 'b'"),
            "2",
            "b",
        ),
    ];

    let outcome = connection.apply_verified(&steps, &CancelToken::default());

    let left = run(&format!("SELECT pos, name FROM {table} ORDER BY pos"));
    run(&format!("DROP TABLE {table}"));
    assert_eq!(
        outcome,
        ApplyOutcome::Failed {
            stage: ApplyStage::Begin,
            error: super::plain_error(super::KEYLESS_AFTER_A_WRITE.into()),
        },
        "{engine:?}"
    );
    let left: Vec<_> = left
        .rows
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row.get(0).unwrap_or("?"),
                row.get(1).unwrap_or("?")
            )
        })
        .collect();
    assert_eq!(
        left,
        ["1:a", "2:b", "3:b"],
        "{engine:?}: nothing was written"
    );

    // The same draft on the table alone, which nothing rewrites, applies: the
    // refusal above was the trigger's, not every keyless draft's.
    run(create[0]);
    run(&format!(
        "INSERT INTO {table} (pos, name) VALUES (1, 'a'), (2, 'b'), (3, 'b')"
    ));
    let outcome = connection.apply_verified(&steps, &CancelToken::default());
    run(&format!("DROP TABLE {table}"));
    assert!(
        matches!(outcome, ApplyOutcome::Applied { affected: 2, .. }),
        "{engine:?}: {outcome:?}"
    );
}

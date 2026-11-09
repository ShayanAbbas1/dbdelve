//! Postgres through psqlODBC and the ODBC bridge, speaking the Postgres
//! dialect: the subject with a transaction `BEGIN` opens and a Read-only hold
//! that is a session statement, which Db2 through `Generic` can express
//! neither of.
//!
//! The tests read `dbdelve_ODBC_POSTGRES`, a connection string without
//! credentials that names the driver by path (`DRIVER=/abs/path/psqlodbcw.so`)
//! or by the name `/etc/odbcinst.ini` registers, and `dbdelve_ODBC_POSTGRES_USER`
//! and `dbdelve_ODBC_POSTGRES_PASSWORD`. The bridge's own unixODBC reads no
//! Homebrew configuration, so on macOS use the path, or point `ODBCSYSINI` at a
//! directory that registers the name.

use std::env;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use super::echo;
use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
use crate::db::plugin::Plugin;
use crate::db::target::Driver;
use crate::db::{
    CancelToken, Connection, ConnectionConfig, Engine, ExternalConfig, Fetch, QueryResult, Target,
    is_cancel,
};
use crate::result_grid::{NewValue, PendingRow};

/// A finite read, since the bridge drains a capped read to its end rather than
/// ending it early.
const MILLION: &str = "SELECT generate_series(1, 1000000) AS n";

fn bridge() -> &'static Plugin {
    static PLUGIN: OnceLock<&'static Plugin> = OnceLock::new();
    PLUGIN.get_or_init(|| echo::built("dbdelve-plugin-odbc", "dbdelve-odbc", "odbc", "ODBC"))
}

fn target() -> Target {
    Target {
        engine: Engine::Postgres,
        driver: Driver::Plugin(bridge()),
    }
}

fn config() -> ExternalConfig {
    ExternalConfig {
        plugin: bridge(),
        speaks: Engine::Postgres,
        // Pinned so the scenarios see the server's behaviour: a result is read
        // whole, not through a cursor psqlODBC would declare inside a
        // transaction of its own.
        connection: format!(
            "{};UseDeclareFetch=0",
            env::var("dbdelve_ODBC_POSTGRES").expect("dbdelve_ODBC_POSTGRES")
        ),
        user: env::var("dbdelve_ODBC_POSTGRES_USER").unwrap_or_default(),
        password: env::var("dbdelve_ODBC_POSTGRES_PASSWORD").unwrap_or_default(),
        statement_timeout: 0,
        ssh: None,
    }
}

fn open_as(config: ExternalConfig) -> Connection {
    Connection::open(ConnectionConfig::External(config)).expect("the bridge opens on Postgres")
}

fn open() -> Connection {
    open_as(config())
}

fn run(connection: &Connection, sql: &str) -> QueryResult {
    connection
        .query(sql, &CancelToken::default(), Fetch::default())
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"))
}

fn conformance_subject() -> Subject {
    speaking(Engine::Postgres)
}

fn speaking(speaks: Engine) -> Subject {
    Subject {
        connection: open_as(ExternalConfig { speaks, ..config() }),
        target: Target {
            engine: speaks,
            ..target()
        },
        schema: "public",
        relation: "conformance",
        key: "id",
        columns: &COLUMNS,
        read: sql_read(speaks, "public"),
        endless_rows: Some(1_000_000),
        endless: MILLION,
        runaway: "SELECT pg_sleep(30)",
        read_only_refusal: "read-only transaction",
        fixture: Some(Fixture {
            types: &[
                "int4",
                "text",
                "int8",
                "numeric",
                "float8",
                "bool",
                "date",
                "timestamp without time zone",
                "bytea",
            ],
            renders: &[],
            find: Box::new(move |key| sql_find(speaks, "public", key)),
        }),
        _guard: None,
    }
}

crate::db::conformance::suite!(
    live_conformance,
    "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD",
    conformance_subject:
    connects,
    query_round_trip,
    catalog_round_trip,
    structure_round_trip,
    types_render_as_expected,
    cancel_stops_a_long_query,
    row_limit_stops_an_eligible_read,
    row_limit_drains_an_ineligible_read,
    capped_read_keeps_the_users_transaction,
    read_only_session_refuses_a_write,
    generated_reads_run,
    generated_writes_round_trip,
);

/// The same server spoken to as Generic SQL, so the dialect Db2 is edited in
/// writes through a real driver wherever CI runs psqlODBC. On a table of the
/// test's own, since the Postgres subject's round trip writes the same id into
/// `conformance` at the same time.
#[test]
#[ignore = "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_psqlodbc_generic_sql_writes_rows_through_one_transaction() {
    let subject = Subject {
        relation: "dbdelve_odbc_generic",
        fixture: Some(Fixture {
            types: &[],
            renders: &[],
            find: Box::new(|key| {
                format!("SELECT label FROM public.dbdelve_odbc_generic WHERE id = {key}")
            }),
        }),
        ..speaking(Engine::Generic)
    };
    let connection = &subject.connection;
    run(
        connection,
        "DROP TABLE IF EXISTS public.dbdelve_odbc_generic",
    );
    run(
        connection,
        "CREATE TABLE public.dbdelve_odbc_generic (id integer PRIMARY KEY, label varchar(40) NOT NULL)",
    );
    run(
        connection,
        "INSERT INTO public.dbdelve_odbc_generic VALUES (1, 'one'), (2, 'two')",
    );

    crate::db::conformance::generated_writes_round_trip(&subject);

    // Generic SQL looks a bare name up folded both ways; Postgres holds only
    // the lower.
    let typed = run(
        connection,
        "SELECT ID, Label FROM Public.Dbdelve_Odbc_Generic",
    );
    let edit = typed
        .edit
        .unwrap_or_else(|| panic!("read-only: {:?}", typed.read_only_reason));
    assert_eq!(
        (edit.schema.as_str(), edit.table.as_str()),
        ("public", "dbdelve_odbc_generic")
    );
    assert_eq!(edit.columns, [Some("id".into()), Some("label".into())]);
    let bare = run(connection, "SELECT * FROM dbdelve_odbc_generic");
    assert!(bare.edit.is_none());
    let reason = bare.read_only_reason.unwrap_or_default();
    assert!(
        reason.starts_with("Name the table with its schema"),
        "{reason}"
    );

    // The second row is not there, so the first one's change is rolled back.
    let rows: Vec<PendingRow> = ["1", "99"]
        .into_iter()
        .map(|id| PendingRow {
            schema: "public".into(),
            table: "dbdelve_odbc_generic".into(),
            sets: vec![("label".into(), NewValue::Value("changed".into()))],
            keys: vec![("id".into(), id.into())],
            ..PendingRow::default()
        })
        .collect();
    let batch = crate::sql::update_batch(Engine::Generic, &rows).unwrap();
    let refused = connection
        .generated(&batch, &CancelToken::default(), None, None)
        .expect_err("a write of a missing row was saved");
    assert!(
        refused.message.contains("nothing was saved"),
        "{}",
        refused.message
    );
    let label = run(
        connection,
        "SELECT label FROM public.dbdelve_odbc_generic WHERE id = 1",
    );
    assert_eq!(label.rows[0].get(0), Some("one"));
    run(connection, "DROP TABLE public.dbdelve_odbc_generic");
}

/// Postgres reads a bare `current_schema` as the function even beside a
/// column by that name, and psqlODBC reads it back as that column's type, so
/// only DBDelve's own reading stops an edit landing on the row keyed `public`.
#[test]
#[ignore = "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_psqlodbc_a_function_read_as_the_key_column_is_never_a_key() {
    let connection = open();
    run(
        &connection,
        "DROP TABLE IF EXISTS public.dbdelve_odbc_niladic",
    );
    run(
        &connection,
        "CREATE TABLE public.dbdelve_odbc_niladic (\"current_schema\" name PRIMARY KEY, label text)",
    );
    run(
        &connection,
        "INSERT INTO public.dbdelve_odbc_niladic VALUES ('victim', 'v'), ('public', 'p')",
    );
    let read = run(
        &connection,
        "SELECT current_schema, label FROM public.dbdelve_odbc_niladic \
         WHERE \"current_schema\" = 'victim'",
    );
    assert_eq!(read.rows[0].get(0), Some("public"));
    assert!(read.edit.is_none());
    let reason = read.read_only_reason.unwrap_or_default();
    assert!(reason.contains("is also a value the server"), "{reason}");
    let quoted = run(
        &connection,
        "SELECT \"current_schema\", label FROM public.dbdelve_odbc_niladic",
    );
    let edit = quoted
        .edit
        .unwrap_or_else(|| panic!("read-only: {:?}", quoted.read_only_reason));
    assert_eq!(edit.keys, [0]);
    run(&connection, "DROP TABLE public.dbdelve_odbc_niladic");
}

/// The Postgres dialect holds Read-only with a session statement; sent through
/// the bridge it must still make the server, not DBDelve, refuse the write.
#[test]
#[ignore = "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_psqlodbc_read_only_hold_is_the_servers() {
    let connection = open();
    run(&connection, "DROP TABLE IF EXISTS dbdelve_odbc_hold");
    run(&connection, "CREATE TABLE dbdelve_odbc_hold (id int)");
    connection.set_read_only(true).unwrap();
    let refused = connection.query(
        "INSERT INTO dbdelve_odbc_hold VALUES (1)",
        &CancelToken::default(),
        Fetch::default(),
    );
    connection.set_read_only(false).unwrap();
    let message = refused.expect_err("the server took a write").message;
    assert!(message.contains("read-only transaction"), "{message}");
    run(&connection, "INSERT INTO dbdelve_odbc_hold VALUES (1)");
    run(&connection, "DROP TABLE dbdelve_odbc_hold");
}

/// A timeout is the server ending the statement, not the user's Cancel, and
/// the session it happens on carries on.
#[test]
#[ignore = "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_psqlodbc_a_statement_timeout_leaves_the_session_open() {
    let connection = open_as(ExternalConfig {
        statement_timeout: 1,
        ..config()
    });
    let started = Instant::now();
    let error = connection
        .query(
            "SELECT pg_sleep(30)",
            &CancelToken::default(),
            Fetch::default(),
        )
        .expect_err("the timeout should have ended the statement");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!is_cancel(target(), &error), "{}", error.message);
    assert!(
        error.message.contains("statement timeout"),
        "{}",
        error.message
    );
    assert!(!connection.is_lost());
    assert_eq!(
        run(&connection, "SELECT 1 AS one").rows[0].get(0),
        Some("1")
    );
}

/// `nextval` is not undone by anything and each run of the statement takes one,
/// so the sequence counts how many times the server saw it.
#[test]
#[ignore = "needs a Postgres and psqlODBC: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_psqlodbc_a_killed_bridge_is_a_reset_and_the_statement_is_not_replayed() {
    let observer = open();
    run(&observer, "DROP SEQUENCE IF EXISTS dbdelve_odbc_replay");
    run(&observer, "CREATE SEQUENCE dbdelve_odbc_replay");
    let runs = || {
        let sql = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM dbdelve_odbc_replay";
        run(&observer, sql).rows[0].get(0).map(str::to_string)
    };

    let connection = super::Connection::open(&config()).unwrap();
    let running = {
        let connection = connection.clone();
        thread::spawn(move || {
            connection.query(
                "SELECT nextval('dbdelve_odbc_replay'), pg_sleep(30)",
                &CancelToken::default(),
                Fetch::default(),
            )
        })
    };
    let started = Instant::now();
    while runs().as_deref() != Some("1") {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the statement never reached the server"
        );
    }

    connection.host.kill();
    let error = running
        .join()
        .unwrap()
        .expect_err("the kill should have ended the statement");
    assert!(
        error
            .message
            .contains("The connection was reset: any open transaction"),
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .ends_with("Whether the statement that was running committed is unknown."),
        "{}",
        error.message
    );
    assert!(connection.is_lost());
    let again = connection
        .query("SELECT 1", &CancelToken::default(), Fetch::default())
        .expect_err("a reset session should stay reset");
    assert!(
        again.message.contains("The connection was reset"),
        "{}",
        again.message
    );
    assert_eq!(runs().as_deref(), Some("1"));

    // The server has not noticed its client went, and the sleep's backend
    // holds a lock the DROP would wait out.
    run(
        &observer,
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE query LIKE 'SELECT nextval(''dbdelve_odbc_replay''%'",
    );
    run(&observer, "DROP SEQUENCE dbdelve_odbc_replay");
}

/// The compose database as the bastion sees it, by service name and the port
/// inside the network. The direct attempt proves this machine cannot reach it,
/// so only a string the tunnel rewrote to its forward connects.
#[test]
#[ignore = "needs psqlODBC and the dev bastion: set dbdelve_ODBC_POSTGRES, _USER, _PASSWORD and dbdelve_SSH_CONFIG"]
fn live_external_through_a_tunnel() {
    let base = config();
    let connection = echo::with_endpoint(&base.connection, "Server=postgres;Port=5432");
    let direct = ConnectionConfig::External(ExternalConfig {
        connection: connection.clone(),
        ..config()
    });
    assert!(
        Connection::open(direct).is_err(),
        "the endpoint must be out of reach without the tunnel"
    );
    let connection = open_as(ExternalConfig {
        connection,
        ssh: crate::db::ssh::live_bastion("dbdelve-bastion"),
        ..base
    });
    let result = run(&connection, "SELECT 1 AS one");
    assert_eq!(result.rows[0].get(0), Some("1"));
}

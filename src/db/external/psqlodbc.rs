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
    Subject {
        connection: open(),
        target: target(),
        schema: "public",
        relation: "conformance",
        key: "id",
        columns: &COLUMNS,
        read: sql_read(Engine::Postgres, "public"),
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
            find: Box::new(|key| sql_find(Engine::Postgres, "public", key)),
        }),
        _guard: None,
    }
}

// Not taken: generated_writes_round_trip, because a plugin target has no
// write ability.
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
);

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

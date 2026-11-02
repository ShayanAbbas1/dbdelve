//! Postgres through psqlODBC and the ODBC bridge, speaking the Postgres
//! dialect: the subject with a transaction `BEGIN` opens and a Read-only hold
//! that is a session statement, which Db2 through `Generic` can express
//! neither of.
//!
//! The tests read `dbdelve_ODBC_POSTGRES`, a connection string without
//! credentials that names the driver by path (`DRIVER=/abs/path/psqlodbcw.so`)
//! or by the name `/etc/odbcinst.ini` registers, and `dbdelve_ODBC_POSTGRES_USER`
//! and `dbdelve_ODBC_POSTGRES_PASSWORD`. The bridge's own unixODBC reads no
//! Homebrew configuration, so on macOS the path is the only route.

use std::env;
use std::sync::OnceLock;

use super::echo;
use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
use crate::db::plugin::Plugin;
use crate::db::target::Driver;
use crate::db::{
    CancelToken, Connection, ConnectionConfig, Engine, ExternalConfig, Fetch, QueryResult, Target,
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

fn open() -> Connection {
    let config = ExternalConfig {
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
    };
    Connection::open(ConnectionConfig::External(config)).expect("the bridge opens on Postgres")
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

//! IBM Db2 through the ODBC bridge, as `Generic` SQL: the acceptance subject
//! for plugin drivers, run on the real bridge over its real pipes.
//!
//! The tests read `dbdelve_ODBC_DB2`, a connection string without
//! credentials naming the driver by path (`dev/db2/driver.sh` prints its
//! `DRIVER=`), and `dbdelve_ODBC_DB2_USER` and `dbdelve_ODBC_DB2_PASSWORD`.

use std::env;
use std::sync::OnceLock;

use super::echo;
use crate::db::conformance::{Fixture, Subject};
use crate::db::plugin::Plugin;
use crate::db::target::Driver;
use crate::db::{
    CancelToken, Connection, ConnectionConfig, Engine, ExternalConfig, Fetch, IndexKind,
    QueryResult, RelationKind, Target,
};
use crate::filter::{self, FilterBar, Operator};

/// Db2 folds the seed's unquoted names to upper case, and `Generic` quotes
/// every name it writes, so these are the names as the catalog holds them.
const COLUMNS: [&str; 9] = [
    "ID", "LABEL", "WHOLE", "FRACTION", "RATIO", "FLAG", "DAY", "MOMENT", "BYTES",
];

/// A recursive CTE Db2 runs to its end, a million rows: a drain that a bridge
/// cutting the statement short would report as fewer.
const MILLION: &str = "WITH T(N) AS (SELECT 1 FROM SYSIBM.SYSDUMMY1 UNION ALL \
                       SELECT N + 1 FROM T WHERE N < 1000000) SELECT N FROM T";

/// Two billion recursive steps folded into one row, so nothing streams and
/// only a cancel ends it inside the scenario's bound.
const RUNAWAY: &str = "WITH T(N) AS (SELECT 1 FROM SYSIBM.SYSDUMMY1 UNION ALL \
                       SELECT N + 1 FROM T WHERE N < 2000000000) SELECT COUNT(*) FROM T";

fn bridge() -> &'static Plugin {
    static PLUGIN: OnceLock<&'static Plugin> = OnceLock::new();
    PLUGIN.get_or_init(|| echo::built("dbdelve-plugin-odbc", "dbdelve-odbc", "odbc", "ODBC"))
}

fn target() -> Target {
    Target {
        engine: Engine::Generic,
        driver: Driver::Plugin(bridge()),
    }
}

fn user() -> String {
    env::var("dbdelve_ODBC_DB2_USER").unwrap_or_default()
}

fn schema() -> &'static str {
    static SCHEMA: OnceLock<String> = OnceLock::new();
    SCHEMA.get_or_init(|| user().to_uppercase())
}

fn config() -> ExternalConfig {
    ExternalConfig {
        plugin: bridge(),
        speaks: Engine::Generic,
        connection: env::var("dbdelve_ODBC_DB2").expect("dbdelve_ODBC_DB2"),
        user: user(),
        password: env::var("dbdelve_ODBC_DB2_PASSWORD").unwrap_or_default(),
        statement_timeout: 0,
        ssh: None,
    }
}

fn open_as(config: ExternalConfig) -> Connection {
    Connection::open(ConnectionConfig::External(config)).expect("the bridge opens on Db2")
}

fn open() -> Connection {
    open_as(config())
}

fn quoted(name: &str) -> String {
    Engine::Generic.quote_identifier(name)
}

fn conformance() -> String {
    Engine::Generic.qualified(schema(), "CONFORMANCE")
}

fn run(connection: &Connection, sql: &str) -> QueryResult {
    connection
        .query(sql, &CancelToken::default(), Fetch::default())
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"))
}

fn cell(result: &QueryResult) -> Option<&str> {
    result.rows.first().and_then(|row| row.get(0))
}

fn conformance_subject() -> Subject {
    let columns: Vec<String> = COLUMNS.iter().map(|column| quoted(column)).collect();
    Subject {
        connection: open(),
        target: target(),
        schema: schema(),
        relation: "CONFORMANCE",
        key: "ID",
        columns: &COLUMNS,
        read: format!(
            "SELECT {} FROM {} WHERE {} < 900 ORDER BY {}",
            columns.join(", "),
            conformance(),
            quoted("ID"),
            quoted("ID"),
        ),
        endless_rows: Some(1_000_000),
        endless: MILLION,
        runaway: RUNAWAY,
        // Generic holds nothing, so the scenario takes its no-op form.
        read_only_refusal: "",
        fixture: Some(Fixture {
            types: &[
                "INTEGER",
                "VARCHAR",
                "BIGINT",
                "DECIMAL",
                "DOUBLE",
                "BOOLEAN",
                "DATE",
                "TIMESTAMP",
                "VARBINARY",
            ],
            // Db2's own character form of a DOUBLE, converted by its driver.
            renders: &[
                ("RATIO", 0, "5.00000000000000E-001"),
                ("RATIO", 1, "-1.25000000000000E+000"),
            ],
            find: Box::new(|key| {
                format!(
                    "SELECT {} FROM {} WHERE {} = {key}",
                    quoted("LABEL"),
                    conformance(),
                    quoted("ID"),
                )
            }),
        }),
        _guard: None,
    }
}

// Not taken, each for its reason: capped_read_keeps_the_users_transaction,
// because the bridge leaves autocommit on and no Db2 statement opens a
// transaction, so there is none for a capped read to keep (echo's
// `a_capped_read_keeps_the_users_transaction` covers the host's side);
// generated_writes_round_trip, because a plugin target has no write ability.
crate::db::conformance::suite!(
    live_conformance,
    "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD",
    conformance_subject:
    connects,
    query_round_trip,
    catalog_round_trip,
    structure_round_trip,
    types_render_as_expected,
    cancel_stops_a_long_query,
    row_limit_stops_an_eligible_read,
    row_limit_drains_an_ineligible_read,
    read_only_session_refuses_a_write,
    generated_reads_run,
);

/// The bridge never ends a read early, so the cap is reached by draining the
/// whole statement on the bridge's side, and the session runs on after it.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_db2_a_capped_read_drains_the_whole_statement() {
    let connection = open();
    assert!(!connection.stops_at_limit());
    let capped = connection
        .query(
            MILLION,
            &CancelToken::default(),
            Fetch {
                limit: Some(3),
                reads_only: true,
                ..Fetch::default()
            },
        )
        .unwrap();
    assert_eq!(capped.rows.len(), 3);
    assert_eq!(capped.stopped, None);
    assert_eq!(capped.capped_from, Some(1_000_000));
    let after = run(&connection, "SELECT 7 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(cell(&after), Some("7"));
}

/// Escape scanning is off, so an ODBC escape reaches Db2 as typed rather than
/// as the driver's rewrite of it: unchanged inside a literal, and outside one
/// a syntax error, where a scanning driver would answer `A`. And the DBMS name
/// the driver reports suggests no built-in dialect.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_db2_statements_reach_the_server_as_typed() {
    let connection = open();
    let echoed = run(
        &connection,
        "SELECT '{fn UCASE(''a'')}' AS ESCAPED FROM SYSIBM.SYSDUMMY1",
    );
    assert_eq!(cell(&echoed), Some("{fn UCASE('a')}"));
    let refused = connection.query(
        "SELECT {fn UCASE('a')} FROM SYSIBM.SYSDUMMY1",
        &CancelToken::default(),
        Fetch::default(),
    );
    let message = refused.expect_err("the driver rewrote the escape").message;
    assert!(message.contains("SQL0104N"), "{message}");
    let dbms = connection.reported_dbms().unwrap();
    assert!(dbms.starts_with("DB2/"), "{dbms}");
    assert_eq!(Engine::from_dbms_name(dbms), None);
}

/// A composite key declared against its column order, an index, a
/// two-column foreign key and a one-column one seen from the side it points
/// at, as the host hands them to the explorer.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_db2_structure_reads_a_composite_key_an_index_and_foreign_keys() {
    let connection = open();
    for sql in [
        "DROP TABLE IF EXISTS DBDELVE_HOST_LEAF",
        "DROP TABLE IF EXISTS DBDELVE_HOST_CHILD",
        "DROP TABLE IF EXISTS DBDELVE_HOST_PARENT",
        "CREATE TABLE DBDELVE_HOST_PARENT (A INTEGER NOT NULL, B VARCHAR(20) NOT NULL, \
         PRIMARY KEY (B, A))",
        "CREATE TABLE DBDELVE_HOST_CHILD (ID INTEGER NOT NULL PRIMARY KEY, PA INTEGER, \
         PB VARCHAR(20), NOTE VARCHAR(10), \
         FOREIGN KEY (PB, PA) REFERENCES DBDELVE_HOST_PARENT (B, A))",
        "CREATE INDEX DBDELVE_HOST_CHILD_NOTE ON DBDELVE_HOST_CHILD (NOTE)",
        "CREATE TABLE DBDELVE_HOST_LEAF (ID INTEGER NOT NULL PRIMARY KEY, \
         CHILD INTEGER REFERENCES DBDELVE_HOST_CHILD (ID))",
    ] {
        run(&connection, sql);
    }
    let schema = schema();

    let catalog = connection.catalog().unwrap();
    let relations = &catalog
        .schemas
        .iter()
        .find(|listed| listed.name == schema)
        .expect("the user's schema")
        .relations;
    let kind = |name: &str| {
        relations
            .iter()
            .find(|relation| relation.name == name)
            .map(|relation| relation.kind)
    };
    assert_eq!(kind("DBDELVE_HOST_PARENT"), Some(RelationKind::Table));
    assert_eq!(kind("RICH_ACCOUNTS"), Some(RelationKind::View));
    assert_eq!(kind("ORDER_TOTALS"), Some(RelationKind::Table));

    let parent = connection.structure(schema, "DBDELVE_HOST_PARENT").unwrap();
    assert_eq!(parent.primary_key(), ["B", "A"]);
    let primary: Vec<_> = parent
        .indexes
        .iter()
        .filter(|index| index.kind == IndexKind::Primary)
        .collect();
    assert_eq!(primary.len(), 1, "{:?}", parent.indexes);
    assert_eq!(primary[0].columns, ["B", "A"]);

    let child = connection.structure(schema, "DBDELVE_HOST_CHILD").unwrap();
    let note = child
        .indexes
        .iter()
        .find(|index| index.name == "DBDELVE_HOST_CHILD_NOTE")
        .expect("the NOTE index");
    assert_eq!(
        (note.kind, note.columns.as_slice()),
        (IndexKind::Plain, &["NOTE".to_string()][..])
    );
    let keys: Vec<_> = child
        .foreign_keys
        .iter()
        .map(|key| {
            (
                key.column.as_str(),
                key.referenced_schema.as_str(),
                key.referenced_table.as_str(),
                key.referenced_column.as_str(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("PB", schema, "DBDELVE_HOST_PARENT", "B"),
            ("PA", schema, "DBDELVE_HOST_PARENT", "A"),
        ]
    );

    let referrers = |relation: &str| -> Vec<(String, String, String)> {
        connection
            .references(schema, relation)
            .unwrap()
            .into_iter()
            .map(|reference| {
                (
                    reference.table,
                    reference.column,
                    reference.referenced_column,
                )
            })
            .collect()
    };
    let leaf = (
        "DBDELVE_HOST_LEAF".to_string(),
        "CHILD".to_string(),
        "ID".to_string(),
    );
    assert_eq!(referrers("DBDELVE_HOST_CHILD"), [leaf]);
}

/// A plugin target is the read-only tier: nothing DBDelve offers writes
/// through it, and no result it returns carries anything to edit. A write the
/// user types still runs as typed, and its count crosses the driver's SQLLEN
/// intact, which IBM's macOS driver writes only half of.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_db2_offers_no_write_and_counts_a_typed_one() {
    let target = target();
    assert!(!target.edits_rows());
    assert!(!target.edits_query_rows());
    assert!(!target.takes_inserts(RelationKind::Table));
    assert!(!target.takes_imports(RelationKind::Table));
    assert!(!target.copies_ddl());

    let connection = open();
    let preview = connection
        .generated(
            &crate::explorer::preview_sql(
                Engine::Generic,
                schema(),
                "CONFORMANCE",
                "",
                Some(10),
                0,
            ),
            &CancelToken::default(),
            None,
            None,
        )
        .unwrap();
    assert!(preview.edit.is_none());

    let scratch = Engine::Generic.qualified(schema(), "SCRATCH");
    let delete = format!("DELETE FROM {scratch} WHERE {} = 970", quoted("ID"));
    run(&connection, &delete);
    let inserted = run(
        &connection,
        &format!(
            "INSERT INTO {scratch} ({}, {}) VALUES (970, 'counted')",
            quoted("ID"),
            quoted("LABEL")
        ),
    );
    assert_eq!(inserted.rows_affected, Some(1));
    assert_eq!(run(&connection, &delete).rows_affected, Some(1));
}

/// Db2 has no default `LIKE` escape, which is why `LikePlain` writes none and
/// refuses a value holding a wildcard or an escape: the backslash in `\%`
/// matches itself here, where Postgres and MySQL read it as an escape.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_db2_reads_a_backslash_in_like_as_itself_so_plain_like_refuses_one() {
    let connection = open();
    let matches = |value: &str| {
        let sql = format!("SELECT 1 FROM SYSIBM.SYSDUMMY1 WHERE '{value}' LIKE '%\\%%'");
        run(&connection, &sql).rows.len()
    };
    assert_eq!(matches("50%"), 0);
    assert_eq!(matches("a\\b"), 1);

    for value in ["50%", "a_b", "a\\b", "[ab]"] {
        for operator in [
            Operator::Contains,
            Operator::NotContains,
            Operator::StartsWith,
            Operator::EndsWith,
        ] {
            assert_eq!(
                filter::filter_predicate(
                    Engine::Generic,
                    "LABEL",
                    Some("VARCHAR"),
                    operator,
                    value
                ),
                None,
                "{operator:?} `{value}`"
            );
            let bar = FilterBar {
                column: Some("LABEL".to_string()),
                operator,
                value: value.to_string(),
                ..FilterBar::default()
            };
            assert!(
                filter::refusal(Engine::Generic, &[bar]).is_some(),
                "{operator:?} `{value}`"
            );
        }
    }
}

/// Db2's own `HOSTNAME` and `PORT` keys through the tunnel rewrite, to the
/// compose service as the bastion sees it. The direct attempt proves this
/// machine cannot reach it. Local only: CI's Db2 job runs no bastion.
#[test]
#[ignore = "needs a Db2, IBM's driver and the dev bastion: set dbdelve_ODBC_DB2, _USER, _PASSWORD and dbdelve_SSH_CONFIG"]
fn live_db2_through_a_tunnel() {
    let base = config();
    let connection = echo::with_endpoint(&base.connection, "HOSTNAME=db2;PORT=50000");
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
    let result = run(&connection, "SELECT 7 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(cell(&result), Some("7"));
}

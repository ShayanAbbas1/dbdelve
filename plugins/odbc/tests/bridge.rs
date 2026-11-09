//! The built bridge, driven over its real pipes the way the host drives it.
//!
//! The live tests read `dbdelve_ODBC_DB2`, a connection string without
//! credentials such as
//! `DRIVER=/abs/path/libdb2.dylib;HOSTNAME=127.0.0.1;PORT=55000;DATABASE=dbdelve;PROTOCOL=TCPIP`,
//! and `dbdelve_ODBC_DB2_USER` and `dbdelve_ODBC_DB2_PASSWORD`, which travel
//! in their own fields as the app sends them.

use std::collections::BTreeMap;
use std::env;
use std::io::Write;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use arrow_array::{Array, BinaryArray, StringArray};
use arrow_schema::SchemaRef;
use dbdelve_plugin_protocol::arrow::{decode_batch, decode_schema};
use dbdelve_plugin_protocol::limits::{CANCEL_DEADLINE, credit_cost};
use dbdelve_plugin_protocol::{
    EXACT, ErrorKind, Frame, Kind, PROTOCOL, Password, Reference, RelationKind, Request, Response,
    WRITE, read_frame, write_frame,
};

struct Bridge {
    child: Child,
    stdout: ChildStdout,
    next: u64,
}

#[derive(Debug)]
struct Outcome {
    schema: Option<SchemaRef>,
    rows: Vec<Vec<Option<String>>>,
    terminal: Response,
}

impl Bridge {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dbdelve-odbc"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdout,
            next: 1,
        }
    }

    /// Opens on the live Db2 the environment names.
    fn open(statement_timeout: u32) -> Self {
        let mut bridge = Self::spawn();
        let opened = bridge.request(Request::Open {
            fields: live_fields(),
            password: Password(env::var("dbdelve_ODBC_DB2_PASSWORD").unwrap_or_default()),
            statement_timeout,
        });
        assert!(
            matches!(opened, Response::Opened { .. }),
            "opened, got {opened:?}"
        );
        bridge
    }

    /// Opens on the live Postgres the environment names, through psqlODBC.
    fn open_postgres() -> Self {
        let user = env::var("dbdelve_ODBC_POSTGRES_USER").unwrap_or_default();
        let password = env::var("dbdelve_ODBC_POSTGRES_PASSWORD").unwrap_or_default();
        let mut bridge = Self::spawn();
        let opened = bridge.open_postgres_as(&user, &password);
        assert!(
            matches!(opened, Response::Opened { .. }),
            "opened, got {opened:?}"
        );
        bridge
    }

    fn open_postgres_as(&mut self, user: &str, password: &str) -> Response {
        let connection = env::var("dbdelve_ODBC_POSTGRES").expect("dbdelve_ODBC_POSTGRES");
        self.open_postgres_with(&connection, user, password)
    }

    fn open_postgres_with(&mut self, connection: &str, user: &str, password: &str) -> Response {
        let connection = connection.to_string();
        self.request(Request::Open {
            fields: BTreeMap::from([
                ("connection".to_string(), connection),
                ("user".to_string(), user.to_string()),
            ]),
            password: Password(password.to_string()),
            statement_timeout: 0,
        })
    }

    fn send(&mut self, id: u64, request: Request) {
        let stdin = self.child.stdin.as_mut().unwrap();
        write_frame(stdin, Kind::Control, id, &request.to_payload()).unwrap();
        stdin.flush().unwrap();
    }

    fn receive(&mut self) -> Frame {
        read_frame(&mut self.stdout)
            .unwrap()
            .expect("a frame before EOF")
    }

    fn request(&mut self, request: Request) -> Response {
        let id = self.next;
        self.next += 1;
        self.send(id, request);
        let frame = self.receive();
        assert_eq!((frame.kind, frame.request), (Kind::Control, id));
        Response::from_payload(&frame.payload).unwrap()
    }

    fn write(&mut self, statements: &[&str]) -> Response {
        self.request(Request::Write {
            statements: statements.iter().map(|sql| sql.to_string()).collect(),
        })
    }

    fn query(&mut self, sql: &str) -> Outcome {
        self.query_until(sql, |_| None)
    }

    /// Runs `sql` as the host does, granting each data frame's credit back
    /// once it is read. `interrupt` sees the rows kept so far and may answer
    /// a `stop` or `cancel` to send, once.
    fn query_until(
        &mut self,
        sql: &str,
        interrupt: impl FnMut(usize) -> Option<Request>,
    ) -> Outcome {
        let id = self.start(sql);
        self.collect(id, interrupt)
    }

    fn start(&mut self, sql: &str) -> u64 {
        let id = self.next;
        self.next += 1;
        self.send(
            id,
            Request::Query {
                sql: sql.to_string(),
                eligible: false,
            },
        );
        id
    }

    fn collect(&mut self, id: u64, mut interrupt: impl FnMut(usize) -> Option<Request>) -> Outcome {
        let mut schema = None;
        let mut rows = Vec::new();
        let mut interrupted = false;
        loop {
            let frame = self.receive();
            assert_eq!(frame.request, id);
            if frame.kind == Kind::Control {
                let terminal = Response::from_payload(&frame.payload).unwrap();
                return Outcome {
                    schema,
                    rows,
                    terminal,
                };
            }
            match &schema {
                None => schema = Some(decode_schema(&frame.payload).unwrap()),
                Some(schema) => {
                    let batch = decode_batch(schema, &frame.payload).unwrap();
                    for row in 0..batch.num_rows() {
                        rows.push(
                            batch
                                .columns()
                                .iter()
                                .map(|column| shown(column.as_ref(), row))
                                .collect(),
                        );
                    }
                }
            }
            assert!(!frame.payload.is_empty(), "an empty data frame");
            let bytes = credit_cost(frame.payload.len());
            self.send(id, Request::Credit { bytes });
            if !interrupted && let Some(request) = interrupt(rows.len()) {
                interrupted = true;
                self.send(id, request);
            }
        }
    }

    fn exits_within(&mut self, deadline: Duration) -> Option<i32> {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.child.kill().unwrap();
        None
    }
}

/// A cell as the host shows it: text as sent, bytes as `0x` and hex.
fn shown(column: &dyn Array, row: usize) -> Option<String> {
    if column.is_null(row) {
        return None;
    }
    if let Some(text) = column.as_any().downcast_ref::<StringArray>() {
        return Some(text.value(row).to_string());
    }
    let bytes = column.as_any().downcast_ref::<BinaryArray>().unwrap();
    let hex: String = bytes
        .value(row)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    Some(format!("0x{hex}"))
}

fn live_fields() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "connection".to_string(),
            env::var("dbdelve_ODBC_DB2").expect("dbdelve_ODBC_DB2"),
        ),
        (
            "user".to_string(),
            env::var("dbdelve_ODBC_DB2_USER").unwrap_or_default(),
        ),
    ])
}

fn ended(outcome: &Outcome) -> (u64, Option<u64>, bool) {
    match &outcome.terminal {
        Response::End {
            rows,
            rows_affected,
            stopped,
            ..
        } => (*rows, *rows_affected, *stopped),
        other => panic!("an end, got {other:?}"),
    }
}

/// A write's refusal: its message, SQLSTATE and whether it lost the session.
fn write_error(response: &Response) -> (&str, Option<&str>, bool) {
    match response {
        Response::Error {
            message,
            sqlstate,
            lost,
            ..
        } => (message.as_str(), sqlstate.as_deref(), *lost),
        other => panic!("an error, got {other:?}"),
    }
}

/// Which of a result's columns are marked exact.
fn exact_columns(outcome: &Outcome) -> Vec<bool> {
    outcome
        .schema
        .as_ref()
        .expect("a result set")
        .fields()
        .iter()
        .map(|field| {
            field
                .metadata()
                .get(EXACT)
                .is_some_and(|value| value == "true")
        })
        .collect()
}

/// A row typed after a write outlives a typed `ROLLBACK` only when the write
/// put the session back in autocommit.
fn still_autocommits(bridge: &mut Bridge, insert: &str, count: &str) {
    ended(&bridge.query(insert));
    ended(&bridge.query("ROLLBACK"));
    assert_eq!(bridge.query(count).rows, [vec![Some("1".to_string())]]);
}

fn error_of(outcome: &Outcome) -> (ErrorKind, Option<String>) {
    match &outcome.terminal {
        Response::Error { kind, sqlstate, .. } => (*kind, sqlstate.clone()),
        other => panic!("an error, got {other:?}"),
    }
}

#[test]
fn it_exits_on_stdin_eof() {
    let mut bridge = Bridge::spawn();
    let hello = bridge.request(Request::Hello { protocol: PROTOCOL });
    assert!(matches!(hello, Response::Hello { .. }));
    drop(bridge.child.stdin.take());
    assert_eq!(bridge.exits_within(Duration::from_secs(5)), Some(0));
}

/// unixODBC formats a failed driver load into a fixed buffer, which macOS's
/// long `dlopen` errors overran, killing the bridge.
#[test]
fn a_driver_that_cannot_be_loaded_is_refused_and_the_bridge_lives() {
    let long = format!("DRIVER={{{}}}", "NoSuchDriver".repeat(80));
    for connection in [
        "DRIVER=",
        "DRIVER={};Server=h",
        "DRIVER=  ;Server=h",
        "DRIVER=NoSuchDriver;Server=h",
        "Driver={No Such Driver};Server=h",
        "DRIVER=/no/such/libdriver.so;Server=h",
        "DRIVER=NoSuchDriver;DRIVER=Another",
        &long,
    ] {
        let mut bridge = Bridge::spawn();
        let refused = bridge.request(Request::Open {
            fields: BTreeMap::from([("connection".to_string(), connection.to_string())]),
            password: Password(String::new()),
            statement_timeout: 0,
        });
        let Response::Error { kind, message, .. } = &refused else {
            panic!("{connection}: an error, got {refused:?}");
        };
        assert_eq!(*kind, ErrorKind::Server, "{connection}: {message}");
        // Only macOS checks a driver name before the driver manager sees it;
        // elsewhere the system manager refuses an unknown name itself.
        assert!(
            message.starts_with("The connection string's DRIVER=")
                || (!cfg!(target_os = "macos") && message.contains("Driver Manager]")),
            "{connection}: {message}"
        );
        let hello = bridge.request(Request::Hello { protocol: PROTOCOL });
        assert!(matches!(hello, Response::Hello { .. }), "{connection}");
    }
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_open_names_the_dbms_and_close_ends_the_process() {
    let mut bridge = Bridge::spawn();
    let opened = bridge.request(Request::Open {
        fields: live_fields(),
        password: Password(env::var("dbdelve_ODBC_DB2_PASSWORD").unwrap_or_default()),
        statement_timeout: 0,
    });
    let Response::Opened {
        dbms_name,
        dbms_version,
        driver,
        stops_early,
        ..
    } = opened
    else {
        panic!("opened, got {opened:?}");
    };
    eprintln!("opened: {dbms_name} {dbms_version} through {driver}");
    assert!(dbms_name.starts_with("DB2/"), "{dbms_name}");
    assert!(!dbms_version.is_empty());
    assert!(!stops_early);

    assert_eq!(bridge.request(Request::Close), Response::Closed);
    assert_eq!(bridge.exits_within(Duration::from_secs(5)), Some(0));
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2"]
fn live_a_wrong_password_is_the_drivers_error() {
    let mut bridge = Bridge::spawn();
    let password = "not-the-password}".to_string();
    let refused = bridge.request(Request::Open {
        fields: live_fields(),
        password: Password(password.clone()),
        statement_timeout: 0,
    });
    let Response::Error {
        message, sqlstate, ..
    } = refused
    else {
        panic!("an error, got {refused:?}");
    };
    assert!(sqlstate.is_some());
    assert!(!message.contains(&password));
}

/// Db2 splits on `;` even inside braces: this password, sent, would have
/// turned TLS on, so it never reaches the driver.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2"]
fn live_a_semicolon_in_a_password_is_refused_before_the_driver() {
    let mut bridge = Bridge::spawn();
    let refused = bridge.request(Request::Open {
        fields: live_fields(),
        password: Password("a;SECURITY=SSL;b".to_string()),
        statement_timeout: 0,
    });
    let Response::Error {
        message, sqlstate, ..
    } = refused
    else {
        panic!("an error, got {refused:?}");
    };
    assert_eq!(sqlstate, None);
    assert!(message.starts_with("The password holds a `;`"), "{message}");
}

/// NULL through the SQLLEN normalisation, wide text, binary, and an escape
/// sequence left alone inside a literal.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_values_arrive_as_the_server_holds_them() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query(
        "SELECT CAST(NULL AS INTEGER) AS NOTHING, 'Zoë 李 🐉' AS TEXT, \
         CAST(X'00FF' AS VARBINARY(4)) AS BYTES, BLOB(X'00FF') AS LOB, \
         '{fn UCASE(''a'')}' AS ESCAPE, 42 AS ANSWER, \
         REPEAT(CLOB('ab'), 100000) AS LONG \
         FROM SYSIBM.SYSDUMMY1",
    );
    let schema = outcome.schema.as_ref().unwrap();
    let types: Vec<_> = schema
        .fields()
        .iter()
        .map(|field| field.metadata()["dbdelve.type"].clone())
        .collect();
    eprintln!("types: {types:?}");
    assert_eq!(types[0], "INTEGER");
    let mut rows = outcome.rows.clone();
    // Several SQLGetData chunks: past the 64 KiB one call reads.
    let long = rows[0].pop().unwrap();
    assert_eq!(long, Some("ab".repeat(100_000)));
    assert_eq!(
        rows,
        [vec![
            None,
            Some("Zoë 李 🐉".to_string()),
            Some("0x00FF".to_string()),
            Some("0x00FF".to_string()),
            Some("{fn UCASE('a')}".to_string()),
            Some("42".to_string()),
        ]]
    );
    assert_eq!(ended(&outcome), (1, None, false));
}

/// Outside a literal a scanning driver would rewrite the escape and answer
/// `A`, so the server's syntax error is what shows scanning is off.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_an_escape_reaches_the_server_as_typed() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query("SELECT {fn UCASE('a')} FROM SYSIBM.SYSDUMMY1");
    assert_eq!(
        error_of(&outcome),
        (ErrorKind::Server, Some("42601".to_string()))
    );
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_write_reports_the_rows_it_affected() {
    let mut bridge = Bridge::open(0);
    let created = bridge.query("CREATE TABLE IF NOT EXISTS DBDELVE_BRIDGE_SCRATCH (N INTEGER)");
    assert!(
        matches!(created.terminal, Response::End { .. }),
        "{created:?}"
    );
    let inserted = bridge.query("INSERT INTO DBDELVE_BRIDGE_SCRATCH VALUES (14)");
    assert_eq!(ended(&inserted), (0, Some(1), false));
    let deleted = bridge.query("DELETE FROM DBDELVE_BRIDGE_SCRATCH WHERE N = 14");
    assert_eq!(ended(&deleted), (0, Some(1), false));
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_an_error_carries_its_sqlstate_and_the_session_survives() {
    let mut bridge = Bridge::open(0);
    let failed = bridge.query("SELECT 1 / 0 FROM SYSIBM.SYSDUMMY1");
    let (kind, sqlstate) = error_of(&failed);
    eprintln!("1/0: {:?}", failed.terminal);
    assert_eq!(kind, ErrorKind::Server);
    assert!(sqlstate.is_some());
    let after = bridge.query("SELECT 1 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(after.rows, [vec![Some("1".to_string())]]);
}

/// `stop` drains: nothing past what was in flight is sent, and the count is
/// the whole statement's.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_stopped_read_is_drained_and_counted_whole() {
    const TOTAL: u64 = 200_000;
    let mut bridge = Bridge::open(0);
    let sql = format!(
        "WITH T(N) AS (SELECT 1 FROM SYSIBM.SYSDUMMY1 UNION ALL \
         SELECT N + 1 FROM T WHERE N < {TOTAL}) SELECT N FROM T"
    );
    let outcome = bridge.query_until(&sql, |kept| (kept >= 10).then_some(Request::Stop));
    let (rows, _, stopped) = ended(&outcome);
    eprintln!("kept {} of {rows}", outcome.rows.len());
    assert_eq!(rows, TOTAL);
    assert!(!stopped);
    assert!((outcome.rows.len() as u64) < TOTAL);
    if let Response::End { messages, .. } = &outcome.terminal {
        eprintln!("messages: {messages:?}");
    }
    let after = bridge.query("SELECT 2 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(after.rows, [vec![Some("2".to_string())]]);
}

/// A statement long enough to cancel, or to time out.
const LONG: &str = "WITH T(N) AS (SELECT 1 FROM SYSIBM.SYSDUMMY1 UNION ALL \
                    SELECT N + 1 FROM T WHERE N < 2000000000) SELECT COUNT(*) FROM T";

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_the_statement_timeout_ends_a_long_statement() {
    let mut bridge = Bridge::open(1);
    let start = Instant::now();
    let outcome = bridge.query(LONG);
    eprintln!(
        "timed out after {:?}: {:?}",
        start.elapsed(),
        outcome.terminal
    );
    // A timeout is the server's error, not the user's Cancel, though Db2
    // reports it as HY008.
    assert_eq!(error_of(&outcome).0, ErrorKind::Server);
    assert!(start.elapsed() < Duration::from_secs(10));
    let after = bridge.query("SELECT 3 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(after.rows, [vec![Some("3".to_string())]]);
}

/// `SQLCancel` from the bridge's stdin thread, while the worker is inside the
/// driver: if the driver manager serialised it behind the running call, the
/// answer would wait for the whole statement and the host would kill the
/// bridge at its deadline.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_cancel_interrupts_a_running_statement_and_the_session_survives() {
    let mut bridge = Bridge::open(0);
    let id = bridge.start(LONG);
    thread::sleep(Duration::from_secs(1));
    let sent = Instant::now();
    bridge.send(id, Request::Cancel);
    let outcome = bridge.collect(id, |_| None);
    let took = sent.elapsed();
    eprintln!("cancel answered after {took:?}: {:?}", outcome.terminal);
    assert_eq!(error_of(&outcome).0, ErrorKind::Cancelled);
    assert!(took < CANCEL_DEADLINE, "{took:?}");
    let after = bridge.query("SELECT 4 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(after.rows, [vec![Some("4".to_string())]]);
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_cancel_mid_stream_ends_the_read() {
    let mut bridge = Bridge::open(0);
    let sql = "WITH T(N) AS (SELECT 1 FROM SYSIBM.SYSDUMMY1 UNION ALL \
               SELECT N + 1 FROM T WHERE N < 2000000000) SELECT N FROM T";
    let outcome = bridge.query_until(sql, |kept| (kept >= 10).then_some(Request::Cancel));
    eprintln!("cancelled after {} rows", outcome.rows.len());
    assert_eq!(error_of(&outcome).0, ErrorKind::Cancelled);
    let after = bridge.query("SELECT 5 FROM SYSIBM.SYSDUMMY1");
    assert_eq!(after.rows, [vec![Some("5".to_string())]]);
}

/// Forced off the server, IBM's driver reconnects on the next statement and
/// runs it with no error, the session's state gone, so the bridge says so. The
/// compose service's container and instance owner are forced from inside.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_silent_reconnect_is_said_alongside_the_statements_result() {
    const HANDLE: &str = "SELECT MON_GET_APPLICATION_HANDLE() FROM SYSIBM.SYSDUMMY1";
    let mut bridge = Bridge::open(0);
    let before = bridge.query(HANDLE).rows[0][0].clone().unwrap();
    let forced = Command::new("docker")
        .args(["exec", "dbdelve-db2-1", "su", "-", "db2inst1", "-c"])
        .arg(format!("db2 \"force application ({before})\""))
        .output()
        .unwrap();
    assert!(forced.status.success(), "{forced:?}");
    // `force application` returns before the agent has gone.
    thread::sleep(Duration::from_secs(3));

    let after = bridge.query(HANDLE);
    let Response::End { messages, .. } = &after.terminal else {
        panic!("an end, got {:?}", after.terminal);
    };
    eprintln!("messages: {messages:?}");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("silently re-established")),
        "{messages:?}"
    );
    assert_ne!(after.rows[0][0].as_deref(), Some(before.as_str()));
    let Response::End { messages, .. } = bridge.query(HANDLE).terminal else {
        panic!("an end");
    };
    assert!(messages.is_empty(), "{messages:?}");
}

fn run_all(bridge: &mut Bridge, statements: &[&str]) {
    for sql in statements {
        let outcome = bridge.query(sql);
        assert!(
            matches!(outcome.terminal, Response::End { .. }),
            "{sql}: {:?}",
            outcome.terminal
        );
    }
}

/// `PARENT`'s key is declared (B, A), against its column order, so only
/// `KEY_SEQ` gives it right; `X_Y` sits beside `XZY`, which its unescaped
/// name would match as a pattern.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_the_catalog_reads_relations_keys_indexes_and_references() {
    let mut bridge = Bridge::open(0);
    run_all(
        &mut bridge,
        &[
            "DROP TABLE IF EXISTS DBDELVE_CAT_LEAF",
            "DROP TABLE IF EXISTS DBDELVE_CAT_CHILD",
            "DROP TABLE IF EXISTS DBDELVE_CAT_PARENT",
            "DROP TABLE IF EXISTS DBDELVE_CAT_X_Y",
            "DROP TABLE IF EXISTS DBDELVE_CAT_XZY",
            "CREATE TABLE DBDELVE_CAT_PARENT (A INTEGER NOT NULL, B VARCHAR(20) NOT NULL, \
             AMOUNT DECIMAL(10,2) DEFAULT 0, PRIMARY KEY (B, A))",
            "CREATE TABLE DBDELVE_CAT_CHILD (ID INTEGER NOT NULL PRIMARY KEY, PA INTEGER, \
             PB VARCHAR(20), NOTE VARCHAR(10), \
             FOREIGN KEY (PB, PA) REFERENCES DBDELVE_CAT_PARENT (B, A))",
            "CREATE INDEX DBDELVE_CAT_CHILD_NOTE ON DBDELVE_CAT_CHILD (NOTE)",
            "CREATE TABLE DBDELVE_CAT_LEAF (ID INTEGER NOT NULL PRIMARY KEY, \
             CHILD INTEGER REFERENCES DBDELVE_CAT_CHILD (ID))",
            "CREATE OR REPLACE VIEW DBDELVE_CAT_VIEW AS SELECT ID FROM DBDELVE_CAT_LEAF",
            "CREATE TABLE DBDELVE_CAT_X_Y (ONLY_HERE INTEGER)",
            "CREATE TABLE DBDELVE_CAT_XZY (NOT_THIS INTEGER)",
        ],
    );
    let schema = env::var("dbdelve_ODBC_DB2_USER").unwrap().to_uppercase();

    let Response::Catalog { schemas } = bridge.request(Request::Catalog) else {
        panic!("a catalog");
    };
    let relations = &schemas
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
    assert_eq!(kind("DBDELVE_CAT_PARENT"), Some(RelationKind::Table));
    assert_eq!(kind("DBDELVE_CAT_VIEW"), Some(RelationKind::View));

    let structure = |bridge: &mut Bridge, relation: &str| match bridge.request(Request::Structure {
        schema: schema.clone(),
        relation: relation.to_string(),
    }) {
        Response::Structure(structure) => structure,
        other => panic!("a structure of {relation}, got {other:?}"),
    };
    let parent = structure(&mut bridge, "DBDELVE_CAT_PARENT");
    let columns: Vec<_> = parent
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.data_type.as_str(),
                column.nullable,
            )
        })
        .collect();
    assert_eq!(
        columns,
        [
            ("A", "INTEGER", false),
            ("B", "VARCHAR(20)", false),
            ("AMOUNT", "DECIMAL(10,2)", true),
        ]
    );
    assert_eq!(parent.columns[2].default.as_deref(), Some("0"));
    assert_eq!(parent.primary_key, ["B", "A"]);
    let primary: Vec<_> = parent
        .indexes
        .iter()
        .filter(|index| index.primary)
        .collect();
    assert_eq!(primary.len(), 1, "{:?}", parent.indexes);
    assert_eq!(primary[0].columns, ["B", "A"]);

    let child = structure(&mut bridge, "DBDELVE_CAT_CHILD");
    let note = child
        .indexes
        .iter()
        .find(|index| index.name == "DBDELVE_CAT_CHILD_NOTE")
        .expect("the NOTE index");
    assert_eq!(
        (note.columns.as_slice(), note.unique),
        (&["NOTE".to_string()][..], false)
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
            ("PB", schema.as_str(), "DBDELVE_CAT_PARENT", "B"),
            ("PA", schema.as_str(), "DBDELVE_CAT_PARENT", "A"),
        ]
    );

    let narrow = structure(&mut bridge, "DBDELVE_CAT_X_Y");
    let names: Vec<_> = narrow
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["ONLY_HERE"]);

    let references =
        |bridge: &mut Bridge, relation: &str| match bridge.request(Request::References {
            schema: schema.clone(),
            relation: relation.to_string(),
        }) {
            Response::References { references } => references,
            other => panic!("references of {relation}, got {other:?}"),
        };
    assert_eq!(references(&mut bridge, "DBDELVE_CAT_PARENT"), []);
    assert_eq!(
        references(&mut bridge, "DBDELVE_CAT_CHILD"),
        [Reference {
            schema: schema.clone(),
            table: "DBDELVE_CAT_LEAF".to_string(),
            column: "CHILD".to_string(),
            referenced_column: "ID".to_string(),
        }]
    );
}

/// psqlODBC copies a braced `UID` with its braces, so a user name with spaces
/// at either end has to travel plain.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_user_with_spaces_at_either_end_logs_in() {
    let mut admin = Bridge::open_postgres();
    for user in [" dbdelve lead", "dbdelve trail "] {
        let quoted = user.replace('"', "\"\"");
        admin.query(&format!("DROP ROLE IF EXISTS \"{quoted}\""));
        let created = admin.query(&format!(
            "CREATE ROLE \"{quoted}\" LOGIN PASSWORD ' p+%41 '"
        ));
        ended(&created);
        let mut bridge = Bridge::spawn();
        let opened = bridge.open_postgres_as(user, " p+%41 ");
        admin.query(&format!("DROP ROLE IF EXISTS \"{quoted}\""));
        assert!(
            matches!(opened, Response::Opened { .. }),
            "{user:?}: {opened:?}"
        );
    }
}

/// psqlODBC percent-decodes a plain `PWD` and reads `+` as a space, so these
/// passwords have to travel braced.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_password_psqlodbc_would_decode_logs_in() {
    let mut admin = Bridge::open_postgres();
    let user = "dbdelve_odbc_password";
    let mut refused = Vec::new();
    for password in [
        "a+b", "50%off", "50%ab", "x%41y", "p{w}d", "{a+b}", "ends}", " p+%41 ",
    ] {
        admin.query(&format!("DROP ROLE IF EXISTS {user}"));
        ended(&admin.query(&format!("CREATE ROLE {user} LOGIN PASSWORD '{password}'")));
        let mut bridge = Bridge::spawn();
        let opened = bridge.open_postgres_as(user, password);
        if !matches!(opened, Response::Opened { .. }) {
            refused.push((password, opened));
        }
    }
    admin.query(&format!("DROP ROLE IF EXISTS {user}"));
    assert!(refused.is_empty(), "{refused:#?}");
}

/// Db2's text form of a `DOUBLE` keeps 15 digits, so a shown value filtered on
/// matched nothing; the binary value's shortest form matches itself.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_double_shows_every_digit_it_holds_and_filters_back_to_itself() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query(
        "SELECT CAST(1.2345678901234567E0 AS DOUBLE), \
         CAST(0.1E0 AS DOUBLE) + CAST(0.2E0 AS DOUBLE), \
         CAST(1.79769E308 AS DOUBLE), CAST(-0.1E0 AS DOUBLE), \
         CAST(1.5E0 AS REAL), CAST(1.17549E-37 AS REAL), CAST(NULL AS DOUBLE) \
         FROM SYSIBM.SYSDUMMY1",
    );
    let shown = [
        "1.2345678901234567",
        "0.30000000000000004",
        "1.79769e+308",
        "-0.1",
        "1.5",
        "1.17549e-37",
    ];
    let mut want: Vec<Option<String>> = shown.iter().map(|v| Some(v.to_string())).collect();
    want.push(None);
    assert_eq!(outcome.rows, [want]);
    for value in &shown[..4] {
        let matched = bridge.query(&format!(
            "SELECT COUNT(*) FROM (SELECT CAST(1.2345678901234567E0 AS DOUBLE) FROM SYSIBM.SYSDUMMY1 \
             UNION ALL SELECT CAST(0.1E0 AS DOUBLE) + CAST(0.2E0 AS DOUBLE) FROM SYSIBM.SYSDUMMY1 \
             UNION ALL SELECT CAST(1.79769E308 AS DOUBLE) FROM SYSIBM.SYSDUMMY1 \
             UNION ALL SELECT CAST(-0.1E0 AS DOUBLE) FROM SYSIBM.SYSDUMMY1) AS T(D) WHERE D = {value}"
        ));
        assert_eq!(matched.rows, [vec![Some("1".to_string())]], "{value}");
    }
}

#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_floats_show_as_the_server_prints_them() {
    let mut bridge = Bridge::open_postgres();
    let values = [
        "'NaN'::float8",
        "'Infinity'::float8",
        "'-Infinity'::float8",
        "1.2345678901234567::float8",
        "0.1::float8 + 0.2::float8",
        "'-0'::float8",
        "1e15::float8",
        "1e-5::float8",
        "1.17549e-37::float4",
        "16777216::float4",
    ];
    let columns: Vec<String> = values
        .iter()
        .flat_map(|value| [value.to_string(), format!("({value})::text")])
        .collect();
    let outcome = bridge.query(&format!("SELECT {}", columns.join(", ")));
    let row = &outcome.rows[0];
    for (index, value) in values.iter().enumerate() {
        assert_eq!(row[2 * index], row[2 * index + 1], "{value}");
    }
}

/// Db2's driver serializes an `XML` value with a byte-order mark and an XML
/// declaration of its own; the server keeps neither.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_xml_arrives_without_the_drivers_byte_order_mark_or_declaration() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query(
        "SELECT XMLPARSE(DOCUMENT '<a b=\"1\">é&amp;&lt;</a>'), \
         XMLPARSE(DOCUMENT '<?xml version=\"1.0\"?><a/>'), \
         XMLPARSE(DOCUMENT '<!--c--><a>&#xFEFF;x</a>'), \
         XMLSERIALIZE(XMLPARSE(DOCUMENT '<a/>') AS CLOB(1K) INCLUDING XMLDECLARATION), \
         CAST(NULL AS XML) \
         FROM SYSIBM.SYSDUMMY1",
    );
    assert_eq!(
        outcome.rows,
        [vec![
            Some("<a b=\"1\">é&amp;&lt;</a>".to_string()),
            Some("<a/>".to_string()),
            Some("<!--c--><a>\u{feff}x</a>".to_string()),
            Some("<?xml version=\"1.0\" encoding=\"UTF-8\"?><a/>".to_string()),
            None,
        ]]
    );
}

/// A binary value travels as its bytes, so the 64 MiB a frame carries is the
/// value's own size, not that of its hex.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_binary_value_may_take_the_whole_frame() {
    const MIB: usize = 1 << 20;
    let mut bridge = Bridge::open_postgres();
    let outcome = bridge.query(&format!("SELECT decode(repeat('AB', {}), 'hex')", 40 * MIB));
    let cell = outcome.rows[0][0].as_ref().expect("a value");
    assert_eq!(cell.len(), 2 + 2 * 40 * MIB);
    assert!(cell[2..].bytes().all(|b| b == b'A' || b == b'B'));
    let refused = bridge.query(&format!(
        "SELECT decode(repeat('AB', {}), 'hex') AS big",
        65 * MIB
    ));
    let Response::Error { message, .. } = &refused.terminal else {
        panic!("a refusal, got {:?}", refused.terminal);
    };
    assert_eq!(
        message,
        "A value in column big is larger than 64 MiB, which protocol 1 cannot carry."
    );
}

/// Prints rows a second for a million-row read of mixed columns, as the host
/// reads it; run with `--release --nocapture` to measure.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_read_throughput() {
    let mut bridge = Bridge::open_postgres();
    ended(&bridge.query(
        "CREATE TEMP TABLE throughput AS SELECT g AS id, 'name ' || g AS name, \
         g * 1.5::float8 AS f, timestamp '2024-01-01' + g * interval '1 second' AS ts, \
         (g % 2 = 0) AS flag, md5(g::text) AS hash, (g * 1.25)::numeric(12, 2) AS amount \
         FROM generate_series(1, 1000000) AS g",
    ));
    let started = Instant::now();
    let outcome = bridge.query("SELECT * FROM throughput");
    let elapsed = started.elapsed();
    assert_eq!(ended(&outcome).0, 1_000_000);
    assert_eq!(outcome.rows.len(), 1_000_000);
    assert_eq!(
        outcome.rows[0],
        [
            Some("1".to_string()),
            Some("name 1".to_string()),
            Some("1.5".to_string()),
            Some("2024-01-01 00:00:01".to_string()),
            Some("0".to_string()),
            Some("c4ca4238a0b923820dcc509a6f75849b".to_string()),
            Some("1.25".to_string()),
        ]
    );
    eprintln!(
        "1,000,000 rows in {elapsed:?}: {:.0} rows/s",
        1_000_000.0 / elapsed.as_secs_f64()
    );
}

/// psqlODBC reads a whole result into memory unless told to use a cursor, so
/// the bridge tells it to, which the server shows as the statement's own
/// cursor; a string that says otherwise is obeyed.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_result_streams_through_a_cursor_unless_the_string_says_otherwise() {
    let cursors = "SELECT count(*) FROM pg_cursors";
    let mut bridge = Bridge::open_postgres();
    assert_eq!(bridge.query(cursors).rows, [vec![Some("1".to_string())]]);

    let user = env::var("dbdelve_ODBC_POSTGRES_USER").unwrap_or_default();
    let password = env::var("dbdelve_ODBC_POSTGRES_PASSWORD").unwrap_or_default();
    let base = env::var("dbdelve_ODBC_POSTGRES").expect("dbdelve_ODBC_POSTGRES");
    let mut whole = Bridge::spawn();
    let opened = whole.open_postgres_with(&format!("{base};UseDeclareFetch=0"), &user, &password);
    assert!(matches!(opened, Response::Opened { .. }), "{opened:?}");
    assert_eq!(whole.query(cursors).rows, [vec![Some("0".to_string())]]);
}

/// The cursor's transaction ends with the statement, so a read leaves the
/// session idle rather than holding a transaction open.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_streamed_read_leaves_no_transaction_open() {
    let mut bridge = Bridge::open_postgres();
    let pid = bridge.query("SELECT pg_backend_pid()").rows[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        ended(&bridge.query("SELECT generate_series(1, 5000)")).0,
        5000
    );
    let mut observer = Bridge::open_postgres();
    let state = observer.query(&format!(
        "SELECT state FROM pg_stat_activity WHERE pid = {pid}"
    ));
    assert_eq!(state.rows, [vec![Some("idle".to_string())]]);
    let failed = bridge.query("SELECT 1/0");
    assert_eq!(error_of(&failed).1.as_deref(), Some("22012"));
    assert_eq!(bridge.query("SELECT 2").rows, [vec![Some("2".to_string())]]);
}

/// A value longer than the buffer its column was bound with is read again
/// whole, and the truncation the fetch warned of is no message.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_value_outgrowing_its_bound_buffer_arrives_whole() {
    let mut bridge = Bridge::open_postgres();
    let outcome = bridge.query(
        "SELECT n, repeat('x', n) AS t, repeat('é', n)::varchar(20000) AS v, \
         (n::numeric ^ 90)::numeric AS big, NULL::text AS nothing, \
         (n::numeric ^ 90)::text AS big_text \
         FROM (VALUES (1), (5000), (20000)) AS r(n) ORDER BY n",
    );
    for (row, n) in outcome.rows.iter().zip([1usize, 5000, 20000]) {
        assert_eq!(row[1].as_deref(), Some("x".repeat(n).as_str()));
        assert_eq!(row[2].as_deref(), Some("é".repeat(n).as_str()));
        assert_eq!(row[4], None);
        assert_eq!(row[3], row[5]);
    }
    assert!(outcome.rows[2][3].as_ref().unwrap().len() > 300);
    let Response::End { messages, .. } = &outcome.terminal else {
        panic!("an end, got {:?}", outcome.terminal);
    };
    assert!(messages.is_empty(), "{messages:?}");
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_value_outgrowing_its_bound_buffer_arrives_whole() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query(
        "SELECT CAST(REPEAT('x', 20000) AS VARCHAR(32000)), CAST('a' AS VARCHAR(32000)), \
         CAST(NULL AS VARCHAR(10)), CAST(1 AS INTEGER) FROM SYSIBM.SYSDUMMY1",
    );
    assert_eq!(
        outcome.rows,
        [vec![
            Some("x".repeat(20000)),
            Some("a".to_string()),
            None,
            Some("1".to_string()),
        ]]
    );
    let Response::End { messages, .. } = &outcome.terminal else {
        panic!("an end, got {:?}", outcome.terminal);
    };
    assert!(messages.is_empty(), "{messages:?}");
}

#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_opened_offers_write() {
    let user = env::var("dbdelve_ODBC_POSTGRES_USER").unwrap_or_default();
    let password = env::var("dbdelve_ODBC_POSTGRES_PASSWORD").unwrap_or_default();
    let mut bridge = Bridge::spawn();
    let Response::Opened { optional, .. } = bridge.open_postgres_as(&user, &password) else {
        panic!("opened");
    };
    assert!(optional.contains(&WRITE.to_string()), "{optional:?}");
}

const POSTGRES_ROWS: &[&str] = &[
    "CREATE TEMP TABLE written (id integer PRIMARY KEY, v varchar(10))",
    "INSERT INTO written VALUES (1, 'a'), (2, 'b'), (3, 'c')",
];

fn written(bridge: &mut Bridge) -> Vec<Vec<Option<String>>> {
    bridge.query("SELECT id, v FROM written ORDER BY id").rows
}

fn row(id: &str, v: &str) -> Vec<Option<String>> {
    vec![Some(id.to_string()), Some(v.to_string())]
}

/// The session is idle afterwards, not idle in a transaction the write left
/// open, which only another session can see.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_write_commits_when_each_statement_changes_one_row() {
    let mut bridge = Bridge::open_postgres();
    run_all(&mut bridge, POSTGRES_ROWS);
    let saved = bridge.write(&[
        "UPDATE written SET v = 'x' WHERE id = 1",
        "INSERT INTO written VALUES (4, 'd')",
        "DELETE FROM written WHERE id = 2",
    ]);
    assert_eq!(
        saved,
        Response::End {
            rows: 0,
            rows_affected: Some(3),
            stopped: false,
            messages: Vec::new(),
            more_sets: 0,
        }
    );
    assert_eq!(
        written(&mut bridge),
        [row("1", "x"), row("3", "c"), row("4", "d")]
    );
    let pid = bridge.query("SELECT pg_backend_pid()").rows[0][0]
        .clone()
        .unwrap();
    let mut observer = Bridge::open_postgres();
    let state = observer.query(&format!(
        "SELECT state FROM pg_stat_activity WHERE pid = {pid}"
    ));
    assert_eq!(state.rows, [vec![Some("idle".to_string())]]);
}

#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_a_write_rolls_back_unless_every_statement_changes_one_row() {
    let mut bridge = Bridge::open_postgres();
    run_all(&mut bridge, POSTGRES_ROWS);
    let first = "UPDATE written SET v = 'x' WHERE id = 1";
    let cases = [
        (
            "UPDATE written SET v = 'x' WHERE id = 99",
            "The 2nd of 2 statements changed 0 rows, not one, so DBDelve rolled back every change and nothing was saved.",
        ),
        (
            "UPDATE written SET v = 'x' WHERE id IN (2, 3)",
            "The 2nd of 2 statements changed 2 rows, not one, so DBDelve rolled back every change and nothing was saved.",
        ),
        (
            "SELECT 1",
            "The 2nd of 2 statements returned rows instead of changing one, so DBDelve rolled back every change and nothing was saved.",
        ),
        (
            "UPDATE written SET v = 'x' WHERE id = 2; UPDATE written SET v = 'x' WHERE id = 3",
            "The 2nd of 2 statements held more than one statement, so DBDelve rolled back every change and nothing was saved.",
        ),
    ];
    for (second, said) in cases {
        let refused = bridge.write(&[first, second]);
        assert_eq!(write_error(&refused), (said, None, false), "{second}");
        assert_eq!(
            written(&mut bridge),
            [row("1", "a"), row("2", "b"), row("3", "c")],
            "{second}"
        );
    }

    let failed = bridge.write(&[first, "UPDATE written SET id = 1 / 0 WHERE id = 2"]);
    let (message, sqlstate, lost) = write_error(&failed);
    assert_eq!((sqlstate, lost), (Some("22012"), false), "{message}");
    assert!(
        message.ends_with(
            "\nThe 2nd of 2 statements failed, so DBDelve rolled back every change and nothing was saved."
        ),
        "{message}"
    );
    assert_eq!(written(&mut bridge)[0], row("1", "a"));

    let empty = bridge.write(&[]);
    assert!(
        matches!(
            empty,
            Response::Error {
                kind: ErrorKind::Protocol,
                ..
            }
        ),
        "{empty:?}"
    );
    still_autocommits(
        &mut bridge,
        "INSERT INTO written VALUES (5, 'e')",
        "SELECT count(*) FROM written WHERE id = 5",
    );
}

#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_cancel_rolls_a_write_back() {
    let mut bridge = Bridge::open_postgres();
    run_all(&mut bridge, POSTGRES_ROWS);
    let id = bridge.next;
    bridge.next += 1;
    bridge.send(
        id,
        Request::Write {
            statements: vec![
                "UPDATE written SET v = 'x' WHERE id = 1".to_string(),
                "UPDATE written SET v = 'y' WHERE id = (SELECT 2 FROM pg_sleep(30))".to_string(),
            ],
        },
    );
    thread::sleep(Duration::from_secs(1));
    let sent = Instant::now();
    bridge.send(id, Request::Cancel);
    let frame = bridge.receive();
    assert!(sent.elapsed() < CANCEL_DEADLINE, "{:?}", sent.elapsed());
    assert_eq!((frame.kind, frame.request), (Kind::Control, id));
    let cancelled = Response::from_payload(&frame.payload).unwrap();
    let Response::Error {
        kind,
        message,
        lost,
        ..
    } = &cancelled
    else {
        panic!("an error, got {cancelled:?}");
    };
    assert_eq!((*kind, *lost), (ErrorKind::Cancelled, false));
    assert_eq!(
        message,
        "The write was cancelled, so DBDelve rolled back every change and nothing was saved."
    );
    assert_eq!(written(&mut bridge)[..2], [row("1", "a"), row("2", "b")]);
    still_autocommits(
        &mut bridge,
        "INSERT INTO written VALUES (5, 'e')",
        "SELECT count(*) FROM written WHERE id = 5",
    );
}

/// psqlODBC reads a `bool` as `1` or `0` and reformats dates, and the bridge
/// reformats floats, so none of those finds its row again.
#[test]
#[ignore = "needs psqlODBC and a Postgres: set dbdelve_ODBC_POSTGRES, _USER and _PASSWORD"]
fn live_postgres_only_text_and_exact_numbers_are_exact() {
    let mut bridge = Bridge::open_postgres();
    let outcome = bridge.query(
        "SELECT 1::int4, 2::int8, 3::int2, 'a'::varchar(5), 'b'::char(3), 1.25::numeric(10, 2), \
         1.5::float8, 1.5::float4, timestamp '2024-01-01 00:00:00', date '2024-01-01', true",
    );
    assert_eq!(
        exact_columns(&outcome),
        [
            true, true, true, true, true, true, false, false, false, false, false
        ]
    );
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_opened_offers_write() {
    let mut bridge = Bridge::spawn();
    let opened = bridge.request(Request::Open {
        fields: live_fields(),
        password: Password(env::var("dbdelve_ODBC_DB2_PASSWORD").unwrap_or_default()),
        statement_timeout: 0,
    });
    let Response::Opened { optional, .. } = opened else {
        panic!("opened, got {opened:?}");
    };
    assert!(optional.contains(&WRITE.to_string()), "{optional:?}");
}

/// `table` dropped and created afresh with rows 1 to 3, `a` to `c`.
fn db2_rows(bridge: &mut Bridge, table: &str) {
    run_all(
        bridge,
        &[
            &format!("DROP TABLE IF EXISTS {table}"),
            &format!("CREATE TABLE {table} (ID INTEGER NOT NULL PRIMARY KEY, V VARCHAR(10))"),
            &format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b'), (3, 'c')"),
        ],
    );
}

fn db2_written(bridge: &mut Bridge, table: &str) -> Vec<Vec<Option<String>>> {
    bridge
        .query(&format!("SELECT ID, V FROM {table} ORDER BY ID"))
        .rows
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_write_commits_when_each_statement_changes_one_row() {
    const TABLE: &str = "DBDELVE_WRITE_COMMIT";
    let mut bridge = Bridge::open(0);
    db2_rows(&mut bridge, TABLE);
    let saved = bridge.write(&[
        &format!("UPDATE {TABLE} SET V = 'x' WHERE ID = 1"),
        &format!("INSERT INTO {TABLE} VALUES (4, 'd')"),
        &format!("DELETE FROM {TABLE} WHERE ID = 2"),
    ]);
    assert!(
        matches!(
            saved,
            Response::End {
                rows: 0,
                rows_affected: Some(3),
                ..
            }
        ),
        "{saved:?}"
    );
    assert_eq!(
        db2_written(&mut bridge, TABLE),
        [row("1", "x"), row("3", "c"), row("4", "d")]
    );
    still_autocommits(
        &mut bridge,
        &format!("INSERT INTO {TABLE} VALUES (5, 'e')"),
        &format!("SELECT COUNT(*) FROM {TABLE} WHERE ID = 5"),
    );
    run_all(&mut bridge, &[&format!("DROP TABLE {TABLE}")]);
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_a_write_rolls_back_unless_every_statement_changes_one_row() {
    const TABLE: &str = "DBDELVE_WRITE_ROLLBACK";
    let mut bridge = Bridge::open(0);
    db2_rows(&mut bridge, TABLE);
    let first = format!("UPDATE {TABLE} SET V = 'x' WHERE ID = 1");
    let cases = [
        (
            format!("UPDATE {TABLE} SET V = 'x' WHERE ID = 99"),
            "The 2nd of 2 statements changed 0 rows, not one, so DBDelve rolled back every change and nothing was saved.",
        ),
        (
            format!("UPDATE {TABLE} SET V = 'x' WHERE ID IN (2, 3)"),
            "The 2nd of 2 statements changed 2 rows, not one, so DBDelve rolled back every change and nothing was saved.",
        ),
        (
            "SELECT 1 FROM SYSIBM.SYSDUMMY1".to_string(),
            "The 2nd of 2 statements returned rows instead of changing one, so DBDelve rolled back every change and nothing was saved.",
        ),
    ];
    for (second, said) in &cases {
        let refused = bridge.write(&[&first, second]);
        assert_eq!(write_error(&refused), (*said, None, false), "{second}");
        assert_eq!(
            db2_written(&mut bridge, TABLE)[0],
            row("1", "a"),
            "{second}"
        );
    }
    let failed = bridge.write(&[
        &first,
        &format!("UPDATE {TABLE} SET ID = 1 / 0 WHERE ID = 2"),
    ]);
    let (message, sqlstate, lost) = write_error(&failed);
    assert!(sqlstate.is_some() && !lost, "{failed:?}");
    assert!(
        message.ends_with(
            "\nThe 2nd of 2 statements failed, so DBDelve rolled back every change and nothing was saved."
        ),
        "{message}"
    );
    assert_eq!(db2_written(&mut bridge, TABLE)[0], row("1", "a"));
    still_autocommits(
        &mut bridge,
        &format!("INSERT INTO {TABLE} VALUES (5, 'e')"),
        &format!("SELECT COUNT(*) FROM {TABLE} WHERE ID = 5"),
    );
    run_all(&mut bridge, &[&format!("DROP TABLE {TABLE}")]);
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_only_text_and_exact_numbers_are_exact() {
    let mut bridge = Bridge::open(0);
    let outcome = bridge.query(
        "SELECT CAST(1 AS INTEGER), CAST(2 AS BIGINT), CAST(3 AS SMALLINT), \
         CAST('a' AS VARCHAR(5)), CAST('b' AS CHAR(3)), CAST(1.25 AS DECIMAL(10, 2)), \
         CAST(1.5E0 AS DOUBLE), CURRENT TIMESTAMP, CURRENT DATE, CAST(1 AS BOOLEAN), \
         CAST(X'00FF' AS VARBINARY(4)) \
         FROM SYSIBM.SYSDUMMY1",
    );
    assert_eq!(
        exact_columns(&outcome),
        [
            true, true, true, true, true, true, false, false, false, false, false
        ]
    );
}

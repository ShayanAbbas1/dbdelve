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

use arrow_array::{Array, StringArray};
use arrow_schema::SchemaRef;
use dbdelve_plugin_protocol::arrow::{decode_batch, decode_schema};
use dbdelve_plugin_protocol::{
    ErrorKind, Frame, Kind, PROTOCOL, Password, Request, Response, read_frame, write_frame,
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

    fn query(&mut self, sql: &str) -> Outcome {
        self.query_until(sql, |_| None)
    }

    /// Runs `sql` as the host does, granting each data frame's credit back
    /// once it is read. `interrupt` sees the rows kept so far and may answer
    /// a `stop` or `cancel` to send, once.
    fn query_until(
        &mut self,
        sql: &str,
        mut interrupt: impl FnMut(usize) -> Option<Request>,
    ) -> Outcome {
        let id = self.next;
        self.next += 1;
        self.send(
            id,
            Request::Query {
                sql: sql.to_string(),
                eligible: false,
            },
        );
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
                                .map(|column| {
                                    let column =
                                        column.as_any().downcast_ref::<StringArray>().unwrap();
                                    (!column.is_null(row)).then(|| column.value(row).to_string())
                                })
                                .collect(),
                        );
                    }
                }
            }
            let bytes = frame.payload.len() as u64;
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
    let password = "not-the-password;}".to_string();
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

/// NULL through the SQLLEN normalisation, wide text, binary, and an escape
/// sequence the driver would rewrite if escape scanning were on.
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
            // IBM's own LOB type code, so the driver's own hex.
            Some("00FF".to_string()),
            Some("{fn UCASE('a')}".to_string()),
            Some("42".to_string()),
        ]]
    );
    assert_eq!(ended(&outcome), (1, None, false));
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

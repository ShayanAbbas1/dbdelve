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
use dbdelve_plugin_protocol::limits::CANCEL_DEADLINE;
use dbdelve_plugin_protocol::{
    ErrorKind, Frame, Kind, PROTOCOL, Password, Reference, RelationKind, Request, Response,
    read_frame, write_frame,
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

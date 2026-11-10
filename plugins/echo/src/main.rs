//! A fake engine for the host's tests: every host path, on command, so every
//! protocol test is deterministic. The statement text drives it, one word and
//! then its arguments:
//!
//! | statement      | answer                                                                 |
//! | -------------- | ---------------------------------------------------------------------- |
//! | `rows N`       | `n` (Int64, 1 to N) and `label` (Utf8 `row n`, null where n is a multiple of 7), 1,024 rows a batch |
//! | `miscount N M` | `rows N`, then an `end` claiming M rows                                |
//! | `types`        | one row of every type protocol 1 carries, then a row of nulls          |
//! | `sleep MS`     | an `end` after MS milliseconds, or `error {kind: cancelled}` on `cancel` |
//! | `hang`         | nothing, ever: stops reading stdin, so `cancel` and EOF go unseen      |
//! | `crash`        | a schema and one batch, then aborts                                    |
//! | `exit N`       | exits with status N, answering nothing                                 |
//! | `orphan`       | starts a process that holds its stdout for 20 s, then exits with status 0 |
//! | `cut`          | half a frame, then exits with status 4                                 |
//! | `flood`        | 16 MiB of batches, ignoring credit                                     |
//! | `oversize`     | a length prefix over `MAX_FRAME`, and nothing after it                 |
//! | `garbage`      | a control frame that is not JSON                                       |
//! | `empty`        | a data frame with no payload                                           |
//! | `compressed`   | a schema, then a batch carrying compression metadata                   |
//! | `dictionary`   | a schema, then a dictionary batch                                      |
//! | `begin`, `commit`, `rollback` | opens or closes the fake transaction                    |
//! | `fail`         | `error {kind: server, sqlstate: "42000"}`                              |
//! | `lost`         | `error {lost: true}`                                                   |
//! | `echo …`, `SET …` | the statement's own text as one cell, proving nothing rewrote it    |
//! | `last`         | the text of the statement before it, as one cell                       |
//! | `pid`          | echo's own process id, as one cell                                     |
//! | `sets N`       | one row, and `end {more_sets: N - 1}`                                  |
//! | `cancels`      | how many `cancel` frames it has received, as one cell                  |
//! | `SELECT … FROM s.t …` | one row of the select list over a table `structure` knows, every column Utf8 tagged with its type, those of a type that reads back exactly marked [`EXACT`], and those naming a column of the table given its [`BASE_TABLE`] |
//!
//! `structure` of a relation named `fail` or `lost` answers as those
//! statements do; `sleep` waits up to 60 s for a `cancel`; `hang` is as above.
//! In the schemas `main` and `MAIN` it knows only [`TABLES`], answering an
//! empty structure for anything else, as the ODBC bridge does for a relation
//! that does not exist; in any other schema every relation is `accounts`.
//!
//! `write` runs its statements as one fake transaction: each changes one row,
//! but one holding `'missing'` changes none, which fails the lot with nothing
//! kept. `last` then answers with the statements, one a line.
//!
//! `stop` ends a read early only when the query was `eligible` and the fake
//! transaction is closed; otherwise echo stops sending and answers with the
//! total the statement would have produced.
//!
//! Environment: `DBDELVE_ECHO_PROTOCOL=<n>` makes `hello` claim protocol n;
//! `DBDELVE_ECHO_STDERR=<text>` writes the text to stderr and exits before
//! `hello`; `DBDELVE_ECHO_PLAIN=1` leaves `references` and `write` out of
//! `optional`;
//! `DBDELVE_ECHO_DRAINS=1` opens with `stops_early: false`;
//! `DBDELVE_ECHO_REFUSE_SET=1` answers every `SET …` as `fail` does;
//! `DBDELVE_ECHO_HELD_ENDS=1` holds `commit` and `rollback` up to 60 s for a
//! `cancel`, answering one with `error {kind: cancelled}` and the transaction
//! still open, as the ODBC bridge answers a call cancelled before it ran;
//! `DBDELVE_ECHO_WEDGED=1` never answers `catalog`, `structure` or
//! `references` while reading stdin on, as a driver stuck in a catalog call;
//! `DBDELVE_ECHO_LINGER=<ms>` only sleeps that long, which is what `orphan`
//! starts; `DBDELVE_ECHO_CLOSED=<path>` takes [`CLOSING`] over `close`, as a
//! driver's disconnect does, and writes the path just before `closed` is
//! sent, so a host that ends stdin first finds no file.
//!
//! It exits on stdin EOF, as every plugin must.

use std::collections::HashMap;
use std::env;
use std::io::{self, BufWriter, StdoutLock, Write};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use arrow_array::*;
use arrow_buffer::{Buffer, IntervalDayTime, IntervalMonthDayNano, i256};
use arrow_data::ArrayData;
use arrow_ipc::writer::{
    DictionaryTracker, IpcDataGenerator, IpcWriteContext, IpcWriteOptions, write_message,
};
use arrow_ipc::{
    BodyCompressionBuilder, BodyCompressionMethod, CompressionType, FieldNode, MessageBuilder,
    MessageHeader, MetadataVersion, RecordBatchBuilder,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dbdelve_plugin_protocol as wire;
use dbdelve_plugin_protocol::arrow::{encode_batch, encode_schema};
use dbdelve_plugin_protocol::limits::{BATCH_ROWS, MAX_FRAME, WINDOW, credit_cost};
use dbdelve_plugin_protocol::{
    BASE_COLUMN, BASE_SCHEMA, BASE_TABLE, EXACT, ErrorKind, Kind, PROTOCOL, REFERENCES, Request,
    Response, Tls, WRITE, read_frame, write_frame,
};

const CLOSING: Duration = Duration::from_millis(200);

fn main() {
    if let Ok(text) = env::var("DBDELVE_ECHO_STDERR") {
        eprintln!("{text}");
        process::exit(1);
    }
    if let Ok(ms) = env::var("DBDELVE_ECHO_LINGER") {
        thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
        return;
    }

    let control = Arc::new(Control::default());
    let (jobs, inbox) = mpsc::channel();
    let worker = Arc::clone(&control);
    thread::spawn(move || Echo::new(worker).work(inbox));

    // The reader never stops reading while a statement runs, so `credit`,
    // `stop` and `cancel` reach the statement they target.
    let mut stdin = io::stdin().lock();
    loop {
        let frame = match read_frame(&mut stdin) {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(violation) => {
                eprintln!("dbdelve-echo: {violation}");
                process::exit(2);
            }
        };
        let id = frame.request;
        let job = match frame.kind {
            Kind::Data => Job::Refuse(id, ErrorKind::Protocol, "echo takes no data".into()),
            Kind::Control => match Request::from_payload(&frame.payload) {
                Ok(Request::Credit { bytes }) => {
                    control.credit(id, bytes);
                    continue;
                }
                Ok(Request::Stop) => {
                    control.stop(id);
                    continue;
                }
                Ok(Request::Cancel) => {
                    control.cancel(id);
                    continue;
                }
                Ok(Request::Query { sql, .. }) if sql.trim() == "hang" => loop {
                    thread::park();
                },
                Ok(Request::Structure { relation, .. }) if relation == "hang" => loop {
                    thread::park();
                },
                Ok(request) => {
                    if matches!(
                        request,
                        Request::Query { .. }
                            | Request::Write { .. }
                            | Request::Catalog
                            | Request::Structure { .. }
                            | Request::References { .. }
                    ) {
                        control.start(id);
                    }
                    Job::Run(id, request)
                }
                Err(violation) => Job::Refuse(id, ErrorKind::Unsupported, violation.to_string()),
            },
        };
        jobs.send(job).expect("the worker outlives the reader");
    }
}

fn catalog() -> Vec<wire::Schema> {
    let relation = |name: &str, kind| wire::Relation {
        name: name.into(),
        kind,
    };
    vec![
        wire::Schema {
            name: "main".into(),
            relations: vec![
                relation("accounts", wire::RelationKind::Table),
                relation("active_accounts", wire::RelationKind::View),
                relation("totals", wire::RelationKind::MaterializedView),
            ],
        },
        wire::Schema {
            name: "audit".into(),
            relations: vec![relation("log", wire::RelationKind::Table)],
        },
    ]
}

/// A table echo knows by its exact name: its columns, as `(name, type)`, and
/// its key.
type Table = (
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

/// Every relation of the schemas `main` and `MAIN`, as `(schema, name, table)`.
/// Folded the two ways a Generic SQL server may fold `main.items`, only one
/// exists; `main.both` both ways, and `main.Cased` as written and folded up.
const TABLES: [(&str, &str, Table); 9] = [
    ("main", "accounts", (ACCOUNTS, &["id"])),
    ("main", "keyless", (ACCOUNTS, &[])),
    (
        "main",
        "inexact",
        (&[("id", "double"), ("label", "text")], &["id"]),
    ),
    (
        "main",
        "mixed",
        (
            &[("id", "bigint"), ("label", "text"), ("LABEL", "text")],
            &["id"],
        ),
    ),
    ("main", "both", (ACCOUNTS, &["id"])),
    ("MAIN", "BOTH", (ITEMS, &["ID"])),
    ("MAIN", "ITEMS", (ITEMS, &["ID"])),
    (
        "main",
        "Cased",
        (&[("id", "bigint"), ("Label", "text")], &["id"]),
    ),
    ("MAIN", "CASED", (ITEMS, &["ID"])),
];

const ACCOUNTS: &[(&str, &str)] = &[("id", "bigint"), ("label", "text")];
const ITEMS: &[(&str, &str)] = &[("ID", "INTEGER"), ("LABEL", "VARCHAR(20)")];

/// The table `schema.relation` names, or `None` where there is none.
fn table(schema: &str, relation: &str) -> Option<Table> {
    if !matches!(schema, "main" | "MAIN") {
        return Some((ACCOUNTS, &["id"]));
    }
    TABLES
        .iter()
        .find(|(s, r, _)| *s == schema && *r == relation)
        .map(|(_, _, table)| *table)
}

/// Echoes the schema and relation asked about into the foreign key, so a test
/// sees both arrive.
fn structure(schema: &str, relation: &str) -> wire::Structure {
    let detailed = !matches!(schema, "main" | "MAIN") || (schema, relation) == ("main", "accounts");
    match table(schema, relation) {
        None => wire::Structure::default(),
        Some(_) if detailed => accounts(schema, relation),
        Some((columns, key)) => wire::Structure {
            columns: columns
                .iter()
                .map(|(name, data_type)| wire::Column {
                    name: (*name).into(),
                    data_type: (*data_type).into(),
                    nullable: true,
                    default: None,
                })
                .collect(),
            primary_key: key.iter().map(|name| (*name).into()).collect(),
            ..wire::Structure::default()
        },
    }
}

/// The table every relation outside `main` and `MAIN` is, with an index and
/// a foreign key.
fn accounts(schema: &str, relation: &str) -> wire::Structure {
    let column = |name: &str, data_type: &str, nullable, default: Option<&str>| wire::Column {
        name: name.into(),
        data_type: data_type.into(),
        nullable,
        default: default.map(Into::into),
    };
    wire::Structure {
        columns: vec![
            column("id", "bigint", false, None),
            column("label", "text", true, Some("'none'")),
        ],
        primary_key: vec!["id".into()],
        indexes: vec![
            wire::Index {
                name: "pk".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
            },
            wire::Index {
                name: "by_label".into(),
                columns: vec!["label".into()],
                unique: false,
                primary: false,
            },
        ],
        foreign_keys: vec![wire::ForeignKey {
            column: "label".into(),
            referenced_schema: schema.into(),
            referenced_table: relation.into(),
            referenced_column: "id".into(),
        }],
    }
}

enum Job {
    Run(u64, Request),
    Refuse(u64, ErrorKind, String),
}

/// The running query's credit and interruptions, shared between the reader,
/// which applies `credit`, `stop` and `cancel` as they arrive, and the worker,
/// which waits on them.
#[derive(Default)]
struct Control {
    query: Mutex<Running>,
    wake: Condvar,
    cancels: AtomicU64,
}

#[derive(Default)]
struct Running {
    id: Option<u64>,
    /// Signed: one frame larger than the window may go when nothing is
    /// outstanding, which takes the credit below zero until its grant returns.
    left: i64,
    stop: bool,
    cancel: bool,
}

enum Interrupt {
    Stop,
    Cancel,
}

impl Control {
    /// Called by the reader before the query reaches the worker, so a `stop` or
    /// `cancel` sent right behind it is never applied to the one before.
    fn start(&self, id: u64) {
        *self.query.lock().unwrap() = Running {
            id: Some(id),
            left: WINDOW as i64,
            stop: false,
            cancel: false,
        };
    }

    fn credit(&self, id: u64, bytes: u64) {
        self.update(id, |running| running.left += bytes as i64);
    }

    fn stop(&self, id: u64) {
        self.update(id, |running| running.stop = true);
    }

    fn cancel(&self, id: u64) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        self.update(id, |running| running.cancel = true);
    }

    fn update(&self, id: u64, change: impl FnOnce(&mut Running)) {
        let mut running = self.query.lock().unwrap();
        if running.id == Some(id) {
            change(&mut running);
            self.wake.notify_all();
        }
    }

    /// The host's rule exactly: a frame goes when it fits in what is left, or
    /// alone when nothing is outstanding.
    fn take_credit(&self, payload: usize) -> Result<(), Interrupt> {
        let size = credit_cost(payload) as i64;
        let mut running = self.query.lock().unwrap();
        loop {
            if running.cancel {
                return Err(Interrupt::Cancel);
            }
            if running.stop {
                return Err(Interrupt::Stop);
            }
            if size <= running.left || running.left == WINDOW as i64 {
                running.left -= size;
                return Ok(());
            }
            running = self.wake.wait(running).unwrap();
        }
    }

    fn cancelled_within(&self, duration: Duration) -> bool {
        let running = self.query.lock().unwrap();
        let (running, _) = self
            .wake
            .wait_timeout_while(running, duration, |running| !running.cancel)
            .unwrap();
        running.cancel
    }
}

struct Echo {
    control: Arc<Control>,
    out: BufWriter<StdoutLock<'static>>,
    /// Opened with `stops_early: false`, as the ODBC bridge always is.
    drains: bool,
    transaction: bool,
    optional: Vec<String>,
    refuses_set: bool,
    holds_ends: bool,
    wedged: bool,
    last: String,
}

impl Echo {
    fn new(control: Arc<Control>) -> Self {
        let plain = env::var("DBDELVE_ECHO_PLAIN").is_ok_and(|value| value == "1");
        Self {
            control,
            out: BufWriter::new(io::stdout().lock()),
            drains: env::var("DBDELVE_ECHO_DRAINS").is_ok_and(|value| value == "1"),
            transaction: false,
            refuses_set: env::var("DBDELVE_ECHO_REFUSE_SET").is_ok_and(|value| value == "1"),
            holds_ends: env::var("DBDELVE_ECHO_HELD_ENDS").is_ok_and(|value| value == "1"),
            wedged: env::var("DBDELVE_ECHO_WEDGED").is_ok_and(|value| value == "1"),
            last: String::new(),
            optional: match plain {
                true => Vec::new(),
                false => vec![REFERENCES.to_string(), WRITE.to_string()],
            },
        }
    }

    fn work(mut self, inbox: Receiver<Job>) {
        for job in inbox {
            match job {
                Job::Run(id, request) => self.run(id, request),
                Job::Refuse(id, kind, message) => self.error(id, kind, message, None, false),
            }
        }
    }

    fn run(&mut self, id: u64, request: Request) {
        if self.wedged
            && matches!(
                request,
                Request::Catalog | Request::Structure { .. } | Request::References { .. }
            )
        {
            loop {
                thread::park();
            }
        }
        match request {
            Request::Hello { .. } => {
                let protocol = env::var("DBDELVE_ECHO_PROTOCOL")
                    .ok()
                    .and_then(|protocol| protocol.parse().ok())
                    .unwrap_or(PROTOCOL);
                self.reply(
                    id,
                    Response::Hello {
                        protocol,
                        implementation: format!("dbdelve-echo {}", env!("CARGO_PKG_VERSION")),
                        optional: self.optional.clone(),
                    },
                );
            }
            Request::Prepare { .. } => self.reply(
                id,
                Response::Prepared {
                    tls: Tls {
                        separate_name: true,
                        name_in_verify_ca: false,
                    },
                },
            ),
            Request::Open { .. } => self.reply(
                id,
                Response::Opened {
                    dbms_name: "Echo".to_string(),
                    dbms_version: env!("CARGO_PKG_VERSION").to_string(),
                    driver: "dbdelve-echo".to_string(),
                    stops_early: !self.drains,
                    optional: self.optional.clone(),
                },
            ),
            Request::Query { sql, eligible } => self.query(id, &sql, eligible),
            Request::Write { statements } => self.write_rows(id, statements),
            Request::Catalog => self.reply(id, Response::Catalog { schemas: catalog() }),
            Request::Structure { schema, relation } => match relation.as_str() {
                "fail" => self.error(
                    id,
                    ErrorKind::Server,
                    "no such relation".into(),
                    None,
                    false,
                ),
                "lost" => self.error(id, ErrorKind::Server, "session gone".into(), None, true),
                "sleep" if self.control.cancelled_within(Duration::from_secs(60)) => {
                    self.cancelled(id);
                }
                _ => self.reply(id, Response::Structure(structure(&schema, &relation))),
            },
            Request::References { schema, relation } => self.reply(
                id,
                Response::References {
                    references: vec![wire::Reference {
                        schema: "referrers".into(),
                        table: format!("{schema}.{relation}"),
                        column: "owner_id".into(),
                        referenced_column: "id".into(),
                    }],
                },
            ),
            Request::Close => {
                // Written before `closed` goes out: once it has, the host may
                // end stdin at once, and the reader's exit on EOF would race
                // the write.
                if let Some(marker) = env::var_os("DBDELVE_ECHO_CLOSED") {
                    thread::sleep(CLOSING);
                    let _ = std::fs::write(marker, b"closed");
                }
                self.reply(id, Response::Closed);
                let _ = self.out.flush();
                process::exit(0);
            }
            other => self.error(
                id,
                ErrorKind::Unsupported,
                format!("echo does not answer {other:?}"),
                None,
                false,
            ),
        }
    }

    fn query(&mut self, id: u64, sql: &str, eligible: bool) {
        let previous = std::mem::replace(&mut self.last, sql.to_owned());
        let mut words = sql.split_whitespace();
        let command = words.next().unwrap_or_default();
        let argument = words.next().unwrap_or_default();
        let number = || argument.parse::<u64>().unwrap_or(0);
        let batches = |total| {
            (1..=total)
                .step_by(BATCH_ROWS)
                .map(move |from| numbered(from, (from + BATCH_ROWS as u64).min(total + 1)))
        };
        match command {
            "rows" => {
                let total = number();
                self.stream(id, numbered_schema(), batches(total), total, eligible, 0);
            }
            "miscount" => {
                let claimed = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                self.stream(id, numbered_schema(), batches(number()), claimed, false, 0);
            }
            "types" => {
                let batch = types();
                self.stream(id, batch.schema(), [batch], 2, eligible, 0);
            }
            "sleep" => match self
                .control
                .cancelled_within(Duration::from_millis(number()))
            {
                true => self.cancelled(id),
                false => self.end(id, 0, false, 0),
            },
            "crash" => {
                let schema = numbered_schema();
                let _ = self.send(id, &encode_schema(&schema));
                let _ = self.send(id, &encode_batch(&numbered(1, 11)).unwrap());
                process::abort();
            }
            "exit" => process::exit(argument.parse().unwrap_or(1)),
            "orphan" => {
                let _ = self.out.flush();
                // Inherits stdout, as a driver's helper process would.
                let _ = process::Command::new(env::current_exe().unwrap())
                    .env("DBDELVE_ECHO_LINGER", "20000")
                    .stdin(process::Stdio::null())
                    .stderr(process::Stdio::null())
                    .spawn();
                process::exit(0);
            }
            "cut" => {
                let mut half = 100u32.to_le_bytes().to_vec();
                half.push(Kind::Control as u8);
                half.extend(id.to_le_bytes());
                half.extend(br#"{"type":"#);
                self.write_raw(&half);
                process::exit(4);
            }
            "flood" => self.flood(id),
            "oversize" => self.write_raw(&(MAX_FRAME + 1).to_le_bytes()),
            "garbage" => self.write(Kind::Control, id, br#"{"type": "end", "rows": "#),
            "empty" => self.write(Kind::Data, id, &[]),
            "compressed" => {
                let schema = numbered_schema();
                if self.send(id, &encode_schema(&schema)).is_ok() {
                    self.write(Kind::Data, id, &compressed());
                    self.end(id, 2, false, 0);
                }
            }
            "dictionary" => {
                let schema = Schema::new(vec![Field::new("d", DataType::Utf8, true)]);
                if self.send(id, &encode_schema(&schema)).is_ok() {
                    self.write(Kind::Data, id, &dictionary());
                    self.end(id, 3, false, 0);
                }
            }
            "begin" => {
                self.transaction = true;
                self.end(id, 0, false, 0);
            }
            "commit" | "rollback"
                if self.holds_ends && self.control.cancelled_within(Duration::from_secs(60)) =>
            {
                self.cancelled(id);
            }
            "commit" | "rollback" => {
                self.transaction = false;
                self.end(id, 0, false, 0);
            }
            "fail" => self.error(
                id,
                ErrorKind::Server,
                "echo failed, as asked".into(),
                Some("42000".into()),
                false,
            ),
            "lost" => self.error(
                id,
                ErrorKind::Server,
                "echo lost the session, as asked".into(),
                None,
                true,
            ),
            "echo" => self.one_cell(id, "statement", Arc::new(StringArray::from(vec![sql])), 0),
            set if set.eq_ignore_ascii_case("set") && self.refuses_set => self.error(
                id,
                ErrorKind::Server,
                "echo refused the setting, as asked".into(),
                Some("42000".into()),
                false,
            ),
            set if set.eq_ignore_ascii_case("set") => {
                self.one_cell(id, "statement", Arc::new(StringArray::from(vec![sql])), 0);
            }
            "pid" => self.one_cell(
                id,
                "pid",
                Arc::new(Int64Array::from(vec![i64::from(process::id())])),
                0,
            ),
            "last" => self.one_cell(
                id,
                "statement",
                Arc::new(StringArray::from(vec![previous])),
                0,
            ),
            "sets" => {
                let more = u32::try_from(number().saturating_sub(1)).unwrap_or(u32::MAX);
                self.one_cell(id, "set", Arc::new(Int64Array::from(vec![1])), more);
            }
            "cancels" => {
                let cancels = self.control.cancels.load(Ordering::SeqCst) as i64;
                self.one_cell(id, "cancels", Arc::new(Int64Array::from(vec![cancels])), 0);
            }
            select if select.eq_ignore_ascii_case("select") => match selected(sql) {
                Some(batch) => self.stream(id, batch.schema(), [batch], 1, eligible, 0),
                None => self.error(
                    id,
                    ErrorKind::Server,
                    "echo found no such table".into(),
                    Some("42S02".into()),
                    false,
                ),
            },
            _ => self.error(
                id,
                ErrorKind::Server,
                format!("echo does not know `{command}`"),
                Some("42601".into()),
                false,
            ),
        }
    }

    /// One fake transaction: committed, as nothing, only if every statement
    /// changed one row.
    fn write_rows(&mut self, id: u64, statements: Vec<String>) {
        self.last = statements.join("\n");
        if let Some(at) = statements
            .iter()
            .position(|statement| statement.contains("'missing'"))
        {
            let message = format!("Statement {} changed 0 rows, so nothing was saved.", at + 1);
            self.error(id, ErrorKind::Server, message, None, false);
            return;
        }
        let end = Response::End {
            rows: 0,
            rows_affected: Some(statements.len() as u64),
            stopped: false,
            messages: Vec::new(),
            more_sets: 0,
        };
        self.reply(id, end);
    }

    fn one_cell(&mut self, id: u64, name: &str, column: ArrayRef, more_sets: u32) {
        let batch = RecordBatch::try_from_iter([(name, column)]).unwrap();
        self.stream(id, batch.schema(), [batch], 1, false, more_sets);
    }

    /// Sends a result set under credit. On `stop`, ends early only when that
    /// keeps the fake transaction, and otherwise answers as if it had fetched
    /// to the end without sending: the host is told the whole statement's
    /// count, which is what it reports as capped.
    fn stream(
        &mut self,
        id: u64,
        schema: SchemaRef,
        batches: impl IntoIterator<Item = RecordBatch>,
        total: u64,
        eligible: bool,
        more_sets: u32,
    ) {
        let mut sent = 0;
        let mut outcome = self.send(id, &encode_schema(&schema));
        for batch in batches {
            if outcome.is_err() {
                break;
            }
            outcome = self.send(id, &encode_batch(&batch).unwrap());
            sent += batch.num_rows() as u64;
        }
        match outcome {
            Ok(()) => self.end(id, total, false, more_sets),
            Err(Interrupt::Stop) if eligible && !self.transaction => {
                self.end(id, sent, true, more_sets);
            }
            Err(Interrupt::Stop) => self.end(id, total, false, more_sets),
            Err(Interrupt::Cancel) => self.cancelled(id),
        }
    }

    fn flood(&mut self, id: u64) {
        let chunk = "x".repeat(1024);
        let column: ArrayRef = Arc::new(StringArray::from(vec![chunk.as_str(); 1024]));
        let batch = RecordBatch::try_from_iter([("chunk", column)]).unwrap();
        self.write(Kind::Data, id, &encode_schema(&batch.schema()));
        let payload = encode_batch(&batch).unwrap();
        for _ in 0..16 {
            self.write(Kind::Data, id, &payload);
        }
        self.end(id, 16 * 1024, false, 0);
    }

    fn send(&mut self, id: u64, payload: &[u8]) -> Result<(), Interrupt> {
        self.control.take_credit(payload.len())?;
        self.write(Kind::Data, id, payload);
        Ok(())
    }

    fn end(&mut self, id: u64, rows: u64, stopped: bool, more_sets: u32) {
        let end = Response::End {
            rows,
            rows_affected: None,
            stopped,
            messages: Vec::new(),
            more_sets,
        };
        self.reply(id, end);
    }

    fn cancelled(&mut self, id: u64) {
        let message = "echo was cancelled".into();
        self.error(id, ErrorKind::Cancelled, message, None, false);
    }

    fn error(
        &mut self,
        id: u64,
        kind: ErrorKind,
        message: String,
        sqlstate: Option<String>,
        lost: bool,
    ) {
        let error = Response::Error {
            kind,
            message,
            sqlstate,
            position: None,
            lost,
        };
        self.reply(id, error);
    }

    fn reply(&mut self, id: u64, response: Response) {
        self.write(Kind::Control, id, &response.to_payload());
    }

    /// The host has gone, or killed this process mid-write: nothing is left to
    /// answer.
    fn write(&mut self, kind: Kind, id: u64, payload: &[u8]) {
        if write_frame(&mut self.out, kind, id, payload).is_err() {
            process::exit(1);
        }
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        if self
            .out
            .write_all(bytes)
            .and_then(|()| self.out.flush())
            .is_err()
        {
            process::exit(1);
        }
    }
}

/// One row of `SELECT <list> FROM <schema>.<table> …`, its table read as a
/// server would: a quoted part as written, an unquoted one as written or
/// folded either way. `None` where there is no such table.
fn selected(sql: &str) -> Option<RecordBatch> {
    let upper = sql.to_ascii_uppercase();
    let from = upper.find(" FROM ")?;
    let list = sql["SELECT".len()..from].trim();
    let name = sql[from + " FROM ".len()..]
        .split_whitespace()
        .next()?
        .trim_end_matches(';');
    let parts: Vec<&str> = name.split('.').collect();
    // A bare name is read from `main`, as a session's search path would.
    let (schema, relation) = match parts.as_slice() {
        [schema, relation] => (*schema, *relation),
        [relation] => ("main", *relation),
        _ => return None,
    };
    let spellings = |part: &str| match part.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
        Some(quoted) => vec![quoted.to_owned()],
        None => vec![part.to_owned(), part.to_uppercase(), part.to_lowercase()],
    };
    let candidates: Vec<(String, String)> = spellings(schema)
        .iter()
        .flat_map(|schema| {
            spellings(relation)
                .into_iter()
                .map(move |relation| (schema.clone(), relation))
        })
        .collect();
    // A relation echo names exactly before one any schema but `main` answers,
    // so the base it reports is the one the host looked up.
    let (schema, relation) = candidates
        .iter()
        .find(|(schema, relation)| TABLES.iter().any(|(s, r, _)| s == schema && r == relation))
        .or_else(|| {
            candidates
                .iter()
                .find(|(schema, relation)| table(schema, relation).is_some())
        })?;
    let (columns, _) = table(schema, relation)?;
    let column = |written: &str| {
        let name = written.rsplit('.').next().unwrap_or(written);
        let unquoted = name.trim_matches('"');
        columns
            .iter()
            .find(|(column, _)| *column == unquoted)
            .or_else(|| {
                columns
                    .iter()
                    .find(|(column, _)| column.eq_ignore_ascii_case(unquoted))
            })
    };
    // `(name, type, the table column it is read from)`.
    let mut fields: Vec<(String, String, Option<&str>)> = Vec::new();
    for item in list.split(',').map(str::trim) {
        if item == "*" {
            fields.extend(columns.iter().map(|(name, data_type)| {
                ((*name).to_owned(), (*data_type).to_owned(), Some(*name))
            }));
            continue;
        }
        let (expression, alias) = match item.to_ascii_uppercase().find(" AS ") {
            Some(at) => (&item[..at], Some(item[at + " AS ".len()..].trim())),
            None => (item, None),
        };
        let named = alias.unwrap_or_else(|| expression.rsplit('.').next().unwrap_or(expression));
        let read = column(expression);
        fields.push((
            named.trim_matches('"').to_owned(),
            read.map_or("integer", |(_, data_type)| data_type)
                .to_owned(),
            read.map(|(name, _)| *name),
        ));
    }
    let (fields, cells): (Vec<Field>, Vec<ArrayRef>) = fields
        .into_iter()
        .map(|(name, data_type, read_from)| {
            let base = data_type.split('(').next().unwrap_or_default().to_owned();
            let exact = !base.eq_ignore_ascii_case("double");
            let value = match exact && !base.to_ascii_lowercase().contains("char") && base != "text"
            {
                true => "1",
                false => "a",
            };
            let mut metadata = HashMap::from([("dbdelve.type".to_owned(), base)]);
            if exact {
                metadata.insert(EXACT.to_owned(), "true".to_owned());
            }
            if let Some(read_from) = read_from {
                metadata.extend([
                    (BASE_SCHEMA.to_owned(), schema.clone()),
                    (BASE_TABLE.to_owned(), relation.clone()),
                    (BASE_COLUMN.to_owned(), read_from.to_owned()),
                ]);
            }
            let field = Field::new(name, DataType::Utf8, true).with_metadata(metadata);
            let cell: ArrayRef = Arc::new(StringArray::from(vec![value]));
            (field, cell)
        })
        .unzip();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), cells).ok()
}

fn numbered_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int64, false),
        Field::new("label", DataType::Utf8, true),
    ]))
}

/// Rows `from` up to but not including `to`.
fn numbered(from: u64, to: u64) -> RecordBatch {
    let n = Int64Array::from_iter_values((from..to).map(|n| n as i64));
    let label =
        StringArray::from_iter((from..to).map(|n| (n % 7 != 0).then(|| format!("row {n}"))));
    RecordBatch::try_new(numbered_schema(), vec![Arc::new(n), Arc::new(label)]).unwrap()
}

/// 2024-02-29 12:34:56 in every spelling a type has, and `0x00FF`, `12.50`.
fn types() -> RecordBatch {
    let float16 = Float16Array::from(
        ArrayData::builder(DataType::Float16)
            .len(2)
            .add_buffer(Buffer::from_slice_ref([0x3e00u16, 0]))
            .null_bit_buffer(Some(Buffer::from([0b01u8])))
            .build()
            .unwrap(),
    );
    let columns: Vec<(&str, ArrayRef, Option<&str>)> = vec![
        ("null", Arc::new(NullArray::new(2)), None),
        (
            "boolean",
            Arc::new(BooleanArray::from(vec![Some(true), None])),
            None,
        ),
        (
            "int8",
            Arc::new(Int8Array::from(vec![Some(-8), None])),
            None,
        ),
        (
            "int16",
            Arc::new(Int16Array::from(vec![Some(-16), None])),
            None,
        ),
        (
            "int32",
            Arc::new(Int32Array::from(vec![Some(-32), None])),
            Some("INTEGER"),
        ),
        (
            "int64",
            Arc::new(Int64Array::from(vec![Some(-64), None])),
            None,
        ),
        (
            "uint8",
            Arc::new(UInt8Array::from(vec![Some(8), None])),
            None,
        ),
        (
            "uint16",
            Arc::new(UInt16Array::from(vec![Some(16), None])),
            None,
        ),
        (
            "uint32",
            Arc::new(UInt32Array::from(vec![Some(32), None])),
            None,
        ),
        (
            "uint64",
            Arc::new(UInt64Array::from(vec![Some(u64::MAX), None])),
            None,
        ),
        ("float16", Arc::new(float16), None),
        (
            "float32",
            Arc::new(Float32Array::from(vec![Some(1.5), None])),
            None,
        ),
        (
            "float64",
            Arc::new(Float64Array::from(vec![Some(2.5), None])),
            Some("DOUBLE"),
        ),
        (
            "decimal128",
            Arc::new(
                Decimal128Array::from(vec![Some(1250), None])
                    .with_precision_and_scale(10, 2)
                    .unwrap(),
            ),
            Some("DECIMAL(10,2)"),
        ),
        (
            "decimal256",
            Arc::new(
                Decimal256Array::from(vec![Some(i256::from_i128(1250)), None])
                    .with_precision_and_scale(40, 2)
                    .unwrap(),
            ),
            None,
        ),
        (
            "utf8",
            Arc::new(StringArray::from(vec![Some("text"), None])),
            Some("VARCHAR(20)"),
        ),
        (
            "large_utf8",
            Arc::new(LargeStringArray::from(vec![Some("large text"), None])),
            None,
        ),
        (
            "utf8_view",
            Arc::new(StringViewArray::from(vec![
                Some("longer than twelve bytes"),
                None,
            ])),
            None,
        ),
        (
            "binary",
            Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
            Some("BLOB"),
        ),
        (
            "large_binary",
            Arc::new(LargeBinaryArray::from(vec![Some(&b"\x01"[..]), None])),
            None,
        ),
        (
            "fixed_size_binary",
            Arc::new(
                FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                    [Some([0xab, 0xcd]), None].into_iter(),
                    2,
                )
                .unwrap(),
            ),
            None,
        ),
        (
            "date32",
            Arc::new(Date32Array::from(vec![Some(19782), None])),
            Some("DATE"),
        ),
        (
            "date64",
            Arc::new(Date64Array::from(vec![Some(1_709_164_800_000), None])),
            None,
        ),
        (
            "time32",
            Arc::new(Time32SecondArray::from(vec![Some(45_296), None])),
            Some("TIME"),
        ),
        (
            "time32_ms",
            Arc::new(Time32MillisecondArray::from(vec![Some(45_296_000), None])),
            None,
        ),
        (
            "time64",
            Arc::new(Time64MicrosecondArray::from(vec![
                Some(45_296_000_000),
                None,
            ])),
            None,
        ),
        (
            "time64_ns",
            Arc::new(Time64NanosecondArray::from(vec![
                Some(45_296_000_000_000),
                None,
            ])),
            None,
        ),
        (
            "timestamp",
            Arc::new(TimestampMicrosecondArray::from(vec![
                Some(1_709_210_096_000_000),
                None,
            ])),
            Some("TIMESTAMP"),
        ),
        (
            "duration",
            Arc::new(DurationSecondArray::from(vec![Some(90), None])),
            None,
        ),
        (
            "interval_ym",
            Arc::new(IntervalYearMonthArray::from(vec![Some(14), None])),
            None,
        ),
        (
            "interval_dt",
            Arc::new(IntervalDayTimeArray::from(vec![
                Some(IntervalDayTime::new(1, 2)),
                None,
            ])),
            None,
        ),
        (
            "interval_mdn",
            Arc::new(IntervalMonthDayNanoArray::from(vec![
                Some(IntervalMonthDayNano::new(1, 2, 3)),
                None,
            ])),
            None,
        ),
    ];
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, column, native)| {
            let field = Field::new(*name, column.data_type().clone(), true);
            match native {
                Some(native) => field.with_metadata(HashMap::from([(
                    "dbdelve.type".to_string(),
                    native.to_string(),
                )])),
                None => field,
            }
        })
        .collect();
    let columns = columns.into_iter().map(|(_, column, _)| column).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

/// Two Int64 rows under compression metadata. Built by hand: arrow-ipc's writer
/// cannot compress without the features this workspace leaves off.
fn compressed() -> Vec<u8> {
    let body: Vec<u8> = [1i64, 2].iter().flat_map(|n| n.to_le_bytes()).collect();
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let nodes = fbb.create_vector(&[FieldNode::new(2, 0)]);
    let buffers = fbb.create_vector(&[
        arrow_ipc::Buffer::new(0, 0),
        arrow_ipc::Buffer::new(0, body.len() as i64),
    ]);
    let mut compression = BodyCompressionBuilder::new(&mut fbb);
    compression.add_codec(CompressionType::LZ4_FRAME);
    compression.add_method(BodyCompressionMethod::BUFFER);
    let compression = compression.finish();
    let mut batch = RecordBatchBuilder::new(&mut fbb);
    batch.add_length(2);
    batch.add_nodes(nodes);
    batch.add_buffers(buffers);
    batch.add_compression(compression);
    let batch = batch.finish();
    let mut message = MessageBuilder::new(&mut fbb);
    message.add_version(MetadataVersion::V5);
    message.add_header_type(MessageHeader::RecordBatch);
    message.add_header(batch.as_union_value());
    message.add_bodyLength(body.len() as i64);
    let message = message.finish();
    fbb.finish(message, None);

    let mut header = fbb.finished_data().to_vec();
    header.resize(header.len().next_multiple_of(8), 0);
    let mut payload = [0xff; 4].to_vec();
    payload.extend((header.len() as i32).to_le_bytes());
    payload.extend(header);
    payload.extend(body);
    payload
}

/// The dictionary batch arrow-ipc's writer sends ahead of a dictionary-encoded
/// column, which protocol 1 does not carry.
fn dictionary() -> Vec<u8> {
    let column: DictionaryArray<types::Int32Type> = vec!["a", "b", "a"].into_iter().collect();
    let batch = RecordBatch::try_from_iter([("d", Arc::new(column) as ArrayRef)]).unwrap();
    let options = IpcWriteOptions::default();
    let generator = IpcDataGenerator::default();
    let mut tracker = DictionaryTracker::new(false);
    generator.schema_to_bytes_with_dictionary_tracker(&batch.schema(), &mut tracker, &options);
    let (dictionaries, _) = generator
        .encode(
            &batch,
            &mut tracker,
            &options,
            &mut IpcWriteContext::default(),
        )
        .unwrap();
    let mut payload = Vec::new();
    let dictionary = dictionaries.into_iter().next().unwrap();
    write_message(&mut payload, dictionary, &options).unwrap();
    payload
}

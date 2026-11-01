//! A query: run as typed, its first result set streamed under credit, the rest
//! counted.

use std::collections::HashMap;
use std::io::Write;
use std::mem;
use std::process;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dbdelve_plugin_protocol::arrow::{encode_batch, encode_schema};
use dbdelve_plugin_protocol::limits::{BATCH_AGE, BATCH_ROWS, BATCH_TARGET, WINDOW};
use dbdelve_plugin_protocol::{ErrorKind, Kind, MAX_PAYLOAD, Response, write_frame};
use odbc_api::handles::StatementImpl;
use odbc_api::sys::{SqlDataType, SqlReturn};

use crate::connect::Session;
use crate::failure;
use crate::raw::{self, DataError, Fetched};

/// So a driver that warns on every row cannot make `end` the size of a result.
const MOST_MESSAGES: usize = 16;

/// What one `SQLGetData` call reads at most; a longer value takes several.
const CHUNK: usize = 64 << 10;

/// The running query's credit and interruptions, shared between the stdin
/// thread, which applies `credit`, `stop` and `cancel` as they arrive, and the
/// worker, which waits on them.
#[derive(Default)]
pub struct Control {
    query: Mutex<Running>,
    wake: Condvar,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interrupt {
    Stop,
    Cancel,
}

impl Control {
    /// Called by the stdin thread before the query reaches the worker, so a
    /// `stop` or `cancel` sent right behind it is never applied to the one
    /// before.
    pub fn start(&self, id: u64) {
        *self.query.lock().unwrap() = Running {
            id: Some(id),
            left: WINDOW as i64,
            stop: false,
            cancel: false,
        };
    }

    pub fn credit(&self, id: u64, bytes: u64) {
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        self.update(id, |running| {
            running.left = running.left.saturating_add(bytes)
        });
    }

    pub fn stop(&self, id: u64) {
        self.update(id, |running| running.stop = true);
    }

    pub fn cancel(&self, id: u64) {
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
    fn take_credit(&self, size: usize) -> Result<(), Interrupt> {
        let size = i64::try_from(size).unwrap_or(i64::MAX);
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

    fn interrupted(&self) -> Option<Interrupt> {
        let running = self.query.lock().unwrap();
        match (running.cancel, running.stop) {
            (true, _) => Some(Interrupt::Cancel),
            (false, true) => Some(Interrupt::Stop),
            (false, false) => None,
        }
    }

    fn cancelled(&self) -> bool {
        self.query.lock().unwrap().cancel
    }
}

/// Runs `sql` as typed and answers with its terminal response, having written
/// the first result set's frames to `out` on the way.
pub fn query(
    session: &Session,
    control: &Control,
    out: &mut impl Write,
    id: u64,
    sql: &str,
) -> Response {
    let statement = match session.handle().allocate_statement() {
        odbc_api::handles::SqlResult::Success(statement)
        | odbc_api::handles::SqlResult::SuccessWithInfo(statement) => statement,
        _ => return failure::from(session.handle(), "SQLAllocHandle"),
    };
    if !raw::set_noscan(&statement) {
        return failure::from(&statement, "SQLSetStmtAttr");
    }
    // A driver that cannot time a statement out still runs it, and the user's
    // Cancel still reaches it.
    if session.statement_timeout() > 0 {
        let _ = raw::set_query_timeout(&statement, session.statement_timeout());
    }
    Run {
        statement: &statement,
        control,
        out,
        id,
        messages: Vec::new(),
    }
    .run(sql)
}

struct Run<'a, W: Write> {
    statement: &'a StatementImpl<'a>,
    control: &'a Control,
    out: &'a mut W,
    id: u64,
    messages: Vec<String>,
}

impl<W: Write> Run<'_, W> {
    fn run(mut self, sql: &str) -> Response {
        match raw::exec_direct(self.statement, sql) {
            SqlReturn::SUCCESS => {}
            SqlReturn::SUCCESS_WITH_INFO => self.note(),
            SqlReturn::NO_DATA => return self.end(0, Some(0), 0),
            _ => return self.failure("SQLExecDirect"),
        }
        let mut rows = None;
        let mut rows_affected = None;
        let mut more_sets = 0;
        loop {
            let Some(columns) = raw::num_result_cols(self.statement) else {
                return self.failure("SQLNumResultCols");
            };
            match (columns, rows) {
                (0, None) if rows_affected.is_none() => {
                    rows_affected = raw::row_count(self.statement);
                }
                (0, _) => {}
                (_, None) => match self.stream(columns) {
                    Ok(total) => rows = Some(total),
                    Err(response) => return response,
                },
                (_, Some(_)) => more_sets += 1,
            }
            match raw::more_results(self.statement) {
                SqlReturn::SUCCESS => {}
                SqlReturn::SUCCESS_WITH_INFO => self.note(),
                SqlReturn::NO_DATA => break,
                _ => return self.failure("SQLMoreResults"),
            }
        }
        if rows.is_some() {
            rows_affected = None;
        }
        self.end(rows.unwrap_or(0), rows_affected, more_sets)
    }

    /// Streams one result set and answers how many rows it produced. On `stop`
    /// nothing more is sent, and the rest is fetched without being read, so
    /// the count is the whole statement's: ODBC cannot tell whether ending the
    /// read early would keep the user's transaction, so it is never ended.
    fn stream(&mut self, columns: u16) -> Result<u64, Response> {
        let mut fields = Vec::new();
        let mut fetched = Vec::new();
        for column in 1..=columns {
            let Some((name, data_type)) = raw::describe_col(self.statement, column) else {
                return Err(self.failure("SQLDescribeCol"));
            };
            let Some(type_name) = raw::type_name(self.statement, column) else {
                return Err(self.failure("SQLColAttribute"));
            };
            fetched.push(fetched_as(data_type));
            fields.push(column_field(name, type_name));
        }
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let mut draining = self.send(&encode_schema(&schema))?;

        let mut total = 0;
        let mut batcher = Batcher::default();
        let mut scratch = vec![0; CHUNK];
        loop {
            match self.control.interrupted() {
                Some(Interrupt::Cancel) => return Err(self.cancelled()),
                Some(Interrupt::Stop) => draining = true,
                None => {}
            }
            match raw::fetch(self.statement) {
                SqlReturn::SUCCESS => {}
                SqlReturn::SUCCESS_WITH_INFO => self.note(),
                SqlReturn::NO_DATA => break,
                _ => return Err(self.failure("SQLFetch")),
            }
            total += 1;
            if draining {
                continue;
            }
            let row = self.row(&schema, &fetched, &mut scratch)?;
            for batch in batcher.push(row, Instant::now()) {
                if self.send_batch(&schema, batch)? {
                    draining = true;
                    break;
                }
            }
        }
        if !draining && let Some(batch) = batcher.finish() {
            self.send_batch(&schema, batch)?;
        }
        Ok(total)
    }

    fn row(
        &mut self,
        schema: &Schema,
        fetched: &[Fetched],
        scratch: &mut [u8],
    ) -> Result<Vec<Option<String>>, Response> {
        let mut row = Vec::with_capacity(fetched.len());
        for (index, &as_fetched) in fetched.iter().enumerate() {
            let column = index as u16 + 1;
            let limit = match as_fetched {
                Fetched::Text => 2 * MAX_PAYLOAD,
                Fetched::Binary => (MAX_PAYLOAD - 2) / 2,
            };
            let value = match raw::get_data(self.statement, column, as_fetched, limit, scratch) {
                Ok(value) => value.map(|bytes| rendered(as_fetched, &bytes)),
                Err(DataError::Odbc) => return Err(self.failure("SQLGetData")),
                Err(DataError::TooLarge) => return Err(too_large(schema, index)),
            };
            if value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_PAYLOAD)
            {
                return Err(too_large(schema, index));
            }
            row.push(value);
        }
        Ok(row)
    }

    /// `Ok(true)` when a `stop` arrived instead of the credit.
    fn send_batch(
        &mut self,
        schema: &SchemaRef,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<bool, Response> {
        let batch = batch(schema, rows);
        let payload = match encode_batch(&batch) {
            Ok(payload) if payload.len() <= MAX_PAYLOAD => payload,
            _ => {
                return Err(failure::plain(
                    "A row of this result is larger than 64 MiB, which protocol 1 cannot carry.",
                ));
            }
        };
        self.send(&payload)
    }

    /// `Ok(true)` when a `stop` arrived instead of the credit.
    fn send(&mut self, payload: &[u8]) -> Result<bool, Response> {
        match self.control.take_credit(payload.len()) {
            Ok(()) => {
                write_or_exit(self.out, Kind::Data, self.id, payload);
                Ok(false)
            }
            Err(Interrupt::Stop) => Ok(true),
            Err(Interrupt::Cancel) => Err(self.cancelled()),
        }
    }

    /// Keeps the warnings the last call left, as `end.messages`.
    fn note(&mut self) {
        for (_, message) in failure::records(self.statement) {
            if self.messages.len() < MOST_MESSAGES && !self.messages.contains(&message) {
                self.messages.push(message);
            }
        }
    }

    /// The last call's error, as cancelled when the user's Cancel arrived. The
    /// flag decides, not the SQLSTATE: a driver may word a cancel its own way,
    /// and Db2 reports its query timeout as the same `HY008` a cancel gets.
    fn failure(&self, function: &str) -> Response {
        let mut response = failure::from(self.statement, function);
        if self.control.cancelled()
            && let Response::Error { kind, .. } = &mut response
        {
            *kind = ErrorKind::Cancelled;
        }
        response
    }

    fn cancelled(&self) -> Response {
        Response::Error {
            kind: ErrorKind::Cancelled,
            message: "The statement was cancelled.".to_string(),
            sqlstate: None,
            position: None,
            lost: false,
        }
    }

    fn end(&mut self, rows: u64, rows_affected: Option<u64>, more_sets: u32) -> Response {
        Response::End {
            rows,
            rows_affected,
            stopped: false,
            messages: mem::take(&mut self.messages),
            more_sets,
        }
    }
}

/// Binary types are read as bytes and shown as `0x…`; every other type as wide
/// text, converted by the driver, so a client code page cannot mangle it. Only
/// ODBC's own binary codes count: a driver's extension code means nothing to
/// another driver, so Db2's `BLOB` (IBM's -98) arrives in the driver's own
/// character form, hex without the `0x`.
fn fetched_as(data_type: i16) -> Fetched {
    match SqlDataType(data_type) {
        SqlDataType::EXT_BINARY
        | SqlDataType::EXT_VAR_BINARY
        | SqlDataType::EXT_LONG_VAR_BINARY => Fetched::Binary,
        _ => Fetched::Text,
    }
}

fn column_field(name: String, type_name: String) -> Field {
    Field::new(name, DataType::Utf8, true)
        .with_metadata(HashMap::from([("dbdelve.type".to_string(), type_name)]))
}

fn rendered(fetched: Fetched, bytes: &[u8]) -> String {
    match fetched {
        Fetched::Text => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        Fetched::Binary => {
            let mut text = String::with_capacity(2 + bytes.len() * 2);
            text.push_str("0x");
            for byte in bytes {
                text.push_str(&format!("{byte:02X}"));
            }
            text
        }
    }
}

fn too_large(schema: &Schema, index: usize) -> Response {
    failure::plain(&format!(
        "A value in column {} is larger than 64 MiB, which protocol 1 cannot carry.",
        schema.field(index).name()
    ))
}

fn batch(schema: &SchemaRef, rows: Vec<Vec<Option<String>>>) -> RecordBatch {
    let mut builders: Vec<StringBuilder> = schema
        .fields()
        .iter()
        .map(|_| StringBuilder::new())
        .collect();
    for row in rows {
        for (builder, value) in builders.iter_mut().zip(row) {
            builder.append_option(value);
        }
    }
    let columns: Vec<ArrayRef> = builders
        .into_iter()
        .map(|mut builder| Arc::new(builder.finish()) as ArrayRef)
        .collect();
    RecordBatch::try_new(Arc::clone(schema), columns).expect("one Utf8 column per field")
}

pub fn write_or_exit(out: &mut impl Write, kind: Kind, id: u64, payload: &[u8]) {
    // The host has gone, or killed this process mid-write: nothing is left to
    // answer.
    if write_frame(out, kind, id, payload).is_err() {
        process::exit(1);
    }
}

/// Gathers rows into batches by the protocol's guideline: a batch closes at
/// `BATCH_ROWS` rows, `BATCH_TARGET` bytes, or `BATCH_AGE` after its first row,
/// so first rows show quickly. A row that would take a batch past the byte
/// target starts the next one, so a row larger than the target travels alone.
///
/// ponytail: the age is checked as each row arrives, so a fetch that blocks
/// holds the rows before it until it returns; a timer thread lifts that if a
/// driver trickles rows.
#[derive(Default)]
struct Batcher {
    rows: Vec<Vec<Option<String>>>,
    bytes: usize,
    since: Option<Instant>,
}

impl Batcher {
    fn push(&mut self, row: Vec<Option<String>>, now: Instant) -> Vec<Vec<Vec<Option<String>>>> {
        let size: usize = row.iter().flatten().map(String::len).sum();
        let mut closed = Vec::new();
        if !self.rows.is_empty() && self.bytes + size > BATCH_TARGET {
            closed.extend(self.finish());
        }
        self.since.get_or_insert(now);
        self.rows.push(row);
        self.bytes += size;
        let aged = self
            .since
            .is_some_and(|since| now.duration_since(since) >= BATCH_AGE);
        if self.rows.len() >= BATCH_ROWS || self.bytes >= BATCH_TARGET || aged {
            closed.extend(self.finish());
        }
        closed
    }

    fn finish(&mut self) -> Option<Vec<Vec<Option<String>>>> {
        self.bytes = 0;
        self.since = None;
        match self.rows.is_empty() {
            true => None,
            false => Some(mem::take(&mut self.rows)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(value: &str) -> Vec<Option<String>> {
        vec![Some(value.to_string()), None]
    }

    #[test]
    fn a_batch_closes_at_its_row_limit() {
        let mut batcher = Batcher::default();
        let now = Instant::now();
        for _ in 0..BATCH_ROWS - 1 {
            assert!(batcher.push(row("x"), now).is_empty());
        }
        let closed = batcher.push(row("x"), now);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].len(), BATCH_ROWS);
        assert_eq!(batcher.finish(), None);
    }

    #[test]
    fn a_batch_closes_at_its_byte_target_and_a_large_row_travels_alone() {
        let mut batcher = Batcher::default();
        let now = Instant::now();
        assert!(batcher.push(row("small"), now).is_empty());
        let large = "x".repeat(BATCH_TARGET);
        let closed = batcher.push(row(&large), now);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0], vec![row("small")]);
        assert_eq!(closed[1], vec![row(&large)]);
    }

    #[test]
    fn a_batch_closes_once_its_first_row_has_waited_long_enough() {
        let mut batcher = Batcher::default();
        let start = Instant::now();
        assert!(batcher.push(row("a"), start).is_empty());
        let closed = batcher.push(row("b"), start + BATCH_AGE + Duration::from_millis(1));
        assert_eq!(closed, vec![vec![row("a"), row("b")]]);
    }

    #[test]
    fn credit_follows_the_hosts_rule_and_goes_below_zero_for_one_large_frame() {
        let control = Control::default();
        control.start(7);
        let window = WINDOW as usize;
        assert_eq!(control.take_credit(window + 1), Ok(()));
        control.credit(7, window as u64 + 1);
        assert_eq!(control.take_credit(window - 1), Ok(()));
        // One byte left: only a one-byte frame fits now.
        assert_eq!(control.take_credit(1), Ok(()));
        control.stop(7);
        assert_eq!(control.take_credit(1), Err(Interrupt::Stop));
    }

    #[test]
    fn credit_stop_and_cancel_for_another_query_are_ignored() {
        let control = Control::default();
        control.start(2);
        control.stop(1);
        control.cancel(1);
        assert_eq!(control.interrupted(), None);
        control.cancel(2);
        assert_eq!(control.interrupted(), Some(Interrupt::Cancel));
    }

    #[test]
    fn binary_is_shown_as_hex_and_text_is_read_as_utf_16() {
        assert_eq!(rendered(Fetched::Binary, &[0x00, 0xff]), "0x00FF");
        let text: Vec<u8> = "Zoë 李 🐉"
            .encode_utf16()
            .flat_map(u16::to_ne_bytes)
            .collect();
        assert_eq!(rendered(Fetched::Text, &text), "Zoë 李 🐉");
    }

    /// Review focus 9, at grep level: every value is read through `raw.rs`.
    #[test]
    fn no_odbc_api_value_reader_is_used_outside_raw() {
        let sources = [
            ("main.rs", include_str!("main.rs")),
            ("connect.rs", include_str!("connect.rs")),
            ("failure.rs", include_str!("failure.rs")),
            ("run.rs", include_str!("run.rs")),
        ];
        // Spelled in halves so this list does not match itself.
        let banned = [
            concat!("TextRow", "Set"),
            concat!("Columnar", "Buffer"),
            concat!("Null", "able"),
            concat!("get_", "text"),
            concat!("get_", "binary"),
            concat!("row_", "count("),
            concat!("bind_", "col"),
        ];
        for (file, source) in sources {
            let source = source.replace(concat!("raw::row_", "count("), "");
            for word in banned {
                assert!(!source.contains(word), "{file} names {word}");
            }
        }
    }
}

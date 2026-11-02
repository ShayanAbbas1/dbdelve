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
use dbdelve_plugin_protocol::limits::{
    BATCH_AGE, BATCH_ROWS, BATCH_TARGET, MAX_BATCH_CELLS, WINDOW, credit_cost,
};
use dbdelve_plugin_protocol::{ErrorKind, Kind, MAX_PAYLOAD, Response, write_frame};
use odbc_api::handles::{Statement as _, StatementImpl};
use odbc_api::sys::{HStmt, SqlDataType, SqlReturn};

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
    statement: Option<Published>,
}

/// The handle of the statement the worker is running, for `cancel` to reach
/// from the stdin thread.
#[derive(Clone, Copy)]
struct Published(HStmt);

// SAFETY: the handle is only ever passed to SQLCancel, which ODBC allows from
// another thread, and only under the lock the worker takes to unpublish it
// before the handle is freed.
unsafe impl Send for Published {}

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
            statement: None,
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

    /// Wakes a worker waiting for credit, and interrupts a driver call in
    /// progress with `SQLCancel`, called here on the stdin thread.
    pub fn cancel(&self, id: u64) {
        let mut running = self.query.lock().unwrap();
        if running.id != Some(id) {
            return;
        }
        running.cancel = true;
        self.wake.notify_all();
        if let Some(Published(statement)) = running.statement {
            // SAFETY: the worker unpublishes the handle under this lock before
            // freeing it, and the lock is held across the call, so the handle
            // is live until it returns.
            unsafe { raw::cancel(statement) }
        }
    }

    /// Runs `call`, one that may wait on the server, with `statement`
    /// published for `cancel` to interrupt. `None` when the request was
    /// cancelled before the call could start, so it never does.
    ///
    /// Only these calls are published. unixODBC frees a statement's diagnostic
    /// records on entry to every call, `SQLCancel`'s included, with no lock, so
    /// two threads in calls on one statement can free the same records twice.
    /// So the records are emptied here first, while nothing else can reach the
    /// handle, and they are read only after `call` returns and the handle is
    /// unpublished, which `cancel`'s lock makes the same as reading them under
    /// it.
    ///
    /// ponytail: two races are left. A `cancel` landing as `call` returns,
    /// while the driver manager records its diagnostics, races those records,
    /// a window of the driver manager's own bookkeeping. One landing before
    /// `call` enters the driver finds nothing running, and the statement runs
    /// to its end unless the host's cancel deadline kills the bridge first.
    /// Closing both needs the driver's async mode.
    pub fn published<T>(&self, statement: &StatementImpl, call: impl FnOnce() -> T) -> Option<T> {
        raw::clear_diagnostics(statement);
        {
            let mut running = self.query.lock().unwrap();
            if running.cancel {
                return None;
            }
            running.statement = Some(Published(statement.as_sys()));
        }
        let result = call();
        self.query.lock().unwrap().statement = None;
        Some(result)
    }

    /// Interrupts the server's side of a cancelled request whose Cancel found
    /// the statement unpublished, so freeing it does not wait for the server
    /// to finish. Called on the worker, the handle's only user by then.
    pub fn settle(&self, statement: &StatementImpl) {
        if self.cancelled() {
            // SAFETY: the statement is live, borrowed for the call, and
            // unpublished, so no other thread uses it.
            unsafe { raw::cancel(statement.as_sys()) }
        }
    }

    /// The last call's error, as cancelled when the user's Cancel arrived. The
    /// flag decides, not the SQLSTATE: a driver may word a cancel its own way,
    /// and Db2 reports its query timeout as the same `HY008` a cancel gets.
    pub fn failure(&self, statement: &StatementImpl, function: &str) -> Response {
        let mut response = failure::from(statement, function);
        if self.cancelled()
            && let Response::Error { kind, .. } = &mut response
        {
            *kind = ErrorKind::Cancelled;
        }
        response
    }

    fn update(&self, id: u64, change: impl FnOnce(&mut Running)) {
        let mut running = self.query.lock().unwrap();
        if running.id == Some(id) {
            change(&mut running);
            self.wake.notify_all();
        }
    }

    /// The host's rule exactly: a frame goes when its cost (`credit_cost`, the
    /// payload and a flat per-frame charge) fits in what is left, or alone
    /// when nothing is outstanding.
    fn take_credit(&self, payload: usize) -> Result<(), Interrupt> {
        let cost = i64::try_from(credit_cost(payload)).unwrap_or(i64::MAX);
        let mut running = self.query.lock().unwrap();
        loop {
            if running.cancel {
                return Err(Interrupt::Cancel);
            }
            if running.stop {
                return Err(Interrupt::Stop);
            }
            if cost <= running.left || running.left == WINDOW as i64 {
                running.left -= cost;
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

    pub fn cancelled(&self) -> bool {
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
    let statement = match session.statement() {
        Ok(statement) => statement,
        Err(error) => return error,
    };
    let response = Run {
        statement: &statement,
        control,
        out,
        id,
        messages: Vec::new(),
    }
    .run(sql);
    control.settle(&statement);
    response
}

struct Run<'a, W: Write> {
    statement: &'a StatementImpl<'a>,
    control: &'a Control,
    out: &'a mut W,
    id: u64,
    messages: Vec<String>,
}

/// What a statement's results add up to: the first result set's rows, the
/// counts of every statement that returned none, and the sets protocol 1
/// does not carry.
#[derive(Default)]
struct Tally {
    rows: Option<u64>,
    affected: Option<u64>,
    more_sets: u32,
}

impl Tally {
    /// A count the driver could not give (`-1`, as DDL reports) adds nothing.
    fn count(&mut self, affected: Option<u64>) {
        if let Some(affected) = affected {
            self.affected = Some(self.affected.unwrap_or(0).saturating_add(affected));
        }
    }
}

impl<W: Write> Run<'_, W> {
    fn run(mut self, sql: &str) -> Response {
        let statement = self.statement;
        let Some(executed) = self
            .control
            .published(statement, || raw::exec_direct(statement, sql))
        else {
            return cancelled();
        };
        let mut has_result = match executed {
            SqlReturn::SUCCESS => true,
            SqlReturn::SUCCESS_WITH_INFO => {
                self.note();
                true
            }
            // A driver may answer a statement that touched no rows this way,
            // and the statements after it still wait behind `SQLMoreResults`.
            SqlReturn::NO_DATA => false,
            _ => return self.failure("SQLExecDirect"),
        };
        let mut tally = Tally::default();
        loop {
            match has_result {
                true => {
                    if let Err(response) = self.result(&mut tally) {
                        return response;
                    }
                }
                false => tally.count(Some(0)),
            }
            let Some(next) = self
                .control
                .published(statement, || raw::more_results(statement))
            else {
                return cancelled();
            };
            has_result = match next {
                SqlReturn::SUCCESS => true,
                SqlReturn::SUCCESS_WITH_INFO => {
                    self.note();
                    true
                }
                // A cancel between statements leaves unixODBC answering
                // NO_DATA for the ones it never ran.
                SqlReturn::NO_DATA if self.control.cancelled() => return cancelled(),
                SqlReturn::NO_DATA => break,
                _ => return self.failure("SQLMoreResults"),
            };
        }
        let affected = match tally.rows {
            Some(_) => None,
            None => tally.affected,
        };
        self.end(tally.rows.unwrap_or(0), affected, tally.more_sets)
    }

    fn result(&mut self, tally: &mut Tally) -> Result<(), Response> {
        let Some(columns) = raw::num_result_cols(self.statement) else {
            return Err(self.failure("SQLNumResultCols"));
        };
        match (columns, tally.rows) {
            (0, _) => tally.count(raw::row_count(self.statement)),
            (_, None) => tally.rows = Some(self.stream(columns)?),
            (_, Some(_)) => tally.more_sets += 1,
        }
        Ok(())
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
        let mut batcher = Batcher::new(fetched.len());
        let mut scratch = vec![0; CHUNK];
        loop {
            match self.control.interrupted() {
                Some(Interrupt::Cancel) => return Err(cancelled()),
                Some(Interrupt::Stop) => draining = true,
                None => {}
            }
            let statement = self.statement;
            let Some(fetched_row) = self.control.published(statement, || raw::fetch(statement))
            else {
                return Err(cancelled());
            };
            match fetched_row {
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
            let control = self.control;
            let cancelled_now = || control.cancelled();
            let read = raw::get_data(
                self.statement,
                column,
                as_fetched,
                limit,
                scratch,
                &cancelled_now,
            );
            let value = match read {
                Ok(value) => value.map(|bytes| rendered(as_fetched, &bytes)),
                Err(DataError::Odbc) => return Err(self.failure("SQLGetData")),
                Err(DataError::TooLarge) => return Err(too_large(schema, index)),
                Err(DataError::Cancelled) => return Err(cancelled()),
                Err(DataError::Unmeasured) => return Err(unmeasured(schema, index)),
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
            Err(Interrupt::Cancel) => Err(cancelled()),
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

    fn failure(&self, function: &str) -> Response {
        self.control.failure(self.statement, function)
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

pub fn rendered(fetched: Fetched, bytes: &[u8]) -> String {
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

pub fn cancelled() -> Response {
    Response::Error {
        kind: ErrorKind::Cancelled,
        message: "The statement was cancelled.".to_string(),
        sqlstate: None,
        position: None,
        lost: false,
    }
}

fn too_large(schema: &Schema, index: usize) -> Response {
    failure::plain(&format!(
        "A value in column {} is larger than 64 MiB, which protocol 1 cannot carry.",
        schema.field(index).name()
    ))
}

fn unmeasured(schema: &Schema, index: usize) -> Response {
    failure::plain(&format!(
        "The ODBC driver did not say how long a value in column {} is, so the bridge cannot tell where it ends.",
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
/// A wide result closes sooner, at the rows `MAX_BATCH_CELLS` allows, which
/// the protocol enforces.
///
/// ponytail: the age is checked as each row arrives, so a fetch that blocks
/// holds the rows before it until it returns; a timer thread lifts that if a
/// driver trickles rows.
struct Batcher {
    most_rows: usize,
    rows: Vec<Vec<Option<String>>>,
    bytes: usize,
    since: Option<Instant>,
}

impl Batcher {
    fn new(columns: usize) -> Self {
        Self {
            most_rows: BATCH_ROWS.min(MAX_BATCH_CELLS / columns.max(1)),
            rows: Vec::new(),
            bytes: 0,
            since: None,
        }
    }

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
        if self.rows.len() >= self.most_rows || self.bytes >= BATCH_TARGET || aged {
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
    use dbdelve_plugin_protocol::limits::FRAME_COST;
    use std::time::Duration;

    fn row(value: &str) -> Vec<Option<String>> {
        vec![Some(value.to_string()), None]
    }

    #[test]
    fn a_batch_closes_at_its_row_limit() {
        let mut batcher = Batcher::new(1);
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
    fn a_wide_result_closes_its_batches_within_the_cell_budget() {
        let columns = 4096;
        let mut batcher = Batcher::new(columns);
        let now = Instant::now();
        let most = MAX_BATCH_CELLS / columns;
        for _ in 0..most - 1 {
            assert!(batcher.push(row("x"), now).is_empty());
        }
        assert_eq!(batcher.push(row("x"), now)[0].len(), most);
    }

    #[test]
    fn a_batch_closes_at_its_byte_target_and_a_large_row_travels_alone() {
        let mut batcher = Batcher::new(1);
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
        let mut batcher = Batcher::new(1);
        let start = Instant::now();
        assert!(batcher.push(row("a"), start).is_empty());
        let closed = batcher.push(row("b"), start + BATCH_AGE + Duration::from_millis(1));
        assert_eq!(closed, vec![vec![row("a"), row("b")]]);
    }

    #[test]
    fn credit_follows_the_hosts_rule_and_goes_below_zero_for_one_large_frame() {
        let control = Control::default();
        control.start(7);
        let (window, frame) = (WINDOW as usize, FRAME_COST as usize);
        assert_eq!(control.take_credit(window + 1), Ok(()));
        control.credit(7, credit_cost(window + 1));
        assert_eq!(control.take_credit(window - 2 * frame - 1), Ok(()));
        // A one-byte frame costs exactly what is left.
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
            ("catalog.rs", include_str!("catalog.rs")),
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

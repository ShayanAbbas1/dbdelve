//! A query: run as typed, its first result set streamed under credit, the rest
//! counted.

use std::collections::HashMap;
use std::io::Write;
use std::mem;
use std::process;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use arrow_array::builder::{BinaryBuilder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dbdelve_plugin_protocol::arrow::{encode_batch, encode_schema};
use dbdelve_plugin_protocol::limits::{
    BATCH_AGE, BATCH_ROWS, BATCH_TARGET, MAX_BATCH_CELLS, WINDOW, credit_cost,
};
use dbdelve_plugin_protocol::{
    BASE_COLUMN, BASE_SCHEMA, BASE_TABLE, EXACT, ErrorKind, Kind, MAX_PAYLOAD, Response,
    write_frame,
};
use odbc_api::handles::{Statement as _, StatementImpl};
use odbc_api::sys::{Desc, HStmt, SqlDataType, SqlReturn};

use crate::connect::Session;
use crate::failure;
use crate::raw::{self, Bound, BoundValue, DataError, Fetched, Record};

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
        reads_bound: session.reads_bound_columns(),
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
    /// Columns may be bound, since `SQLGetData` can still read one whose
    /// value outgrew its buffer.
    reads_bound: bool,
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
        let mut plan = Vec::new();
        for column in 1..=columns {
            let Some(described) = raw::describe_col(self.statement, column) else {
                return Err(self.failure("SQLDescribeCol"));
            };
            let Some(type_name) = raw::type_name(self.statement, column) else {
                return Err(self.failure("SQLColAttribute"));
            };
            let read = read_as(described.data_type, described.size, &type_name);
            if self.reads_bound
                && let Some(bound) = bound_as(read, described.data_type, described.size)
            {
                plan.push((column, bound));
            }
            fields.push(column_field(
                described.name,
                type_name,
                described.data_type,
                read,
                base(self.statement, column),
            ));
            fetched.push(read);
        }
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let mut draining = self.send(&encode_schema(&schema))?;
        let mut bindings = raw::Bindings::new(self.statement, &plan);
        let slots: Vec<Option<usize>> = (1..=columns)
            .map(|column| bindings.as_ref().and_then(|b| b.slot(column)))
            .collect();

        let mut total = 0;
        let mut batcher = Batcher::new(fetched.len());
        let mut scratch = vec![0; CHUNK];
        loop {
            match self.control.interrupted() {
                Some(Interrupt::Cancel) => return Err(cancelled()),
                Some(Interrupt::Stop) => draining = true,
                None => {}
            }
            if draining {
                // Unbound, so the rest is fetched without converting a value.
                bindings = None;
            }
            if let Some(bindings) = &mut bindings {
                bindings.clear();
            }
            let statement = self.statement;
            let Some(fetched_row) = self.control.published(statement, || raw::fetch(statement))
            else {
                return Err(cancelled());
            };
            match fetched_row {
                SqlReturn::SUCCESS => {}
                SqlReturn::SUCCESS_WITH_INFO => self.note_fetched(bindings.is_some()),
                SqlReturn::NO_DATA => break,
                _ => return Err(self.failure("SQLFetch")),
            }
            total += 1;
            if draining {
                continue;
            }
            let bound = bindings
                .as_ref()
                .map(|bindings| (bindings, slots.as_slice()));
            let row = self.row(&schema, &fetched, bound, &mut scratch)?;
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
        fetched: &[Read],
        bound: Option<(&raw::Bindings, &[Option<usize>])>,
        scratch: &mut [u8],
    ) -> Result<Vec<Option<Cell>>, Response> {
        let mut row = Vec::with_capacity(fetched.len());
        for (index, &read) in fetched.iter().enumerate() {
            let column = index as u16 + 1;
            let slot =
                bound.and_then(|(bindings, slots)| slots[index].map(|slot| (bindings, slot)));
            let value = match (slot, read) {
                (Some((bindings, slot)), _) => match bindings.value(slot) {
                    BoundValue::Null => Ok(None),
                    BoundValue::Text(bytes) => Ok(Some(Cell::Text(wide_text(bytes)))),
                    BoundValue::Truncated => self.chunked(column, Fetched::Text, scratch),
                    BoundValue::Double(value) => Ok(Some(Cell::Text(shown_f64(value)))),
                    BoundValue::Real(value) => Ok(Some(Cell::Text(shown_f32(value)))),
                },
                (None, read) => match read {
                    Read::Double => raw::get_double(self.statement, column)
                        .map(|v| v.map(|v| Cell::Text(shown_f64(v)))),
                    Read::Real => raw::get_real(self.statement, column)
                        .map(|v| v.map(|v| Cell::Text(shown_f32(v)))),
                    Read::Chunked(as_fetched) => self.chunked(column, as_fetched, scratch),
                    Read::ImplicitXml => self
                        .chunked(column, Fetched::Text, scratch)
                        .map(|v| v.map(Cell::undeclared)),
                },
            };
            let value = match value {
                Ok(value) => value,
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

    fn chunked(
        &self,
        column: u16,
        as_fetched: Fetched,
        scratch: &mut [u8],
    ) -> Result<Option<Cell>, DataError> {
        let limit = match as_fetched {
            Fetched::Text => 2 * MAX_PAYLOAD,
            Fetched::Binary => MAX_PAYLOAD,
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
        )?;
        Ok(read.map(|bytes| match as_fetched {
            Fetched::Text => Cell::Text(wide_text(&bytes)),
            Fetched::Binary => Cell::Bytes(bytes),
        }))
    }

    /// `Ok(true)` when a `stop` arrived instead of the credit.
    fn send_batch(
        &mut self,
        schema: &SchemaRef,
        rows: Vec<Vec<Option<Cell>>>,
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
        self.keep(failure::records(self.statement));
    }

    /// As `note`, but a fetch into bound columns warns `01004` for each value
    /// that outgrew its buffer, which is then read whole, so that is no
    /// message.
    fn note_fetched(&mut self, bound: bool) {
        let records = failure::records(self.statement)
            .into_iter()
            .filter(|record| !(bound && record.state == "01004"))
            .collect();
        self.keep(records);
    }

    fn keep(&mut self, records: Vec<Record>) {
        keep(&mut self.messages, &records);
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

/// Adds the records' messages to `messages`, each once, up to `MOST_MESSAGES`.
pub fn keep(messages: &mut Vec<String>, records: &[Record]) {
    for message in failure::messages(records) {
        if messages.len() < MOST_MESSAGES && !messages.iter().any(|kept| kept == message) {
            messages.push(message.to_string());
        }
    }
}

/// How a column's values are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Chunked(Fetched),
    Double,
    Real,
    /// Text the driver serialized from a type that keeps no declaration of
    /// its own.
    ImplicitXml,
}

/// IBM's `SQL_XML`. Db2 stores an XML value parsed and keeps no declaration,
/// and its driver adds a byte-order mark and one (`encoding="UTF-16"`) when it
/// serializes the value, so both are the driver's and are taken off.
const IBM_XML: i16 = -370;

/// The widest an approximate numeric may be, in either of the radixes a driver
/// sizes it in (`DOUBLE` as 15 or 17 digits, or 53 bits), and still fit a
/// `double`; past it the type is a decimal reported as a float, as Oracle's
/// `FLOAT(126)` is, which only its text form keeps whole.
const DOUBLE_SIZE: usize = 53;
const REAL_SIZE: usize = 24;

/// Approximate numerics are read as binary floats, whose shortest round-trip
/// form is exact where a driver's text may round (Db2 shows a `DOUBLE` to 15
/// digits). Everything else is read as `fetched_as` says.
fn read_as(data_type: i16, size: usize, type_name: &str) -> Read {
    match SqlDataType(data_type) {
        SqlDataType::DOUBLE | SqlDataType::FLOAT if size <= DOUBLE_SIZE => Read::Double,
        SqlDataType::REAL if size <= REAL_SIZE => Read::Real,
        _ if data_type == IBM_XML && type_name.eq_ignore_ascii_case("XML") => Read::ImplicitXml,
        _ => Read::Chunked(fetched_as(data_type, type_name)),
    }
}

/// How a column read with `SQLGetData` is bound instead, when it can be: a
/// float as itself, and text of a type ODBC defines in a buffer sized from the
/// column's declared size, which a value may still outgrow (psqlODBC declares
/// an unbounded `text` or `numeric` at a guess) and is then read again whole.
/// A driver's own type and binary stay unbound, read in chunks.
fn bound_as(read: Read, data_type: i16, size: usize) -> Option<Bound> {
    match read {
        Read::Double => Some(Bound::Double),
        Read::Real => Some(Bound::Real),
        Read::Chunked(Fetched::Text) if odbc_defines(data_type) => Some(Bound::Text(match size {
            0 => 1024,
            _ => size.saturating_mul(2).clamp(64, 8192),
        })),
        _ => None,
    }
}

/// Binary types are read as bytes and shown as `0x…`; every other type as wide
/// text, converted by the driver, so a client code page cannot mangle it.
///
/// A code ODBC does not define is the driver's own and means nothing to any
/// other driver, so its type name is all there is to go on: a driver's own
/// type named `BLOB` or `BINARY` is read as bytes too. Otherwise Db2's `BLOB`
/// (IBM's code -98) arrives in the driver's own character form, hex without
/// the `0x`, unlike every other binary column. The name is asked only of a
/// driver's own code, never of ODBC's: those already say what they are, and a
/// name like Oracle's `BINARY_DOUBLE` belongs to a float reported as one.
fn fetched_as(data_type: i16, type_name: &str) -> Fetched {
    match SqlDataType(data_type) {
        SqlDataType::EXT_BINARY
        | SqlDataType::EXT_VAR_BINARY
        | SqlDataType::EXT_LONG_VAR_BINARY => Fetched::Binary,
        _ if !odbc_defines(data_type) && names_binary(type_name) => Fetched::Binary,
        _ => Fetched::Text,
    }
}

/// ODBC 3.8's own SQL type codes: `SQL_GUID` (-11) to `SQL_VARCHAR` (12), the
/// datetime codes and the intervals.
fn odbc_defines(data_type: i16) -> bool {
    matches!(data_type, -11..=12 | 91..=95 | 101..=113)
}

fn names_binary(type_name: &str) -> bool {
    let name = type_name.to_ascii_uppercase();
    name.contains("BLOB") || name.contains("BINARY")
}

/// One value as it travels: text as Utf8, and bytes as Binary, which the host
/// shows as `0x` and hex, so a binary value's limit is its own size rather
/// than its hex's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    Text(String),
    Bytes(Vec<u8>),
}

impl Cell {
    fn len(&self) -> usize {
        match self {
            Cell::Text(text) => text.len(),
            Cell::Bytes(bytes) => bytes.len(),
        }
    }

    fn undeclared(self) -> Self {
        match self {
            Cell::Text(text) => Cell::Text(undeclared(&text).to_string()),
            bytes => bytes,
        }
    }
}

fn column_field(
    name: String,
    type_name: String,
    sql_type: i16,
    read: Read,
    base: Option<Base>,
) -> Field {
    let data_type = match read {
        Read::Chunked(Fetched::Binary) => DataType::Binary,
        _ => DataType::Utf8,
    };
    let exact = exact(sql_type, &type_name, read);
    let mut metadata = HashMap::from([("dbdelve.type".to_string(), type_name)]);
    if exact {
        metadata.insert(EXACT.to_string(), "true".to_string());
    }
    if let Some(base) = base {
        metadata.insert(BASE_TABLE.to_string(), base.table);
        metadata.insert(BASE_COLUMN.to_string(), base.column);
        if !base.schema.is_empty() {
            metadata.insert(BASE_SCHEMA.to_string(), base.schema);
        }
    }
    Field::new(name, data_type, true).with_metadata(metadata)
}

/// The table column a result column is read from, as the driver reports it.
#[derive(Debug, Default)]
struct Base {
    schema: String,
    table: String,
    column: String,
}

/// Only where the driver names a table as well as a column: psqlODBC repeats
/// an expression's label as its base column, `current_schema` included, and
/// names no table for it.
fn base(statement: &StatementImpl, column: u16) -> Option<Base> {
    let named = |field| raw::text_attribute(statement, column, field).filter(|n| !n.is_empty());
    Some(Base {
        table: named(Desc::BaseTableName)?,
        column: named(Desc::BaseColumnName)?,
        schema: named(Desc::SchemaName).unwrap_or_default(),
    })
}

/// Text and exact numbers read whole as the driver's text, which is the
/// server's own and reads back as the same value, so a row can be found by
/// it. Not floats, which the bridge reformats; nor dates, times and
/// intervals, which each driver renders its own way and psqlODBC alters;
/// nor booleans, which a driver may show as `1` for a `true`, even one it
/// reports as text (psqlODBC's `BoolsAsChar`, on by default, makes a `bool`
/// a varchar), so a boolean is told by its type name too; nor long text,
/// which a server may refuse to compare.
fn exact(sql_type: i16, type_name: &str, read: Read) -> bool {
    read == Read::Chunked(Fetched::Text)
        && !type_name.eq_ignore_ascii_case("bool")
        && !type_name.eq_ignore_ascii_case("boolean")
        && matches!(
            SqlDataType(sql_type),
            SqlDataType::CHAR
                | SqlDataType::VARCHAR
                | SqlDataType::EXT_W_CHAR
                | SqlDataType::EXT_W_VARCHAR
                | SqlDataType::EXT_TINY_INT
                | SqlDataType::SMALLINT
                | SqlDataType::INTEGER
                | SqlDataType::EXT_BIG_INT
                | SqlDataType::DECIMAL
                | SqlDataType::NUMERIC
                | SqlDataType::EXT_GUID
        )
}

/// UTF-16 code units, as `Fetched::Text` reads them.
pub fn wide_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&unit| u16::from_ne_bytes(unit))
        .collect();
    String::from_utf16_lossy(&units)
}

/// `text` without a leading byte-order mark and the XML declaration after it.
fn undeclared(text: &str) -> &str {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let declared = text
        .strip_prefix("<?xml")
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_whitespace()));
    match declared.and_then(|rest| rest.split_once("?>")) {
        Some((_, rest)) => rest,
        None => text,
    }
}

/// A `double` as Postgres prints one: its shortest round-trip digits, fixed
/// from 1e-4 up to 1e15 and in `e±NN` notation outside, and `NaN`,
/// `Infinity` and `-Infinity` by name. A literal every SQL dialect reads back
/// as the same value.
fn shown_f64(value: f64) -> String {
    named(value.is_nan(), value.is_infinite(), value < 0.0)
        .unwrap_or_else(|| laid_out(&format!("{value:e}"), 15))
}

/// A `real` the same way, fixed below 1e6, as Postgres prints a `float4`.
fn shown_f32(value: f32) -> String {
    named(value.is_nan(), value.is_infinite(), value < 0.0)
        .unwrap_or_else(|| laid_out(&format!("{value:e}"), 6))
}

fn named(nan: bool, infinite: bool, negative: bool) -> Option<String> {
    match (nan, infinite, negative) {
        (true, _, _) => Some("NaN".to_string()),
        (_, true, false) => Some("Infinity".to_string()),
        (_, true, true) => Some("-Infinity".to_string()),
        _ => None,
    }
}

/// `scientific` is Rust's `{:e}`, the shortest digits that round-trip.
fn laid_out(scientific: &str, fixed_below: i32) -> String {
    let (sign, unsigned) = match scientific.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", scientific),
    };
    let (mantissa, exponent) = unsigned.split_once('e').unwrap_or((unsigned, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let body = match exponent {
        ..-4 => scientific_notation(&digits, exponent),
        -4..0 => format!("0.{}{digits}", "0".repeat((-exponent - 1) as usize)),
        _ if exponent < fixed_below => {
            let whole = exponent as usize + 1;
            match digits.len() > whole {
                true => format!("{}.{}", &digits[..whole], &digits[whole..]),
                false => format!("{digits}{}", "0".repeat(whole - digits.len())),
            }
        }
        _ => scientific_notation(&digits, exponent),
    };
    format!("{sign}{body}")
}

fn scientific_notation(digits: &str, exponent: i32) -> String {
    let (first, rest) = digits.split_at(1);
    let point = match rest.is_empty() {
        true => String::new(),
        false => format!(".{rest}"),
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{first}{point}e{sign}{:02}", exponent.abs())
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

enum Builder {
    Text(StringBuilder),
    Bytes(BinaryBuilder),
}

fn batch(schema: &SchemaRef, rows: Vec<Vec<Option<Cell>>>) -> RecordBatch {
    let mut builders: Vec<Builder> = schema
        .fields()
        .iter()
        .map(|field| match field.data_type() {
            DataType::Binary => Builder::Bytes(BinaryBuilder::new()),
            _ => Builder::Text(StringBuilder::new()),
        })
        .collect();
    for row in rows {
        for (builder, value) in builders.iter_mut().zip(row) {
            match (builder, value) {
                (Builder::Text(builder), Some(Cell::Text(text))) => builder.append_value(text),
                (Builder::Bytes(builder), Some(Cell::Bytes(bytes))) => builder.append_value(bytes),
                (Builder::Text(builder), _) => builder.append_null(),
                (Builder::Bytes(builder), _) => builder.append_null(),
            }
        }
    }
    let columns: Vec<ArrayRef> = builders
        .into_iter()
        .map(|builder| match builder {
            Builder::Text(mut builder) => Arc::new(builder.finish()) as ArrayRef,
            Builder::Bytes(mut builder) => Arc::new(builder.finish()) as ArrayRef,
        })
        .collect();
    RecordBatch::try_new(Arc::clone(schema), columns).expect("one column per field, of its type")
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
    rows: Vec<Vec<Option<Cell>>>,
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

    fn push(&mut self, row: Vec<Option<Cell>>, now: Instant) -> Vec<Vec<Vec<Option<Cell>>>> {
        let size: usize = row.iter().flatten().map(Cell::len).sum();
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

    fn finish(&mut self) -> Option<Vec<Vec<Option<Cell>>>> {
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

    fn row(value: &str) -> Vec<Option<Cell>> {
        vec![Some(Cell::Text(value.to_string())), None]
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
    fn a_drivers_own_type_is_read_as_bytes_only_when_its_name_says_binary() {
        assert_eq!(fetched_as(-3, "VARBINARY"), Fetched::Binary);
        assert_eq!(fetched_as(-98, "BLOB"), Fetched::Binary);
        assert_eq!(fetched_as(-99, "CLOB"), Fetched::Text);
        assert_eq!(fetched_as(-370, "XML"), Fetched::Text);
        assert_eq!(fetched_as(8, "BINARY_DOUBLE"), Fetched::Text);
        assert_eq!(fetched_as(12, "VARCHAR"), Fetched::Text);
    }

    #[test]
    fn an_approximate_numeric_is_read_as_a_float_only_when_it_fits_one() {
        assert_eq!(read_as(8, 53, "DOUBLE"), Read::Double);
        assert_eq!(read_as(8, 15, "float8"), Read::Double);
        assert_eq!(read_as(6, 0, "FLOAT"), Read::Double);
        assert_eq!(read_as(7, 24, "REAL"), Read::Real);
        assert_eq!(read_as(6, 126, "FLOAT"), Read::Chunked(Fetched::Text));
        assert_eq!(read_as(3, 31, "DECIMAL"), Read::Chunked(Fetched::Text));
    }

    #[test]
    fn floats_and_odbc_text_are_bound_and_binary_and_a_drivers_own_types_are_not() {
        assert_eq!(bound_as(Read::Double, 8, 15), Some(Bound::Double));
        assert_eq!(bound_as(Read::Real, 7, 24), Some(Bound::Real));
        let text = Read::Chunked(Fetched::Text);
        assert_eq!(bound_as(text, 12, 10), Some(Bound::Text(64)));
        assert_eq!(bound_as(text, -9, 1000), Some(Bound::Text(2000)));
        assert_eq!(bound_as(text, -10, 0), Some(Bound::Text(1024)));
        assert_eq!(bound_as(text, -1, 1 << 30), Some(Bound::Text(8192)));
        assert_eq!(bound_as(text, -99, 100), None);
        assert_eq!(bound_as(Read::Chunked(Fetched::Binary), -3, 10), None);
        assert_eq!(bound_as(Read::ImplicitXml, -370, 0), None);
    }

    #[test]
    fn only_ibms_xml_loses_the_declaration_its_driver_adds() {
        assert_eq!(read_as(-370, 0, "XML"), Read::ImplicitXml);
        assert_eq!(read_as(-370, 0, "OTHER"), Read::Chunked(Fetched::Text));
        assert_eq!(read_as(-10, 0, "xml"), Read::Chunked(Fetched::Text));
        let declared = "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-16\" ?><a>\u{feff}</a>";
        assert_eq!(undeclared(declared), "<a>\u{feff}</a>");
        assert_eq!(
            undeclared("<?xml-stylesheet href=\"s\"?><a/>"),
            "<?xml-stylesheet href=\"s\"?><a/>"
        );
        assert_eq!(undeclared("<a/>"), "<a/>");
        assert_eq!(undeclared("\u{feff}<a/>"), "<a/>");
    }

    #[test]
    fn a_float_shows_its_shortest_round_trip_digits_as_postgres_prints_them() {
        let doubles = [
            (1.2345678901234567, "1.2345678901234567"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.79769e308, "1.79769e+308"),
            (-1.79769e308, "-1.79769e+308"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (100.0, "100"),
            (123456789012345.0, "123456789012345"),
            (1e15, "1e+15"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-0.0, "-0"),
            (0.0, "0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (value, shown) in doubles {
            assert_eq!(shown_f64(value), shown);
            if value.is_finite() {
                assert_eq!(shown.parse::<f64>().unwrap().to_bits(), value.to_bits());
            }
        }
        assert_eq!(shown_f32(1.17549e-37), "1.17549e-37");
        assert_eq!(shown_f32(3.4028e38), "3.4028e+38");
        assert_eq!(shown_f32(16777216.0), "1.6777216e+07");
        assert_eq!(shown_f32(1.5), "1.5");
    }

    #[test]
    fn text_is_read_as_utf_16() {
        let text: Vec<u8> = "Zoë 李 🐉"
            .encode_utf16()
            .flat_map(u16::to_ne_bytes)
            .collect();
        assert_eq!(wide_text(&text), "Zoë 李 🐉");
    }

    #[test]
    fn a_binary_column_travels_as_bytes_and_text_as_utf8() {
        let reads = [Read::Chunked(Fetched::Binary), Read::Chunked(Fetched::Text)];
        let fields: Vec<Field> = reads
            .iter()
            .map(|&read| column_field("c".to_string(), "t".to_string(), 12, read, None))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let rows = vec![
            vec![
                Some(Cell::Bytes(vec![0, 0xff])),
                Some(Cell::Text("a".to_string())),
            ],
            vec![None, None],
        ];
        let batch = batch(&schema, rows);
        assert_eq!(batch.column(0).data_type(), &DataType::Binary);
        assert_eq!(batch.column(1).data_type(), &DataType::Utf8);
        assert_eq!(batch.column(0).null_count(), 1);
    }

    #[test]
    fn only_text_and_exact_numbers_read_whole_are_exact() {
        let text = Read::Chunked(Fetched::Text);
        for exact_type in [1, 12, -8, -9, -6, 5, 4, -5, 3, 2, -11] {
            assert!(exact(exact_type, "t", text), "{exact_type}");
        }
        // Float, real and double; date, time, timestamp and an interval;
        // bit, long text, binary and a driver's own type.
        for other in [6, 7, 8, 91, 92, 93, 103, -7, -1, -10, -3, -99, -370] {
            assert!(!exact(other, "t", text), "{other}");
        }
        assert!(!exact(12, "t", Read::Chunked(Fetched::Binary)));
        assert!(!exact(4, "t", Read::Double));
        assert!(!exact(12, "bool", text));
        assert!(!exact(1, "BOOLEAN", text));
        let field = column_field("c".to_string(), "int4".to_string(), 4, text, None);
        assert_eq!(
            field.metadata().get(EXACT).map(String::as_str),
            Some("true")
        );
        let field = column_field("c".to_string(), "float8".to_string(), 8, Read::Double, None);
        assert_eq!(field.metadata().get(EXACT), None);
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
            ("write.rs", include_str!("write.rs")),
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

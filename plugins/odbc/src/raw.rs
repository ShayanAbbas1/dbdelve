//! Every ODBC call the bridge makes through `odbc_api::sys` rather than
//! through odbc-api's own wrappers, and the one place an SQLLEN out-value is
//! read.
//!
//! IBM's macOS Db2 driver writes only the low 32 bits of every SQLLEN it
//! returns (an indicator, a row count, a numeric attribute), leaving the upper
//! half as it found it. So every SQLLEN out-parameter here starts at zero and
//! is read back through [`sqllen`], for every driver alike. odbc-api's buffers,
//! indicators and `row_count` trust a 64-bit SQLLEN, which is why no value is
//! read through them. Columns are bound one row at a time ([`Bindings`]), never
//! as arrays: a bound array's indicator stride would carry the same bug where
//! no wrapper could fix it.

use odbc_api::handles::{self, AnyHandle, Statement, StatementImpl};
use odbc_api::sys::{
    CDataType, ConnectionAttribute, Desc, FreeStmtOption, HDbc, HStmt, InfoType, Len, NO_TOTAL,
    Nullability, Pointer, SQLBindCol, SQLCancel, SQLColAttributeW, SQLColumnsW, SQLDescribeColW,
    SQLExecDirectW, SQLFetch, SQLForeignKeysW, SQLFreeHandle, SQLFreeStmt, SQLGetData,
    SQLGetDiagRecW, SQLGetInfoW, SQLGetStmtAttrW, SQLMoreResults, SQLNumResultCols,
    SQLPrimaryKeysW, SQLRowCount, SQLSetConnectAttrW, SQLSetStmtAttrW, SQLTablesW, SqlDataType,
    SqlReturn, StatementAttribute, ULen,
};

pub const NULL_DATA: isize = -1;

/// One diagnostic record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub state: String,
    pub native: i32,
    pub message: String,
}

/// Every record the last call on `handle` left, first first, up to `most`.
/// Read here rather than through odbc-api's `Record`, which panics on a
/// SQLSTATE that is not UTF-8 or a negative length.
pub fn diagnostics(handle: &impl AnyHandle, most: i16) -> Vec<Record> {
    let mut records = Vec::new();
    for number in 1..=most {
        let mut state = [0u16; 6];
        let mut message = vec![0u16; 512];
        loop {
            let capacity = i16::try_from(message.len()).unwrap_or(i16::MAX);
            let mut native: i32 = 0;
            let mut length: i16 = 0;
            // SAFETY: a live handle the caller borrows; the driver manager
            // writes a five-character state and its NUL into `state`, and at
            // most `capacity` characters into `message`, which holds that many.
            let result = unsafe {
                SQLGetDiagRecW(
                    handle.handle_type(),
                    handle.as_handle(),
                    number,
                    state.as_mut_ptr(),
                    &mut native,
                    message.as_mut_ptr(),
                    capacity,
                    &mut length,
                )
            };
            if !succeeded(result) {
                return records;
            }
            let length = usize::try_from(length).unwrap_or(0);
            if length >= message.len() && message.len() < i16::MAX as usize {
                message = vec![0u16; (length + 1).min(i16::MAX as usize)];
                continue;
            }
            let state = String::from_utf16_lossy(&state[..5]);
            let message = String::from_utf16_lossy(&message[..length.min(message.len() - 1)]);
            records.push(Record {
                state: state.trim_end_matches('\0').to_string(),
                native,
                message,
            });
            break;
        }
    }
    records
}

/// Frees a handle whatever `SQLFreeHandle` answers. odbc-api's own drop panics
/// on anything but `SUCCESS`, and a driver may free with a warning.
///
/// # Safety
///
/// The handle is not used again, and its odbc-api wrapper is never dropped.
pub unsafe fn free(handle: &impl AnyHandle) {
    // SAFETY: the caller gives the handle up for good.
    let _ = unsafe { SQLFreeHandle(handle.handle_type(), handle.as_handle()) };
}

/// An SQLLEN as the driver meant it. A value whose upper 32 bits are zero is
/// the sign-extended `i32` of its low half; anything else is used as is, so a
/// correct 64-bit driver's −1 stays −1 and the 32-bit write of −1
/// (`0x0000_0000_FFFF_FFFF`) becomes −1 too.
///
/// ponytail: a genuine 64-bit length or count in [2^31, 2^32) is misread as
/// negative. Lengths cannot get there (MAX_FRAME caps what a value may
/// gather, and the read loop never sizes from the indicator's total); a row
/// count there is reported as unknown. Lift this when a driver needs it and
/// the macOS Db2 driver has gone.
pub fn sqllen(raw: isize) -> isize {
    match (raw as u64) >> 32 {
        0 => raw as i32 as isize,
        _ => raw,
    }
}

fn succeeded(result: SqlReturn) -> bool {
    matches!(result, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

/// A string `SQLGetInfo` answers. odbc-api reads `SQL_DBMS_NAME` alone, and
/// has no getter for `SQL_DBMS_VER` or `SQL_DRIVER_NAME`. `None` when the call
/// failed, its diagnostics left on the connection handle.
pub fn info_string(connection: &handles::Connection, info: InfoType) -> Option<String> {
    let handle = connection.as_sys();
    let mut buffer = vec![0u16; 256];
    let mut bytes = get_info(handle, info, &mut buffer)?;
    // The length is what the whole string needs, in bytes, without its NUL.
    if bytes / 2 >= buffer.len() {
        buffer = vec![0u16; bytes / 2 + 1];
        bytes = get_info(handle, info, &mut buffer)?;
    }
    let chars = (bytes / 2).min(buffer.len().saturating_sub(1));
    Some(String::from_utf16_lossy(&buffer[..chars]))
}

/// A `SQLUINTEGER` `SQLGetInfo` answers, such as a bitmask.
pub fn info_u32(connection: &handles::Connection, info: InfoType) -> Option<u32> {
    let mut value: u32 = 0;
    let mut bytes: i16 = 0;
    // SAFETY: a live connection handle the caller borrows; the driver writes
    // one `SQLUINTEGER` into `value`, the size it is given, and the length
    // out is an `SQLSMALLINT`.
    let result = unsafe {
        SQLGetInfoW(
            connection.as_sys(),
            info,
            &mut value as *mut u32 as Pointer,
            4,
            &mut bytes,
        )
    };
    succeeded(result).then_some(value)
}

/// The driver runs statements in transactions it is told to end:
/// `SQL_TXN_CAPABLE` is other than `SQL_TC_NONE`. `false` when it cannot say.
pub fn transactions(connection: &handles::Connection) -> bool {
    const SQL_TC_NONE: u16 = 0;
    let mut value: u16 = SQL_TC_NONE;
    let mut bytes: i16 = 0;
    // SAFETY: a live connection handle the caller borrows; the driver writes
    // one `SQLUSMALLINT` into `value`, the size it is given, and the length
    // out is an `SQLSMALLINT`.
    let result = unsafe {
        SQLGetInfoW(
            connection.as_sys(),
            InfoType::TransactionCapable,
            &mut value as *mut u16 as Pointer,
            2,
            &mut bytes,
        )
    };
    succeeded(result) && value != SQL_TC_NONE
}

fn get_info(handle: HDbc, info: InfoType, buffer: &mut [u16]) -> Option<usize> {
    let capacity = i16::try_from(buffer.len() * 2).unwrap_or(i16::MAX);
    let mut bytes: i16 = 0;
    // SAFETY: `handle` belongs to a live, connected `handles::Connection` the
    // caller borrows for the whole call; the driver writes at most `capacity`
    // bytes into `buffer`, which holds at least that many, and the length out
    // is an `SQLSMALLINT`, so no driver's SQLLEN width can overrun it.
    let result = unsafe {
        SQLGetInfoW(
            handle,
            info,
            buffer.as_mut_ptr() as Pointer,
            capacity,
            &mut bytes,
        )
    };
    match succeeded(result) {
        true => usize::try_from(bytes).ok(),
        false => None,
    }
}

/// Asks IBM's Db2 driver to warn (`CLI0212W`) on the first statement after
/// its automatic client reroute reconnected without an error, which it
/// otherwise does silently. `SQL_ATTR_REPORT_SEAMLESSFAILOVER_WARNING` in
/// IBM's `sqlcli1.h`, numbered below the range ODBC reserves for driver
/// attributes, so another driver may read 2605 as something else.
pub fn report_seamless_failover(connection: &handles::Connection) -> bool {
    const SQL_ATTR_REPORT_SEAMLESSFAILOVER_WARNING: i32 = 2605;
    const SQL_TRUE: usize = 1;
    // SAFETY: a live connection handle the caller borrows; the attribute
    // takes an integer passed by value in the pointer argument, so the driver
    // dereferences nothing.
    succeeded(unsafe {
        SQLSetConnectAttrW(
            connection.as_sys(),
            ConnectionAttribute(SQL_ATTR_REPORT_SEAMLESSFAILOVER_WARNING),
            SQL_TRUE as Pointer,
            0,
        )
    })
}

/// Turns ODBC escape scanning off, so `{fn …}`, `{d '…'}` and `{call …}` reach
/// the server as the user typed them rather than rewritten by the driver.
/// Read back rather than trusted: a driver may accept the call with `01S02`
/// and keep a value of its own.
pub fn set_noscan(statement: &StatementImpl) -> bool {
    const SQL_NOSCAN_ON: usize = 1;
    set_attribute(statement, StatementAttribute::NoScan, SQL_NOSCAN_ON)
        && attribute(statement, StatementAttribute::NoScan) == Some(SQL_NOSCAN_ON)
}

/// Empties the driver manager's diagnostic records for `statement`, which
/// unixODBC does on entry to any call; reading an attribute is one that every
/// state allows and that posts nothing of its own.
pub fn clear_diagnostics(statement: &StatementImpl) {
    let _ = attribute(statement, StatementAttribute::NoScan);
}

fn attribute(statement: &StatementImpl, attribute: StatementAttribute) -> Option<usize> {
    let mut value: usize = 0;
    let mut length: i32 = 0;
    // SAFETY: a live statement handle; both attributes read here are an
    // SQLULEN, written into a zeroed `usize` of that size, so a driver writing
    // only the low half still leaves the small value it meant.
    let result = unsafe {
        SQLGetStmtAttrW(
            statement.as_sys(),
            attribute,
            &mut value as *mut usize as Pointer,
            0,
            &mut length,
        )
    };
    succeeded(result).then_some(value)
}

pub fn set_query_timeout(statement: &StatementImpl, seconds: u32) -> bool {
    set_attribute(
        statement,
        StatementAttribute::QueryTimeout,
        seconds as usize,
    )
}

fn set_attribute(statement: &StatementImpl, attribute: StatementAttribute, value: usize) -> bool {
    // SAFETY: the handle is a live statement the caller borrows; both
    // attributes take an SQLULEN passed by value in the pointer argument, so
    // the driver dereferences nothing.
    succeeded(unsafe { SQLSetStmtAttrW(statement.as_sys(), attribute, value as Pointer, 0) })
}

/// Runs the statement's text as typed, through the wide entry point.
/// odbc-api's `execute` is not used: it calls `SQLNumResultCols` straight
/// after, which clears the warnings `SQLExecDirect` left, and those are the
/// statement's messages.
pub fn exec_direct(statement: &StatementImpl, sql: &str) -> SqlReturn {
    let text: Vec<u16> = sql.encode_utf16().collect();
    let Ok(length) = i32::try_from(text.len()) else {
        return SqlReturn::ERROR;
    };
    // SAFETY: the handle is a live statement the caller borrows, and `text`
    // holds exactly `length` UTF-16 units for the whole call.
    unsafe { SQLExecDirectW(statement.as_sys(), text.as_ptr(), length) }
}

pub fn num_result_cols(statement: &StatementImpl) -> Option<u16> {
    let mut columns: i16 = 0;
    // SAFETY: a live statement handle, and an `SQLSMALLINT` out the driver
    // cannot overrun.
    let result = unsafe { SQLNumResultCols(statement.as_sys(), &mut columns) };
    match succeeded(result) {
        true => u16::try_from(columns).ok(),
        false => None,
    }
}

pub fn fetch(statement: &StatementImpl) -> SqlReturn {
    // SAFETY: a live statement handle; any bound columns are held by a live
    // `Bindings`, whose buffers outlive it, and unbound when it drops.
    unsafe { SQLFetch(statement.as_sys()) }
}

pub fn more_results(statement: &StatementImpl) -> SqlReturn {
    // SAFETY: a live statement handle; a result's `Bindings` has dropped, and
    // unbound its columns, before the next result is asked for.
    unsafe { SQLMoreResults(statement.as_sys()) }
}

/// `SQLGetData` may read a column that is bound, which recovers a value too
/// long for its bound buffer, and any column in any order.
pub fn reads_bound_columns(connection: &handles::Connection) -> bool {
    const SQL_GD_ANY_COLUMN: u32 = 1;
    const SQL_GD_BOUND: u32 = 8;
    let wanted = SQL_GD_ANY_COLUMN | SQL_GD_BOUND;
    info_u32(connection, InfoType::GetDataExtensions).is_some_and(|bits| bits & wanted == wanted)
}

/// How one column is bound for `SQLFetch` to fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// Wide text of at most this many UTF-16 units; a longer value is
    /// truncated there and read again with `SQLGetData`.
    Text(usize),
    Double,
    Real,
}

pub enum BoundValue<'a> {
    Null,
    /// UTF-16 code units, as native-endian bytes.
    Text(&'a [u8]),
    /// Longer than its buffer, or of a length the driver did not give.
    Truncated,
    Double(f64),
    Real(f32),
}

struct Slot {
    bound: Bound,
    /// `u64`s so a bound `double` is aligned.
    buffer: Vec<u64>,
    indicator: Box<Len>,
}

/// Columns bound for the current row, unbound on drop, which must come before
/// the statement moves to another result or is freed.
pub struct Bindings<'s> {
    statement: &'s StatementImpl<'s>,
    slots: Vec<(u16, Slot)>,
}

impl<'s> Bindings<'s> {
    /// `None` when the driver refused a binding, every one then undone.
    pub fn new(statement: &'s StatementImpl<'s>, columns: &[(u16, Bound)]) -> Option<Self> {
        let mut bindings = Self {
            statement,
            slots: Vec::with_capacity(columns.len()),
        };
        for &(column, bound) in columns {
            let (target, bytes) = match bound {
                Bound::Text(units) => (CDataType::WChar, (units + 1) * 2),
                Bound::Double => (CDataType::Double, 8),
                Bound::Real => (CDataType::Float, 4),
            };
            let mut slot = Slot {
                bound,
                buffer: vec![0; bytes.div_ceil(8)],
                indicator: Box::new(0),
            };
            // SAFETY: a live statement handle; the buffer holds `bytes` bytes
            // and the indicator one SQLLEN, both on the heap, so moving the
            // slot into `bindings` leaves them where the driver was told, and
            // both live until `drop` unbinds them.
            let result = unsafe {
                SQLBindCol(
                    statement.as_sys(),
                    column,
                    target,
                    slot.buffer.as_mut_ptr() as Pointer,
                    bytes as Len,
                    &mut *slot.indicator,
                )
            };
            bindings.slots.push((column, slot));
            if !succeeded(result) {
                return None;
            }
        }
        Some(bindings)
    }

    /// Zeroes every indicator before a fetch, for a driver that writes only
    /// the low half of one.
    pub fn clear(&mut self) {
        for (_, slot) in &mut self.slots {
            *slot.indicator = 0;
        }
    }

    /// The slot bound to `column`, if any.
    pub fn slot(&self, column: u16) -> Option<usize> {
        self.slots.iter().position(|(bound, _)| *bound == column)
    }

    /// The fetched row's value in slot `index`.
    pub fn value(&self, index: usize) -> BoundValue<'_> {
        let slot = &self.slots[index].1;
        let indicator = sqllen(*slot.indicator);
        if indicator == NULL_DATA {
            return BoundValue::Null;
        }
        let bytes: &[u8] = as_bytes(&slot.buffer);
        match slot.bound {
            Bound::Double => BoundValue::Double(f64::from_ne_bytes(bytes[..8].try_into().unwrap())),
            Bound::Real => BoundValue::Real(f32::from_ne_bytes(bytes[..4].try_into().unwrap())),
            Bound::Text(units) => match usize::try_from(indicator) {
                Ok(length) if length <= units * 2 => BoundValue::Text(&bytes[..length & !1]),
                _ => BoundValue::Truncated,
            },
        }
    }
}

impl Drop for Bindings<'_> {
    fn drop(&mut self) {
        // SAFETY: a live statement handle; afterwards the driver holds no
        // pointer into the slots, which drop after this.
        let _ = unsafe { SQLFreeStmt(self.statement.as_sys(), FreeStmtOption::Unbind) };
    }
}

fn as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: any `u64` is eight initialised bytes, and `u8` has no alignment.
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 8) }
}

/// `None` when the driver cannot say, which a negative count means.
pub fn row_count(statement: &StatementImpl) -> Option<u64> {
    let mut count: Len = 0;
    // SAFETY: a live statement handle, and an SQLLEN out zeroed first so a
    // driver writing only its low half leaves a readable value.
    let result = unsafe { SQLRowCount(statement.as_sys(), &mut count) };
    match succeeded(result) {
        true => u64::try_from(sqllen(count)).ok(),
        false => None,
    }
}

/// A column as `SQLDescribeCol` describes it.
pub struct Described {
    pub name: String,
    pub data_type: i16,
    /// Digits, or bits for a type whose radix is 2; 0 when the driver gave
    /// none.
    pub size: usize,
}

/// The column's name, SQL data type and size. The type is an `SQLSMALLINT`,
/// which no SQLLEN width can garble; the size, an SQLULEN, is zeroed first, so
/// a driver writing only its low half still leaves a small size readable.
pub fn describe_col(statement: &StatementImpl, column: u16) -> Option<Described> {
    let mut name = vec![0u16; 128];
    loop {
        let capacity = i16::try_from(name.len()).unwrap_or(i16::MAX);
        let mut length: i16 = 0;
        let mut data_type = SqlDataType(0);
        let mut size: ULen = 0;
        let mut digits: i16 = 0;
        let mut nullable = Nullability(0);
        // SAFETY: a live statement handle; the driver writes at most
        // `capacity` characters into `name`, which holds that many, and every
        // other out is a local the call cannot overrun.
        let result = unsafe {
            SQLDescribeColW(
                statement.as_sys(),
                column,
                name.as_mut_ptr(),
                capacity,
                &mut length,
                &mut data_type,
                &mut size,
                &mut digits,
                &mut nullable,
            )
        };
        if !succeeded(result) {
            return None;
        }
        let length = usize::try_from(length).unwrap_or(0);
        if length >= name.len() && name.len() < i16::MAX as usize {
            name = vec![0u16; (length + 1).min(i16::MAX as usize)];
            continue;
        }
        let name = String::from_utf16_lossy(&name[..length.min(name.len())]);
        return Some(Described {
            name,
            data_type: data_type.0,
            size: usize::try_from(sqllen(size as isize)).unwrap_or(0),
        });
    }
}

/// The driver's own name for the column's type (`SQL_DESC_TYPE_NAME`).
pub fn type_name(statement: &StatementImpl, column: u16) -> Option<String> {
    let mut buffer = vec![0u16; 64];
    loop {
        let capacity = i16::try_from(buffer.len() * 2).unwrap_or(i16::MAX);
        let mut bytes: i16 = 0;
        let mut numeric: Len = 0;
        // SAFETY: a live statement handle; the driver writes at most
        // `capacity` bytes into `buffer`, which holds that many. The numeric
        // out is unused for a string field but given a zeroed local anyway, in
        // case a driver writes it regardless.
        let result = unsafe {
            SQLColAttributeW(
                statement.as_sys(),
                column,
                Desc::TypeName,
                buffer.as_mut_ptr() as Pointer,
                capacity,
                &mut bytes,
                &mut numeric,
            )
        };
        if !succeeded(result) {
            return None;
        }
        let chars = usize::try_from(bytes).unwrap_or(0) / 2;
        if chars >= buffer.len() && buffer.len() * 2 < i16::MAX as usize {
            buffer = vec![0u16; chars + 1];
            continue;
        }
        return Some(String::from_utf16_lossy(&buffer[..chars.min(buffer.len())]));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetched {
    /// UTF-16 code units, as native-endian bytes.
    Text,
    Binary,
}

pub enum DataError {
    /// The call failed; its diagnostics are on the statement.
    Odbc,
    /// The value is longer than `limit` bytes, read so far and no further.
    TooLarge,
    /// The user's Cancel arrived between two chunks.
    Cancelled,
    /// The driver gave no length for a chunk, and nothing else tells it.
    Unmeasured,
}

/// One whole value of the current row's `column`, `None` for NULL.
///
/// Read in chunks into `scratch`, and driven by the return code and the bytes
/// gathered, never by the indicator's total, which a 32-bit driver cannot
/// report past 2^31: `01004` means the chunk was full and more follows. Only a
/// chunk that fit trusts its indicator, and then for no more than the chunk.
/// `cancelled` is asked before each chunk after the first, since a long value
/// is read with the statement unpublished, out of `SQLCancel`'s reach.
pub fn get_data(
    statement: &StatementImpl,
    column: u16,
    fetched: Fetched,
    limit: usize,
    scratch: &mut [u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Vec<u8>>, DataError> {
    let (target, terminator) = match fetched {
        Fetched::Text => (CDataType::WChar, 2),
        Fetched::Binary => (CDataType::Binary, 0),
    };
    // Whole UTF-16 units, so a full chunk never splits one.
    let capacity = scratch.len() & !1;
    let mut value = Vec::new();
    let mut first = true;
    loop {
        if !first && cancelled() {
            return Err(DataError::Cancelled);
        }
        let mut indicator: Len = 0;
        // SAFETY: a live statement handle positioned on a row; the driver
        // writes at most `capacity` bytes into `scratch`, which holds that
        // many, and the indicator is a zeroed local read through `sqllen`.
        let result = unsafe {
            SQLGetData(
                statement.as_sys(),
                column,
                target,
                scratch.as_mut_ptr() as Pointer,
                capacity as Len,
                &mut indicator,
            )
        };
        let indicator = sqllen(indicator);
        let more = match result {
            SqlReturn::NO_DATA => return Ok(Some(value)),
            SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {
                if first && indicator == NULL_DATA {
                    return Ok(None);
                }
                result == SqlReturn::SUCCESS_WITH_INFO && truncated(statement)
            }
            _ => return Err(DataError::Odbc),
        };
        let Some(written) =
            chunk_length(fetched, more, indicator, &scratch[..capacity], terminator)
        else {
            return Err(DataError::Unmeasured);
        };
        value.extend_from_slice(&scratch[..written]);
        if value.len() > limit {
            return Err(DataError::TooLarge);
        }
        if !more {
            return Ok(Some(value));
        }
        first = false;
    }
}

/// One approximate numeric of the current row's `column`, read as the
/// driver's binary `double` or `float`, `None` for NULL. A driver's text form
/// may round it (Db2's shows 15 digits), where the binary value is exact.
pub fn get_double(statement: &StatementImpl, column: u16) -> Result<Option<f64>, DataError> {
    get_fixed(statement, column, CDataType::Double)
}

pub fn get_real(statement: &StatementImpl, column: u16) -> Result<Option<f32>, DataError> {
    get_fixed(statement, column, CDataType::Float)
}

fn get_fixed<T: Copy + Default>(
    statement: &StatementImpl,
    column: u16,
    target: CDataType,
) -> Result<Option<T>, DataError> {
    let mut value = T::default();
    let mut indicator: Len = 0;
    // SAFETY: a live statement handle positioned on a row; `target` is the C
    // type of `T`, a fixed-size type the driver writes whole and no more of,
    // and the indicator is a zeroed local read through `sqllen`.
    let result = unsafe {
        SQLGetData(
            statement.as_sys(),
            column,
            target,
            &mut value as *mut T as Pointer,
            size_of::<T>() as Len,
            &mut indicator,
        )
    };
    match result {
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO if sqllen(indicator) == NULL_DATA => {
            Ok(None)
        }
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => Ok(Some(value)),
        _ => Err(DataError::Odbc),
    }
}

/// The bytes of one chunk `SQLGetData` wrote into `chunk`. A truncated binary
/// chunk is full; a truncated wide one is full unless the driver stopped a
/// unit short rather than split a surrogate pair, which leaves its terminator
/// in the last unit's place (a text holding U+0000 exactly there loses it). A
/// chunk that fit trusts its indicator, unless that is `SQL_NO_TOTAL`, where
/// text ends at its terminator and binary has no end to find.
fn chunk_length(
    fetched: Fetched,
    more: bool,
    indicator: isize,
    chunk: &[u8],
    terminator: usize,
) -> Option<usize> {
    let full = chunk.len() - terminator;
    let unit_short = fetched == Fetched::Text && full >= 2 && chunk[full - 2..full] == [0, 0];
    match (more, usize::try_from(indicator)) {
        (true, _) if unit_short => Some(full - 2),
        (true, _) => Some(full),
        (false, Ok(length)) => Some(length.min(full)),
        (false, Err(_)) if indicator == NO_TOTAL && fetched == Fetched::Text => chunk
            .as_chunks::<2>()
            .0
            .iter()
            .position(|unit| *unit == [0, 0])
            .map(|units| units * 2),
        (false, Err(_)) => None,
    }
}

/// A catalog function's text argument. `None` is passed as the NULL pointer,
/// which ODBC reads as "any", where an empty string means "none":
/// odbc-api's catalog calls take `&SqlText` and cannot pass NULL, so
/// `SQLTables` there would list only tables without a catalog, and
/// `SQLForeignKeys` would look for a table named "".
struct Argument(Option<Vec<u16>>);

impl Argument {
    fn new(text: Option<&str>) -> Option<Self> {
        let wide = text.map(|text| text.encode_utf16().collect::<Vec<u16>>());
        match &wide {
            Some(wide) if i16::try_from(wide.len()).is_err() => None,
            _ => Some(Self(wide)),
        }
    }

    fn pointer(&self) -> *const u16 {
        self.0
            .as_ref()
            .map_or(std::ptr::null(), |wide| wide.as_ptr())
    }

    fn length(&self) -> i16 {
        self.0.as_ref().map_or(0, |wide| wide.len() as i16)
    }
}

fn arguments<const N: usize>(texts: [Option<&str>; N]) -> Option<[Argument; N]> {
    let arguments: Vec<Argument> = texts
        .into_iter()
        .map(Argument::new)
        .collect::<Option<_>>()?;
    arguments.try_into().ok()
}

pub fn tables(statement: &StatementImpl, schema: Option<&str>, table: &str) -> SqlReturn {
    let Some([catalog, schema, table, kinds]) = arguments([None, schema, Some(table), None]) else {
        return SqlReturn::ERROR;
    };
    // SAFETY: a live statement handle; each argument is NULL or holds exactly
    // its length in UTF-16 units for the whole call.
    unsafe {
        SQLTablesW(
            statement.as_sys(),
            catalog.pointer(),
            catalog.length(),
            schema.pointer(),
            schema.length(),
            table.pointer(),
            table.length(),
            kinds.pointer(),
            kinds.length(),
        )
    }
}

/// Every column of the tables `schema` and `table` match, as patterns.
pub fn columns(
    statement: &StatementImpl,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: &str,
) -> SqlReturn {
    let Some([catalog, schema, table, column]) = arguments([catalog, schema, Some(table), None])
    else {
        return SqlReturn::ERROR;
    };
    // SAFETY: as in `tables`.
    unsafe {
        SQLColumnsW(
            statement.as_sys(),
            catalog.pointer(),
            catalog.length(),
            schema.pointer(),
            schema.length(),
            table.pointer(),
            table.length(),
            column.pointer(),
            column.length(),
        )
    }
}

pub fn primary_keys(
    statement: &StatementImpl,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: &str,
) -> SqlReturn {
    let Some([catalog, schema, table]) = arguments([catalog, schema, Some(table)]) else {
        return SqlReturn::ERROR;
    };
    // SAFETY: as in `tables`.
    unsafe {
        SQLPrimaryKeysW(
            statement.as_sys(),
            catalog.pointer(),
            catalog.length(),
            schema.pointer(),
            schema.length(),
            table.pointer(),
            table.length(),
        )
    }
}

/// Each side is (catalog, schema, table); the side left all `None` is the
/// one asked for.
pub fn foreign_keys(
    statement: &StatementImpl,
    primary: [Option<&str>; 3],
    foreign: [Option<&str>; 3],
) -> SqlReturn {
    let Some([pk_catalog, pk_schema, pk_table]) = arguments(primary) else {
        return SqlReturn::ERROR;
    };
    let Some([fk_catalog, fk_schema, fk_table]) = arguments(foreign) else {
        return SqlReturn::ERROR;
    };
    // SAFETY: as in `tables`.
    unsafe {
        SQLForeignKeysW(
            statement.as_sys(),
            pk_catalog.pointer(),
            pk_catalog.length(),
            pk_schema.pointer(),
            pk_schema.length(),
            pk_table.pointer(),
            pk_table.length(),
            fk_catalog.pointer(),
            fk_catalog.length(),
            fk_schema.pointer(),
            fk_schema.length(),
            fk_table.pointer(),
            fk_table.length(),
        )
    }
}

// odbc-sys declares no `SQLStatistics`. The symbol is the driver manager's,
// which odbc-sys already links on every platform.
unsafe extern "system" {
    fn SQLStatisticsW(
        statement: HStmt,
        catalog: *const u16,
        catalog_length: i16,
        schema: *const u16,
        schema_length: i16,
        table: *const u16,
        table_length: i16,
        unique: u16,
        reserved: u16,
    ) -> SqlReturn;
}

/// Every index of one table, and its `SQL_TABLE_STAT` row, which callers skip.
pub fn statistics(
    statement: &StatementImpl,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: &str,
) -> SqlReturn {
    const SQL_INDEX_ALL: u16 = 1;
    const SQL_QUICK: u16 = 0;
    let Some([catalog, schema, table]) = arguments([catalog, schema, Some(table)]) else {
        return SqlReturn::ERROR;
    };
    // SAFETY: as in `tables`; the signature is ODBC's `SQLStatisticsW`, whose
    // last two arguments are `SQLUSMALLINT`s.
    unsafe {
        SQLStatisticsW(
            statement.as_sys(),
            catalog.pointer(),
            catalog.length(),
            schema.pointer(),
            schema.length(),
            table.pointer(),
            table.length(),
            SQL_INDEX_ALL,
            SQL_QUICK,
        )
    }
}

fn truncated(statement: &StatementImpl) -> bool {
    diagnostics(statement, 16)
        .iter()
        .any(|record| record.state == "01004")
}

/// Cancels whatever runs on `statement`, from a thread other than the one
/// running it.
///
/// # Safety
///
/// `statement` must be a live statement handle, and must stay allocated until
/// this returns.
pub unsafe fn cancel(statement: HStmt) {
    // SAFETY: the caller keeps the handle allocated for the call; ODBC allows
    // SQLCancel on a statement another thread is using.
    let _ = unsafe { SQLCancel(statement) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_32_bit_write_reads_as_its_sign_extended_low_half() {
        assert_eq!(sqllen(0x0000_0000_0000_0001), 1);
        assert_eq!(sqllen(0x0000_0000_0000_0005), 5);
        assert_eq!(sqllen(0x0000_0000_FFFF_FFFF), -1);
        assert_eq!(sqllen(0x0000_0000_FFFF_FFFC), -4);
    }

    #[test]
    fn a_correct_64_bit_value_is_unchanged() {
        assert_eq!(sqllen(-1), -1);
        assert_eq!(sqllen(-4), -4);
        assert_eq!(sqllen(0x0000_0001_0000_0005), 0x0000_0001_0000_0005);
    }

    /// The ceiling the doc comment names: changing it should be deliberate.
    #[test]
    fn a_length_in_the_upper_half_of_32_bits_reads_negative() {
        assert_eq!(sqllen(0x0000_0000_8000_0000), i32::MIN as isize);
    }

    #[test]
    fn a_chunk_is_measured_without_stale_bytes() {
        let (text, binary) = (Fetched::Text, Fetched::Binary);
        // Eight bytes: three wide units and the terminator's place.
        let full = [b'a', 0, b'b', 0, b'c', 0, 0, 0];
        let unit_short = [b'a', 0, b'b', 0, 0, 0, b'x', 0];
        let ended = [b'a', 0, 0, 0, b'x', 0, b'x', 0];
        assert_eq!(chunk_length(text, true, NO_TOTAL, &full, 2), Some(6));
        assert_eq!(chunk_length(text, true, NO_TOTAL, &unit_short, 2), Some(4));
        assert_eq!(chunk_length(text, false, 2, &ended, 2), Some(2));
        assert_eq!(chunk_length(text, false, NO_TOTAL, &ended, 2), Some(2));
        assert_eq!(chunk_length(binary, true, NO_TOTAL, &ended, 0), Some(8));
        assert_eq!(chunk_length(binary, false, NO_TOTAL, &ended, 0), None);
    }
}

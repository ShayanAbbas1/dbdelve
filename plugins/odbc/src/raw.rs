//! Every ODBC call the bridge makes through `odbc_api::sys` rather than
//! through odbc-api's own wrappers, and the one place an SQLLEN out-value is
//! read.
//!
//! IBM's macOS Db2 driver writes only the low 32 bits of every SQLLEN it
//! returns (an indicator, a row count, a numeric attribute), leaving the upper
//! half as it found it. So every SQLLEN out-parameter here starts at zero and
//! is read back through [`sqllen`], for every driver alike. odbc-api's buffers,
//! indicators and `row_count` trust a 64-bit SQLLEN, which is why no value is
//! read through them, and why nothing binds columns: a bound array's indicator
//! stride would carry the same bug where no wrapper could fix it.

use odbc_api::handles::{self, Statement, StatementImpl};
use odbc_api::sys::{
    CDataType, Desc, HDbc, HStmt, InfoType, Len, Nullability, Pointer, SQLCancel, SQLColAttributeW,
    SQLColumnsW, SQLDescribeColW, SQLExecDirectW, SQLFetch, SQLForeignKeysW, SQLGetData,
    SQLGetInfoW, SQLMoreResults, SQLNumResultCols, SQLPrimaryKeysW, SQLRowCount, SQLSetStmtAttrW,
    SQLTablesW, SqlDataType, SqlReturn, StatementAttribute, ULen,
};

use crate::failure;

pub const NULL_DATA: isize = -1;

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

/// Turns ODBC escape scanning off, so `{fn …}`, `{d '…'}` and `{call …}` reach
/// the server as the user typed them rather than rewritten by the driver.
pub fn set_noscan(statement: &StatementImpl) -> bool {
    const SQL_NOSCAN_ON: usize = 1;
    set_attribute(statement, StatementAttribute::NoScan, SQL_NOSCAN_ON)
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
    // SAFETY: a live statement handle with no bound columns, so the driver
    // writes nowhere of ours.
    unsafe { SQLFetch(statement.as_sys()) }
}

pub fn more_results(statement: &StatementImpl) -> SqlReturn {
    // SAFETY: a live statement handle with no bound columns.
    unsafe { SQLMoreResults(statement.as_sys()) }
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

/// The column's name and its SQL data type. The type is an `SQLSMALLINT`,
/// which no SQLLEN width can garble; the column size, an SQLULEN, is zeroed
/// and never read.
pub fn describe_col(statement: &StatementImpl, column: u16) -> Option<(String, i16)> {
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
        return Some((name, data_type.0));
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

/// One whole value of the current row's `column`, `None` for NULL.
///
/// Read in chunks into `scratch`, and driven by the return code and the bytes
/// gathered, never by the indicator's total, which a 32-bit driver cannot
/// report past 2^31: `01004` means the chunk was full and more follows. Only a
/// chunk that fit trusts its indicator, and then for no more than the chunk.
pub fn get_data(
    statement: &StatementImpl,
    column: u16,
    fetched: Fetched,
    limit: usize,
    scratch: &mut [u8],
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
        let full = capacity - terminator;
        let written = match more {
            true => full,
            false => usize::try_from(indicator).map_or(full, |length| length.min(full)),
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

pub fn tables(statement: &StatementImpl, schema: &str, table: &str) -> SqlReturn {
    let Some([catalog, schema, table, kinds]) = arguments([None, Some(schema), Some(table), None])
    else {
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
    failure::records(statement)
        .iter()
        .any(|(state, _)| state == "01004")
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
}

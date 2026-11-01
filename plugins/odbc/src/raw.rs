//! Every ODBC call the bridge makes through `odbc_api::sys` rather than
//! through odbc-api's own wrappers.

use odbc_api::handles;
use odbc_api::sys::{HDbc, InfoType, Pointer, SQLGetInfoW, SqlReturn};

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
    match result {
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => Some(usize::try_from(bytes).ok()?),
        _ => None,
    }
}

//! ODBC diagnostics as the protocol's `error`.

use dbdelve_plugin_protocol::{ErrorKind, Response};
use odbc_api::handles::{Diagnostics, Record, slice_to_cow_utf8};

/// IM002 is the driver manager finding no DSN by that name and no default
/// driver. A `DRIVER=` name it has no entry for is not IM002: unixODBC then
/// loads the name as a library path and fails with 01000, "Can't open lib".
const NO_DRIVER_HINT: &str = "Name the driver by its library path (DRIVER=/path/to/library) or register it with the driver manager.";

/// So a driver that stacks hundreds of warnings on one call cannot make an
/// error message the size of a result.
const MOST_RECORDS: i16 = 16;

/// Every diagnostic record the last call on `handle` left, as one `error`.
pub fn from(handle: &impl Diagnostics, function: &str) -> Response {
    error(&records(handle), function)
}

/// The (SQLSTATE, message) pairs the last call on `handle` left, first first.
pub fn records(handle: &impl Diagnostics) -> Vec<(String, String)> {
    let mut record = Record::with_capacity(512);
    let mut records = Vec::new();
    let mut number = 1;
    while number <= MOST_RECORDS && record.fill_from(handle, number) {
        let message = slice_to_cow_utf8(&record.message).into_owned();
        records.push((record.state.as_str().to_string(), message));
        number += 1;
    }
    records
}

pub fn plain(message: &str) -> Response {
    Response::Error {
        kind: ErrorKind::Server,
        message: message.to_string(),
        sqlstate: None,
        position: None,
        lost: false,
    }
}

fn error(records: &[(String, String)], function: &str) -> Response {
    let state = |wanted: &str| records.iter().any(|(state, _)| state == wanted);
    let mut message = records
        .iter()
        .map(|(_, message)| message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if message.is_empty() {
        message = format!("{function} failed, and the driver said nothing about why.");
    }
    if state("IM002") {
        message.push('\n');
        message.push_str(NO_DRIVER_HINT);
    }
    // Never `Cancelled` from the SQLSTATE alone: Db2 reports its own query
    // timeout as HY008, which is no Cancel of the user's. The query path marks
    // a failure cancelled when the user's `cancel` arrived.
    Response::Error {
        kind: ErrorKind::Server,
        message,
        sqlstate: records.first().map(|(state, _)| state.clone()),
        position: None,
        lost: records.iter().any(|(state, _)| state.starts_with("08")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(state, message)| (state.to_string(), message.to_string()))
            .collect()
    }

    #[test]
    fn every_record_is_joined_and_the_first_state_kept() {
        let Response::Error {
            kind,
            message,
            sqlstate,
            lost,
            ..
        } = error(
            &records(&[("42000", "syntax"), ("01000", "note")]),
            "SQLExecDirect",
        )
        else {
            panic!("an error");
        };
        assert_eq!(kind, ErrorKind::Server);
        assert_eq!(message, "syntax\nnote");
        assert_eq!(sqlstate.as_deref(), Some("42000"));
        assert!(!lost);
    }

    #[test]
    fn a_connection_exception_is_lost_and_a_timeout_is_no_cancel() {
        let Response::Error { lost, .. } = error(&records(&[("08S01", "link")]), "f") else {
            panic!("an error");
        };
        assert!(lost);
        let Response::Error { kind, .. } = error(&records(&[("HY008", "timed out")]), "f") else {
            panic!("an error");
        };
        assert_eq!(kind, ErrorKind::Server);
    }

    #[test]
    fn no_driver_gets_the_hint() {
        let Response::Error { message, .. } = error(&records(&[("IM002", "not found")]), "f")
        else {
            panic!("an error");
        };
        assert_eq!(message, format!("not found\n{NO_DRIVER_HINT}"));
    }

    #[test]
    fn a_failure_without_records_still_says_what_failed() {
        let Response::Error {
            message, sqlstate, ..
        } = error(&[], "SQLConnect")
        else {
            panic!("an error");
        };
        assert!(message.starts_with("SQLConnect failed"));
        assert_eq!(sqlstate, None);
    }
}

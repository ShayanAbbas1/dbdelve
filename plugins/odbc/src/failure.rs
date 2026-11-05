//! ODBC diagnostics as the protocol's `error`.

use dbdelve_plugin_protocol::{ErrorKind, Response};
use odbc_api::handles::AnyHandle;

use crate::raw::{self, Record};

/// IM002 is the driver manager finding no DSN by that name and no default
/// driver. A `DRIVER=` name it has no entry for is not IM002: unixODBC then
/// loads the name as a library path and fails with 01000, "Can't open lib".
const NO_DRIVER_HINT: &str = "Name the driver by its library path (DRIVER=/path/to/library) or register it with the driver manager.";

/// So a driver that stacks hundreds of warnings on one call cannot make an
/// error message the size of a result.
const MOST_RECORDS: i16 = 16;

/// Every diagnostic record the last call on `handle` left, as one `error`.
pub fn from(handle: &impl AnyHandle, function: &str) -> Response {
    error(&records(handle), function)
}

/// The records the last call on `handle` left, first first.
pub fn records(handle: &impl AnyHandle) -> Vec<Record> {
    raw::diagnostics(handle, MOST_RECORDS)
}

/// Said after IBM's `CLI0212W`, whose own words name a failover and not what
/// it cost the session.
pub const RECONNECTED: &str = "The connection was lost and the driver silently re-established it before running this statement, so temporary tables, global variables and session settings other than SET CURRENT special registers were reset.";

/// IBM's Db2 driver's report that its automatic client reroute reconnected
/// without an error (`raw::report_seamless_failover`). Its SQLSTATE, 01000, is
/// any warning, so the code and the native error decide.
fn reconnected_silently(record: &Record) -> bool {
    record.native == -99999 && record.message.contains("CLI0212W")
}

/// The records' messages, with `RECONNECTED` after a silent reconnect.
pub fn messages(records: &[Record]) -> Vec<&str> {
    let mut messages = Vec::new();
    for record in records {
        messages.push(record.message.as_str());
        if reconnected_silently(record) {
            messages.push(RECONNECTED);
        }
    }
    messages
}

/// `response` with every spelling of `secret` the connection string could
/// carry blanked, for a driver that quotes the string in its error. A short
/// secret may blank ordinary words too, which beats showing it.
pub fn scrubbed(response: Response, secret: &str, spellings: &[String]) -> Response {
    match response {
        Response::Error {
            kind,
            mut message,
            sqlstate,
            position,
            lost,
        } if !secret.is_empty() => {
            for spelling in spellings.iter().map(String::as_str).chain([secret]) {
                message = message.replace(spelling, "********");
            }
            Response::Error {
                kind,
                message,
                sqlstate,
                position,
                lost,
            }
        }
        other => other,
    }
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

fn error(records: &[Record], function: &str) -> Response {
    let state = |wanted: &str| records.iter().any(|record| record.state == wanted);
    let mut message = messages(records).join("\n");
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
        sqlstate: records.first().map(|record| record.state.clone()),
        position: None,
        lost: records.iter().any(|record| is_lost(&record.state)),
    }
}

/// Db2 CLI reports a dead link as 40003 and psqlODBC reports the server
/// terminating the session as 57P01/57P02, both with no 08 record, so the
/// session would stay green until the next statement. 40001/40P01 are
/// retryable transaction failures on a live connection, and 57P03 is a
/// refusal at connect time, so neither counts.
fn is_lost(state: &str) -> bool {
    state.starts_with("08") || matches!(state, "40003" | "57P01" | "57P02")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(pairs: &[(&str, &str)]) -> Vec<Record> {
        pairs
            .iter()
            .map(|(state, message)| Record {
                state: state.to_string(),
                native: 0,
                message: message.to_string(),
            })
            .collect()
    }

    /// IBM's driver words it this way (12.1, live), alongside the statement's
    /// own result.
    #[test]
    fn a_silent_reconnect_is_said_and_any_other_warning_is_not() {
        let warning = |state: &str, native, message: &str| Record {
            state: state.to_string(),
            native,
            message: message.to_string(),
        };
        let failover = "[IBM][CLI Driver] CLI0212W  A seamless failover occurred during a connect or an execute request. SQLSTATE=01000";
        assert_eq!(
            messages(&[warning("01000", -99999, failover)]),
            [failover, RECONNECTED]
        );
        for other in [
            warning("01000", 0, failover),
            warning(
                "01000",
                -99999,
                "[IBM][CLI Driver] CLI0109E  String data right truncation.",
            ),
        ] {
            assert_eq!(
                messages(std::slice::from_ref(&other)),
                [other.message.as_str()]
            );
        }
    }

    #[test]
    fn a_quoted_password_is_blanked_in_every_spelling() {
        let quoted = error(
            &records(&[("08001", "bad string UID=u;PWD={p}}w} near p}w")]),
            "f",
        );
        let Response::Error { message, .. } = scrubbed(quoted, "p}w", &["{p}}w}".to_string()])
        else {
            panic!("an error");
        };
        assert_eq!(message, "bad string UID=u;PWD=******** near ********");
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

    #[test]
    fn a_dead_link_or_terminated_session_is_lost() {
        for state in ["08001", "08S01", "40003", "57P01", "57P02"] {
            let Response::Error { lost, .. } = error(&records(&[(state, "gone")]), "f") else {
                panic!("an error");
            };
            assert!(lost, "{state}");
        }
    }

    #[test]
    fn a_lost_record_after_the_first_still_marks_the_session() {
        let Response::Error { lost, .. } =
            error(&records(&[("HY000", "x"), ("40003", "link")]), "f")
        else {
            panic!("an error");
        };
        assert!(lost);
    }

    #[test]
    fn retryable_and_ordinary_failures_are_not_lost() {
        for state in [
            "40001", "40P01", "42000", "42P01", "57014", "57P03", "HY000", "HY008",
        ] {
            let Response::Error { lost, .. } = error(&records(&[(state, "no")]), "f") else {
                panic!("an error");
            };
            assert!(!lost, "{state}");
        }
    }
}

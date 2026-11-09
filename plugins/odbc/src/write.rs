//! A write: statements DBDelve wrote, each meant to change one row, run as one
//! transaction through the driver's own transaction control and kept only
//! when every one changed exactly one row.

use dbdelve_plugin_protocol::{ErrorKind, Response};
use odbc_api::handles::StatementImpl;
use odbc_api::sys::SqlReturn;

use crate::connect::Session;
use crate::run::{self, Control};
use crate::{failure, raw};

/// Runs `statements` with autocommit off, commits only when each changed
/// exactly one row, rolls back otherwise, and turns autocommit back on.
///
/// Never `BEGIN` text: a driver that commits each statement itself would run
/// it as a no-op and commit the first change alone. A session that cannot be
/// rolled back or put back in autocommit would run the user's next statement
/// inside a transaction nobody ends, so that error is `lost`.
pub fn write(session: &Session, control: &Control, statements: &[String]) -> Response {
    if statements.is_empty() {
        return Response::Error {
            kind: ErrorKind::Protocol,
            message: "A write names no statements.".to_string(),
            sqlstate: None,
            position: None,
            lost: false,
        };
    }
    let statement = match session.statement() {
        Ok(statement) => statement,
        Err(error) => return error,
    };
    if !session.set_autocommit(false) {
        return failure::from(session.handle(), "SQLSetConnectAttr");
    }
    let mut messages = Vec::new();
    let changed = each(&statement, control, statements, &mut messages);
    drop(statement);
    let outcome = match changed {
        // A cancel that lands between the last statement and the commit
        // still keeps nothing.
        Ok(_) if control.cancelled() => Err(Undone {
            error: headed(run::cancelled(), ""),
            at_commit: false,
        }),
        Ok(changed) if session.end(true) => Ok(changed),
        Ok(_) => Err(Undone {
            error: headed(
                failure::from(session.handle(), "SQLEndTran"),
                "The commit failed",
            ),
            at_commit: true,
        }),
        Err(error) => Err(Undone {
            error,
            at_commit: false,
        }),
    };
    let rolled_back = outcome.is_ok() || session.end(false);
    // Turning autocommit on commits whatever is open, so a transaction that
    // would not roll back is left for the disconnect to roll back instead.
    let autocommit = rolled_back && session.set_autocommit(true);
    match outcome {
        Ok(changed) => saved(changed, messages, autocommit),
        Err(undone) => undone.said(rolled_back, autocommit),
    }
}

/// Runs each statement on one handle, in order, and answers how many rows
/// they changed, or the first one's error or refusal.
fn each(
    statement: &StatementImpl,
    control: &Control,
    statements: &[String],
    messages: &mut Vec<String>,
) -> Result<u64, Response> {
    let mut changed = 0;
    let mut outcome = Ok(());
    for (index, sql) in statements.iter().enumerate() {
        outcome = change(statement, control, sql, messages).map_err(|failed| {
            let which = which(index, statements.len());
            match failed {
                Failed::Error(error) => headed(error, &format!("{which} failed")),
                Failed::Refused(why) => refusal(&format!("{which} {}", why.said())),
            }
        });
        if outcome.is_err() {
            break;
        }
        changed += 1;
    }
    control.settle(statement);
    outcome.map(|()| changed)
}

enum Failed {
    /// The driver's or the server's error, or a cancel.
    Error(Response),
    Refused(Refusal),
}

/// Why a statement that ran was not one change of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    Changed(u64),
    Uncounted,
    ReturnedRows,
    MoreThanOne,
}

impl Refusal {
    fn said(self) -> String {
        match self {
            Refusal::Changed(rows) => format!("changed {rows} rows, not one"),
            Refusal::Uncounted => "did not say how many rows it changed".to_string(),
            Refusal::ReturnedRows => "returned rows instead of changing one".to_string(),
            Refusal::MoreThanOne => "held more than one statement".to_string(),
        }
    }
}

/// Runs one statement and requires it to have changed exactly one row and
/// left nothing behind it.
fn change(
    statement: &StatementImpl,
    control: &Control,
    sql: &str,
    messages: &mut Vec<String>,
) -> Result<(), Failed> {
    let failed = |function| Failed::Error(control.failure(statement, function));
    let Some(executed) = control.published(statement, || raw::exec_direct(statement, sql)) else {
        return Err(Failed::Error(run::cancelled()));
    };
    let rows = match executed {
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {
            if executed == SqlReturn::SUCCESS_WITH_INFO {
                run::keep(messages, &failure::records(statement));
            }
            match raw::num_result_cols(statement) {
                None => return Err(failed("SQLNumResultCols")),
                Some(0) => raw::row_count(statement),
                Some(_) => return Err(Failed::Refused(Refusal::ReturnedRows)),
            }
        }
        // ODBC's answer for a searched UPDATE or DELETE that matched nothing.
        SqlReturn::NO_DATA => Some(0),
        _ => return Err(failed("SQLExecDirect")),
    };
    match rows {
        Some(1) => {}
        Some(rows) => return Err(Failed::Refused(Refusal::Changed(rows))),
        None => return Err(Failed::Refused(Refusal::Uncounted)),
    }
    let Some(next) = control.published(statement, || raw::more_results(statement)) else {
        return Err(Failed::Error(run::cancelled()));
    };
    match next {
        SqlReturn::NO_DATA if control.cancelled() => Err(Failed::Error(run::cancelled())),
        SqlReturn::NO_DATA => Ok(()),
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {
            Err(Failed::Refused(Refusal::MoreThanOne))
        }
        _ => Err(failed("SQLMoreResults")),
    }
}

/// A write that did not commit, said before the rollback's outcome is known.
struct Undone {
    error: Response,
    /// The commit itself failed, so what the server kept is less certain.
    at_commit: bool,
}

impl Undone {
    /// The error as the user reads it, with what became of the changes and
    /// of the session.
    fn said(self, rolled_back: bool, autocommit: bool) -> Response {
        let Response::Error {
            kind,
            mut message,
            sqlstate,
            position,
            lost,
        } = self.error
        else {
            return self.error;
        };
        message.push_str(match (rolled_back, self.at_commit) {
            (true, _) => ", so DBDelve rolled back every change and nothing was saved.",
            (false, false) => {
                ", and rolling back failed too, so the connection can no longer be trusted. Nothing was committed."
            }
            (false, true) => {
                ", and rolling back failed too, so whether the changes were saved is unknown and the connection can no longer be trusted."
            }
        });
        if rolled_back && !autocommit {
            message.push('\n');
            message.push_str(NOT_AUTOCOMMIT);
        }
        Response::Error {
            kind,
            message,
            sqlstate,
            position,
            lost: lost || !rolled_back || !autocommit,
        }
    }
}

const NOT_AUTOCOMMIT: &str =
    "The connection could not be put back in autocommit, so it can no longer be trusted.";

fn saved(changed: u64, messages: Vec<String>, autocommit: bool) -> Response {
    match autocommit {
        true => Response::End {
            rows: 0,
            rows_affected: Some(changed),
            stopped: false,
            messages,
            more_sets: 0,
        },
        false => Response::Error {
            kind: ErrorKind::Server,
            message: format!("Every change was saved.\n{NOT_AUTOCOMMIT}"),
            sqlstate: None,
            position: None,
            lost: true,
        },
    }
}

/// `error` with a line after the driver's own saying which step failed, which
/// `Undone::said` finishes. A cancel is said as one, whatever the driver said.
fn headed(error: Response, head: &str) -> Response {
    match error {
        Response::Error {
            kind: ErrorKind::Cancelled,
            sqlstate,
            position,
            lost,
            ..
        } => Response::Error {
            kind: ErrorKind::Cancelled,
            message: "The write was cancelled".to_string(),
            sqlstate,
            position,
            lost,
        },
        Response::Error {
            kind,
            message,
            sqlstate,
            position,
            lost,
        } => Response::Error {
            kind,
            message: format!("{message}\n{head}"),
            sqlstate,
            position,
            lost,
        },
        other => other,
    }
}

fn refusal(message: &str) -> Response {
    Response::Error {
        kind: ErrorKind::Server,
        message: message.to_string(),
        sqlstate: None,
        position: None,
        lost: false,
    }
}

/// "The statement", or "The 2nd of 3 statements".
fn which(index: usize, total: usize) -> String {
    match total {
        1 => "The statement".to_string(),
        _ => format!("The {} of {total} statements", ordinal(index + 1)),
    }
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(response: &Response) -> &str {
        match response {
            Response::Error { message, .. } => message,
            other => panic!("an error, got {other:?}"),
        }
    }

    fn lost(response: &Response) -> bool {
        matches!(response, Response::Error { lost: true, .. })
    }

    fn server(message: &str, sqlstate: &str) -> Response {
        Response::Error {
            kind: ErrorKind::Server,
            message: message.to_string(),
            sqlstate: Some(sqlstate.to_string()),
            position: None,
            lost: false,
        }
    }

    #[test]
    fn ordinals_read_as_english() {
        let said: Vec<_> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 101, 111]
            .into_iter()
            .map(ordinal)
            .collect();
        assert_eq!(
            said,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd",
                "101st", "111th"
            ]
        );
        assert_eq!(which(0, 1), "The statement");
        assert_eq!(which(1, 3), "The 2nd of 3 statements");
    }

    #[test]
    fn a_refused_count_says_which_statement_and_that_nothing_was_saved() {
        let error = refusal(&format!("{} {}", which(1, 3), Refusal::Changed(0).said()));
        let said = Undone {
            error,
            at_commit: false,
        }
        .said(true, true);
        assert_eq!(
            message(&said),
            "The 2nd of 3 statements changed 0 rows, not one, so DBDelve rolled back every change and nothing was saved."
        );
        assert!(!lost(&said));
    }

    #[test]
    fn a_server_error_keeps_its_text_and_sqlstate() {
        let error = headed(server("division by zero", "22012"), "The statement failed");
        let said = Undone {
            error,
            at_commit: false,
        }
        .said(true, true);
        let Response::Error {
            kind,
            message,
            sqlstate,
            lost,
            ..
        } = said
        else {
            panic!("an error");
        };
        assert_eq!(kind, ErrorKind::Server);
        assert_eq!(
            message,
            "division by zero\nThe statement failed, so DBDelve rolled back every change and nothing was saved."
        );
        assert_eq!(sqlstate.as_deref(), Some("22012"));
        assert!(!lost);
    }

    #[test]
    fn a_cancel_is_said_as_one() {
        let said = Undone {
            error: headed(run::cancelled(), "The 1st of 2 statements failed"),
            at_commit: false,
        }
        .said(true, true);
        assert!(matches!(
            said,
            Response::Error {
                kind: ErrorKind::Cancelled,
                lost: false,
                ..
            }
        ));
        assert_eq!(
            message(&said),
            "The write was cancelled, so DBDelve rolled back every change and nothing was saved."
        );
    }

    #[test]
    fn a_failed_rollback_or_autocommit_loses_the_session() {
        let undone = || Undone {
            error: headed(server("gone", "HY000"), "The commit failed"),
            at_commit: true,
        };
        let said = undone().said(false, false);
        assert!(lost(&said));
        assert!(!message(&said).contains(NOT_AUTOCOMMIT));
        assert!(message(&said).ends_with(
            "whether the changes were saved is unknown and the connection can no longer be trusted."
        ));
        let said = undone().said(true, false);
        assert!(lost(&said));
        assert!(message(&said).ends_with(NOT_AUTOCOMMIT));
        assert!(lost(&saved(2, Vec::new(), false)));
        assert_eq!(
            saved(2, Vec::new(), true),
            Response::End {
                rows: 0,
                rows_affected: Some(2),
                stopped: false,
                messages: Vec::new(),
                more_sets: 0,
            }
        );
    }
}

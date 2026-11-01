use std::mem::ManuallyDrop;

use dbdelve_plugin_protocol::{REFERENCES, Response};
use odbc_api::handles::{self, SqlResult, SqlText};
use odbc_api::sys::{AttrOdbcVersion, InfoType};

use crate::{failure, raw};

/// Below the host's `OPEN_DEADLINE`, so a slow login fails with the driver's
/// own words before the host gives up on the process.
const LOGIN_TIMEOUT: u32 = 30;

/// The environment every connection handle borrows. Leaked: the bridge holds
/// one for the life of the process, and a connection must not outlive it.
pub fn environment() -> Result<&'static handles::Environment, Response> {
    let environment = match handles::Environment::new() {
        SqlResult::Success(environment) | SqlResult::SuccessWithInfo(environment) => environment,
        _ => {
            return Err(failure::plain(
                "The ODBC driver manager could not allocate an environment.",
            ));
        }
    };
    // ODBC 3.0, not 3.80: IBM's Db2 driver (12.1), told 3.80, answers every
    // static `SQLGetInfo` string (`SQL_DRIVER_NAME`, `SQL_SEARCH_PATTERN_ESCAPE`,
    // `SQL_IDENTIFIER_QUOTE_CHAR`) with `03.80`. Nothing here needs 3.80.
    if !succeeded(environment.declare_version(AttrOdbcVersion::Odbc3)) {
        return Err(failure::from(&environment, "SQLSetEnvAttr"));
    }
    Ok(Box::leak(Box::new(environment)))
}

/// An open ODBC connection.
///
/// odbc-api's `Connection` is not used because it never hands out its raw
/// handle, and `SQL_DBMS_VER` and `SQL_DRIVER_NAME` need one; statements are
/// allocated from this handle and run through `raw`.
pub struct Session {
    handle: ManuallyDrop<handles::Connection<'static>>,
    /// Seconds; 0 is none.
    statement_timeout: u32,
}

impl Session {
    /// A string without `=` names a DSN, which `SQLConnect` takes with the user
    /// and password beside it; anything else is a connection string for
    /// `SQLDriverConnect`. A blank password is sent as blank.
    pub fn open(
        environment: &'static handles::Environment,
        connection: &str,
        user: &str,
        password: &str,
        statement_timeout: u32,
    ) -> Result<Self, Response> {
        let mut handle = match environment.allocate_connection() {
            SqlResult::Success(handle) | SqlResult::SuccessWithInfo(handle) => handle,
            _ => return Err(failure::from(environment, "SQLAllocHandle")),
        };
        // A driver with no login timeout still connects, bounded by the host's
        // deadline instead, so its refusal is not a reason to fail the open.
        let _ = handle.set_login_timeout_sec(LOGIN_TIMEOUT);
        let (connected, function) = match is_dsn(connection) {
            true => (
                handle.connect(
                    &SqlText::new(connection.trim()),
                    &SqlText::new(user),
                    &SqlText::new(password),
                ),
                "SQLConnect",
            ),
            false => (
                handle.connect_with_connection_string(&SqlText::new(&connection_string(
                    connection, user, password,
                ))),
                "SQLDriverConnect",
            ),
        };
        match succeeded(connected) {
            true => Ok(Self {
                handle: ManuallyDrop::new(handle),
                statement_timeout,
            }),
            false => Err(failure::from(&handle, function)),
        }
    }

    pub fn handle(&self) -> &handles::Connection<'static> {
        &self.handle
    }

    pub fn statement_timeout(&self) -> u32 {
        self.statement_timeout
    }

    pub fn opened(&self) -> Result<Response, Response> {
        let info = |kind| {
            raw::info_string(&self.handle, kind)
                .ok_or_else(|| failure::from(self.handle(), "SQLGetInfo"))
        };
        Ok(Response::Opened {
            dbms_name: info(InfoType::DbmsName)?,
            dbms_version: info(InfoType::DbmsVer)?,
            driver: info(InfoType::DriverName)?,
            // ODBC cannot say whether a transaction is open, so ending a read
            // early is never provably safe: the bridge always drains.
            stops_early: false,
            optional: vec![REFERENCES.to_string()],
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut disconnected = succeeded(self.handle.disconnect());
        if !disconnected {
            // A transaction left open refuses the disconnect; give it back.
            let _ = self.handle.rollback();
            disconnected = succeeded(self.handle.disconnect());
        }
        if disconnected {
            // SAFETY: this is the handle's last use; `drop` runs once.
            unsafe { ManuallyDrop::drop(&mut self.handle) }
        }
        // Otherwise the handle is leaked: freeing a connected handle fails,
        // and odbc-api panics on that failure.
    }
}

fn succeeded(result: SqlResult<()>) -> bool {
    matches!(
        result,
        SqlResult::Success(()) | SqlResult::SuccessWithInfo(())
    )
}

pub fn is_dsn(connection: &str) -> bool {
    !connection.contains('=')
}

/// A user `a@b` or a password holding `;` or `}` arrives whole. With a user
/// the password key is sent even when blank, since some token-based logins
/// need it present; without one it is still sent when given, for a string that
/// names its own `UID`.
pub fn connection_string(base: &str, user: &str, password: &str) -> String {
    let mut attributes = Vec::new();
    if !user.is_empty() {
        attributes.push(format!("UID={}", value(user)));
    }
    if !user.is_empty() || !password.is_empty() {
        attributes.push(format!("PWD={}", value(password)));
    }
    if attributes.is_empty() {
        return base.to_string();
    }
    let mut string = base.to_string();
    let typed = base.trim_end();
    if !typed.is_empty() && !typed.ends_with(';') {
        string.push(';');
    }
    string.push_str(&attributes.join(";"));
    string
}

/// Braced only where the value needs it. Db2's CLI driver keeps braces as part
/// of the value (12.1, live: `PWD={secret}` is refused as a wrong password), so
/// a plain value goes plain; `@` needs no braces in any driver's parser. A `;`,
/// a brace, or space at either end, which a driver splits on or trims, is
/// braced with `}` doubled, as the ODBC grammar spells it.
fn value(value: &str) -> String {
    let plain = !value.contains([';', '{', '}']) && value.trim() == value;
    match plain {
        true => value.to_string(),
        false => format!("{{{}}}", value.replace('}', "}}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_value_that_needs_braces_gets_them() {
        assert_eq!(
            connection_string("DRIVER={x};HOSTNAME=h", "a@b", "p;w}d"),
            "DRIVER={x};HOSTNAME=h;UID=a@b;PWD={p;w}}d}"
        );
        assert_eq!(
            connection_string("DRIVER={x};", " u", "{p"),
            "DRIVER={x};UID={ u};PWD={{p}"
        );
    }

    #[test]
    fn a_blank_password_is_still_sent() {
        assert_eq!(
            connection_string("DRIVER={x}", "u", ""),
            "DRIVER={x};UID=u;PWD="
        );
    }

    #[test]
    fn without_a_user_only_a_given_password_is_added() {
        assert_eq!(connection_string("DRIVER={x}; ", "", ""), "DRIVER={x}; ");
        assert_eq!(
            connection_string("DRIVER={x};UID=u", "", "p"),
            "DRIVER={x};UID=u;PWD=p"
        );
    }

    #[test]
    fn a_string_without_an_equals_sign_is_a_dsn() {
        assert!(is_dsn("Db2Dev"));
        assert!(!is_dsn("DRIVER={x};HOSTNAME=h"));
    }
}

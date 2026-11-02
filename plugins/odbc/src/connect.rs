use std::mem::ManuallyDrop;
use std::ops::Deref;

use dbdelve_plugin_protocol::{REFERENCES, Response};
use odbc_api::handles::{self, SqlResult, SqlText, StatementImpl};
use odbc_api::sys::{AttrOdbcVersion, InfoType};

use crate::{failure, raw};

/// Below the host's `OPEN_DEADLINE`, so a slow login fails with the driver's
/// own words before the host gives up on the process.
const LOGIN_TIMEOUT: u32 = 30;

/// What ODBC's `SQLSMALLINT` lengths can name, in UTF-16 units; odbc-api
/// panics past it.
const MOST_TEXT: usize = i16::MAX as usize;

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
/// allocated from this handle and run through `raw`. The handle is freed
/// through `raw::free`, never odbc-api's drop.
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
        let dsn = is_dsn(connection);
        let texts = match dsn {
            true => vec![
                connection.trim().to_string(),
                user.to_string(),
                password.to_string(),
            ],
            false => vec![connection_string(connection, user, password)?],
        };
        if texts
            .iter()
            .any(|text| text.encode_utf16().count() > MOST_TEXT)
        {
            return Err(failure::plain(
                "The connection string, user or password is longer than the 32,767 characters ODBC takes.",
            ));
        }
        let mut handle = match environment.allocate_connection() {
            SqlResult::Success(handle) | SqlResult::SuccessWithInfo(handle) => {
                ManuallyDrop::new(handle)
            }
            _ => return Err(failure::from(environment, "SQLAllocHandle")),
        };
        // A driver with no login timeout still connects, bounded by the host's
        // deadline instead, so its refusal is not a reason to fail the open.
        let _ = handle.set_login_timeout_sec(LOGIN_TIMEOUT);
        let (connected, function) = match dsn {
            true => (
                handle.connect(
                    &SqlText::new(&texts[0]),
                    &SqlText::new(&texts[1]),
                    &SqlText::new(&texts[2]),
                ),
                "SQLConnect",
            ),
            false => (
                handle.connect_with_connection_string(&SqlText::new(&texts[0])),
                "SQLDriverConnect",
            ),
        };
        if succeeded(connected) {
            return Ok(Self {
                handle,
                statement_timeout,
            });
        }
        let error = failure::from(&*handle, function);
        // SAFETY: the handle never connected and is not used again.
        unsafe { raw::free(&*handle) };
        Err(failure::scrubbed(error, password, &[value(password)]))
    }

    pub fn handle(&self) -> &handles::Connection<'static> {
        &self.handle
    }

    /// A statement with escape scanning off, refused when the driver will not
    /// turn it off, and with the statement timeout set. A driver that cannot
    /// time a statement out still runs it, and the user's Cancel still reaches
    /// it, so the timeout's refusal is not a reason to fail.
    pub fn statement(&self) -> Result<Statement<'_>, Response> {
        let statement = match self.handle.allocate_statement() {
            SqlResult::Success(statement) | SqlResult::SuccessWithInfo(statement) => {
                Statement(ManuallyDrop::new(statement))
            }
            _ => return Err(failure::from(&*self.handle, "SQLAllocHandle")),
        };
        if !raw::set_noscan(&statement) {
            return Err(failure::plain(
                "The ODBC driver would not turn escape scanning off (SQL_ATTR_NOSCAN), so a statement could reach the server rewritten.",
            ));
        }
        if self.statement_timeout > 0 {
            let _ = raw::set_query_timeout(&statement, self.statement_timeout);
        }
        Ok(statement)
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
            // SAFETY: this is the handle's last use; `drop` runs once, and the
            // odbc-api wrapper stays in its `ManuallyDrop`.
            unsafe { raw::free(&*self.handle) }
        }
        // Otherwise the handle is leaked: a connected handle cannot be freed.
    }
}

/// A statement handle the bridge frees itself, through `raw::free`.
pub struct Statement<'c>(ManuallyDrop<StatementImpl<'c>>);

impl<'c> Deref for Statement<'c> {
    type Target = StatementImpl<'c>;

    fn deref(&self) -> &StatementImpl<'c> {
        &self.0
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        // SAFETY: this is the handle's last use; `drop` runs once, and the
        // odbc-api wrapper stays in its `ManuallyDrop`.
        unsafe { raw::free(&*self.0) }
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

/// A user `a@b` or a password holding a brace arrives whole. With a user the
/// password key is sent even when blank, since some token-based logins need it
/// present; without one it is still sent when given, for a string that names
/// its own `UID`.
///
/// A `;` in either is refused: Db2's CLI driver (12.1, live) splits on it even
/// inside braces, so `PWD={a;SECURITY=SSL;b}` set `SECURITY=SSL`, and a
/// credential would become a key of the string.
pub fn connection_string(base: &str, user: &str, password: &str) -> Result<String, Response> {
    for (field, text) in [("user", user), ("password", password)] {
        if text.contains(';') {
            return Err(failure::plain(&format!(
                "The {field} holds a `;`, which some ODBC drivers read as the end of the value even inside braces, so the bridge will not put it in a connection string."
            )));
        }
    }
    let mut attributes = Vec::new();
    if !user.is_empty() {
        attributes.push(format!("UID={}", value(user)));
    }
    if !user.is_empty() || !password.is_empty() {
        attributes.push(format!("PWD={}", value(password)));
    }
    if attributes.is_empty() {
        return Ok(base.to_string());
    }
    let mut string = base.to_string();
    let typed = base.trim_end();
    if !typed.is_empty() && !typed.ends_with(';') {
        string.push(';');
    }
    string.push_str(&attributes.join(";"));
    Ok(string)
}

/// Braced only where the value cannot be read plain. Db2's CLI driver keeps
/// braces as part of the value (12.1, live: `PWD={secret}` and `PWD={a}}b{c}`
/// are refused, `PWD=a}b{c` is taken), so braces inside a value go plain, as
/// does `@`. A leading `{`, which the ODBC grammar reads as an opening brace,
/// or space at either end, which a driver trims, is braced with `}` doubled.
/// Db2 can take neither: it trims the spaces plain and keeps the braces.
fn value(value: &str) -> String {
    let plain = !value.starts_with('{') && value.trim() == value;
    match plain {
        true => value.to_string(),
        false => format!("{{{}}}", value.replace('}', "}}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(base: &str, user: &str, password: &str) -> String {
        connection_string(base, user, password).unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn only_a_value_that_needs_braces_gets_them() {
        assert_eq!(
            string("DRIVER={x};HOSTNAME=h", "a@b", "p}w{"),
            "DRIVER={x};HOSTNAME=h;UID=a@b;PWD=p}w{"
        );
        assert_eq!(
            string("DRIVER={x};", " u", "{p}"),
            "DRIVER={x};UID={ u};PWD={{p}}}"
        );
    }

    #[test]
    fn a_semicolon_in_a_credential_is_refused_by_name() {
        for (user, password, field) in [("u", "a;b", "password"), ("a;b", "p", "user")] {
            let Err(Response::Error { message, .. }) =
                connection_string("DRIVER={x}", user, password)
            else {
                panic!("a refusal");
            };
            assert!(message.starts_with(&format!("The {field} holds a `;`")));
        }
    }

    #[test]
    fn a_blank_password_is_still_sent() {
        assert_eq!(string("DRIVER={x}", "u", ""), "DRIVER={x};UID=u;PWD=");
    }

    #[test]
    fn without_a_user_only_a_given_password_is_added() {
        assert_eq!(string("DRIVER={x}; ", "", ""), "DRIVER={x}; ");
        assert_eq!(
            string("DRIVER={x};UID=u", "", "p"),
            "DRIVER={x};UID=u;PWD=p"
        );
    }

    #[test]
    fn a_string_without_an_equals_sign_is_a_dsn() {
        assert!(is_dsn("Db2Dev"));
        assert!(!is_dsn("DRIVER={x};HOSTNAME=h"));
    }
}

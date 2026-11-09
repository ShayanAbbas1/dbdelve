use std::mem::ManuallyDrop;
use std::ops::Deref;

use dbdelve_plugin_protocol::{REFERENCES, Response, WRITE};
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
    reads_bound_columns: bool,
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
        if !dsn && let Some(driver) = driver_named(&texts[0]) {
            check_driver(&driver)?;
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
        let mut connected = succeeded(connected);
        let driver = match connected {
            true => raw::info_string(&handle, InfoType::DriverName).unwrap_or_default(),
            false => String::new(),
        };
        if connected
            && !dsn
            && let Some(streaming) = streaming(&driver, &texts[0])
        {
            let _ = handle.disconnect();
            connected = succeeded(handle.connect_with_connection_string(&SqlText::new(&streaming)));
        }
        if connected {
            if is_ibm_cli(&driver) {
                let _ = raw::report_seamless_failover(&handle);
            }
            let reads_bound_columns = raw::reads_bound_columns(&handle);
            return Ok(Self {
                handle,
                statement_timeout,
                reads_bound_columns,
            });
        }
        let error = failure::from(&*handle, function);
        // SAFETY: the handle never connected and is not used again.
        unsafe { raw::free(&*handle) };
        Err(failure::scrubbed(
            error,
            password,
            &[braced_when(password, true)],
        ))
    }

    pub fn handle(&self) -> &handles::Connection<'static> {
        &self.handle
    }

    /// The driver's `SQLGetData` reads bound columns, in any order.
    pub fn reads_bound_columns(&self) -> bool {
        self.reads_bound_columns
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
        let mut optional = vec![REFERENCES.to_string()];
        if raw::transactions(&self.handle) {
            optional.push(WRITE.to_string());
        }
        Ok(Response::Opened {
            dbms_name: info(InfoType::DbmsName)?,
            dbms_version: info(InfoType::DbmsVer)?,
            driver: info(InfoType::DriverName)?,
            // ODBC cannot say whether a transaction is open, so ending a read
            // early is never provably safe: the bridge always drains.
            stops_early: false,
            optional,
        })
    }

    /// Autocommit on or off, through the driver's own transaction control.
    pub fn set_autocommit(&self, on: bool) -> bool {
        succeeded(self.handle.set_autocommit(on))
    }

    /// Commits or rolls back the transaction autocommit being off opened.
    pub fn end(&self, commit: bool) -> bool {
        succeeded(match commit {
            true => self.handle.commit(),
            false => self.handle.rollback(),
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

/// Rows psqlODBC fetches through its cursor at a time.
const CURSOR_ROWS: u32 = 4096;

/// psqlODBC (`UseDeclareFetch=0`, its default) reads a whole result into the
/// bridge's memory before the first row, which defeats a row limit that stops
/// the stream. So when the driver turns out to be psqlODBC and the string
/// leaves the choice open, it is reconnected told to fetch through a cursor,
/// `Fetch` rows at a time unless the string sets that too. In autocommit
/// psqlODBC opens the cursor in a transaction of its own and ends it with the
/// statement.
fn streaming(driver: &str, connection: &str) -> Option<String> {
    if !driver.to_ascii_lowercase().starts_with("psqlodbc") {
        return None;
    }
    let pairs = attributes(connection);
    let names = |keys: &[&str]| {
        pairs
            .iter()
            .any(|(name, _)| keys.iter().any(|key| name.eq_ignore_ascii_case(key)))
    };
    if names(&["UseDeclareFetch", "B6"]) {
        return None;
    }
    let mut string = connection.to_string();
    let typed = connection.trim_end();
    if !typed.is_empty() && !typed.ends_with(';') {
        string.push(';');
    }
    string.push_str("UseDeclareFetch=1");
    if !names(&["Fetch", "A7"]) {
        string.push_str(&format!(";Fetch={CURSOR_ROWS}"));
    }
    Some(string)
}

/// IBM's Db2 driver by the `SQL_DRIVER_NAME` it reports: `libdb2.a` on macOS
/// (12.1, live), and matched loosely beyond it, as only that one was seen.
/// IBM's driver for IBM i and other vendors' Db2 drivers report names of
/// their own.
fn is_ibm_cli(driver: &str) -> bool {
    let driver = driver.to_ascii_lowercase();
    driver.starts_with("libdb2") || driver.starts_with("db2")
}

/// The longest `DRIVER=` value the bridge passes on. unixODBC formats a failed
/// load as the name and the loader's error, which repeats the name, into a
/// 2,048-byte buffer; past that it aborts the process.
const MOST_DRIVER: usize = 512;

/// The `DRIVER=` value the driver manager will load.
pub fn driver_named(connection: &str) -> Option<String> {
    attributes(connection)
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("DRIVER"))
        .map(|(_, value)| value)
}

/// The string's pairs as unixODBC reads them: split on `;`, a value in braces
/// running to a `}` not doubled, the later of two keywords winning, and
/// whichever of `DSN` and `DRIVER`/`FILEDSN` comes first excluding the other.
fn attributes(connection: &str) -> Vec<(String, String)> {
    let bytes = connection.as_bytes();
    let mut at = 0;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let (mut dsn, mut driver) = (false, false);
    loop {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b';') {
            at += 1;
        }
        let start = at;
        while at < bytes.len() && bytes[at] != b'=' {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        let keyword = connection[start..at].to_string();
        at += 1;
        let mut value = Vec::new();
        if bytes.get(at) == Some(&b'{') {
            at += 1;
            while at < bytes.len() && (bytes[at] != b'}' || bytes.get(at + 1) == Some(&b'}')) {
                if bytes[at] == b'}' {
                    at += 1;
                }
                value.push(bytes[at]);
                at += 1;
            }
            if at < bytes.len() {
                at += 1;
            }
        } else {
            while at < bytes.len() && bytes[at] != b';' {
                value.push(bytes[at]);
                at += 1;
            }
        }
        let value = String::from_utf8_lossy(&value).into_owned();
        let is = |name: &str| keyword.eq_ignore_ascii_case(name);
        if is("DSN") {
            if driver {
                continue;
            }
            dsn = true;
        } else if is("DRIVER") || is("FILEDSN") {
            if dsn {
                continue;
            }
            driver = true;
        }
        match pairs
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&keyword))
        {
            Some(pair) => pair.1 = value,
            None => pairs.push((keyword, value)),
        }
    }
    pairs
}

/// Refuses a driver the driver manager could not load, before it tries:
/// unixODBC's report of a failed load overflows its buffer and kills the
/// bridge when the name or the loader's error is long, and macOS's loader
/// lists every path it tried.
fn check_driver(driver: &str) -> Result<(), Response> {
    if driver.trim().is_empty() {
        return Err(failure::plain(
            "The connection string's DRIVER= is empty. Name the driver by its library path (DRIVER=/path/to/library) or by the name the driver manager registers.",
        ));
    }
    if driver.len() > MOST_DRIVER {
        return Err(failure::plain(&format!(
            "The connection string's DRIVER= is longer than the {MOST_DRIVER} bytes the bridge passes to the driver manager."
        )));
    }
    loadable(driver)
}

/// The vendored driver manager loads a driver with exactly this `dlopen`, so
/// a failure here is the failure it would have, reported without its
/// overflow. The library stays loaded: the driver manager's own `dlopen` then
/// finds it, rather than loading it a second time.
#[cfg(target_os = "macos")]
fn loadable(driver: &str) -> Result<(), Response> {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    const RTLD_LAZY: c_int = 0x1;
    const RTLD_GLOBAL: c_int = 0x8;
    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlerror() -> *const c_char;
    }

    let not_loadable = |why: &str| {
        failure::plain(&format!(
            "The connection string's DRIVER={driver} could not be loaded: {why}\nName the driver by its library path (DRIVER=/path/to/library) or by the name the driver manager registers."
        ))
    };
    let library = registered_library(driver).unwrap_or_else(|| driver.to_string());
    let Ok(path) = CString::new(library) else {
        return Err(not_loadable("it holds a NUL character."));
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call; the
    // handle is deliberately never closed.
    let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY | RTLD_GLOBAL) };
    if !handle.is_null() {
        return Ok(());
    }
    // SAFETY: `dlerror` answers NULL or a NUL-terminated string the loader
    // owns, read here before any other loader call on this thread.
    let why = unsafe {
        let why = dlerror();
        match why.is_null() {
            true => "the loader gave no reason.".to_string(),
            false => CStr::from_ptr(why).to_string_lossy().into_owned(),
        }
    };
    Err(not_loadable(&why))
}

#[cfg(not(target_os = "macos"))]
fn loadable(_driver: &str) -> Result<(), Response> {
    Ok(())
}

/// The library `odbcinst.ini` registers under `driver`, looked up as the
/// driver manager does on a 64-bit platform: `Driver64`, then `Driver`.
#[cfg(target_os = "macos")]
fn registered_library(driver: &str) -> Option<String> {
    use std::ffi::{CStr, CString, c_char, c_int};

    unsafe extern "C" {
        fn SQLGetPrivateProfileString(
            section: *const c_char,
            entry: *const c_char,
            default: *const c_char,
            out: *mut c_char,
            size: c_int,
            file: *const c_char,
        ) -> c_int;
    }

    let section = CString::new(driver).ok()?;
    for entry in [c"Driver64", c"Driver"] {
        let mut out = vec![0 as c_char; 1001];
        // SAFETY: every string argument is NUL-terminated and outlives the
        // call; unixODBC writes at most `size` bytes, NUL included, into `out`.
        unsafe {
            SQLGetPrivateProfileString(
                section.as_ptr(),
                entry.as_ptr(),
                c"".as_ptr(),
                out.as_mut_ptr(),
                out.len() as c_int,
                c"ODBCINST.INI".as_ptr(),
            );
        }
        // SAFETY: `out` was zeroed, and the call leaves it NUL-terminated.
        let library = unsafe { CStr::from_ptr(out.as_ptr()) }.to_string_lossy();
        if !library.is_empty() {
            return Some(library.into_owned());
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn registered_library(_driver: &str) -> Option<String> {
    None
}

/// psqlODBC, by the library a `DRIVER=` loads or a name its installers
/// register (`PostgreSQL Unicode`, `PostgreSQL ANSI(x64)`). Another vendor's
/// Postgres driver taken for it gets a braced password, which the ODBC grammar
/// reads as the same value.
fn is_psqlodbc(driver: &str) -> bool {
    let library = registered_library(driver).unwrap_or_else(|| driver.to_string());
    let file = library.rsplit(['/', '\\']).next().unwrap_or_default();
    file.to_ascii_lowercase().starts_with("psqlodbc")
        || driver.trim().to_ascii_lowercase().starts_with("postgresql")
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
        attributes.push(format!("UID={}", user_value(user)));
    }
    if !user.is_empty() || !password.is_empty() {
        let decoded = driver_named(base).is_some_and(|driver| is_psqlodbc(&driver));
        attributes.push(format!("PWD={}", value(password, decoded)));
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
/// is braced with `}` doubled, and so is a password's space at either end,
/// which a driver trims. A user's spaces go plain: psqlODBC (18) strips braces
/// from a password but keeps them in a `UID`, and reads a plain one as
/// written. Db2 can take neither: it trims the spaces plain and keeps the
/// braces.
///
/// psqlODBC (18, `decode` in its `dlg_specific.c`) reads a plain password as
/// URL-encoded, `+` as a space and `%41` as `A`, and a braced one as written,
/// so on that driver a password holding either is braced too. Only a `DRIVER=`
/// says which driver it is before the connect; a `DSN=` string does not.
fn value(value: &str, decoded: bool) -> String {
    let encoded = decoded && value.contains(['+', '%']);
    braced_when(value, value.trim() != value || encoded)
}

fn user_value(user: &str) -> String {
    braced_when(user, false)
}

fn braced_when(value: &str, unreadable: bool) -> String {
    match value.starts_with('{') || unreadable {
        true => format!("{{{}}}", value.replace('}', "}}")),
        false => value.to_string(),
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
            string("DRIVER={x};", "u", "{p}"),
            "DRIVER={x};UID=u;PWD={{p}}}"
        );
    }

    /// psqlODBC takes a `UID` as written, braces and all, and strips braces
    /// only from a password.
    #[test]
    fn spaces_at_either_end_of_a_user_go_plain_and_of_a_password_braced() {
        assert_eq!(
            string("DRIVER={x}", " u ", " p "),
            "DRIVER={x};UID= u ;PWD={ p }"
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
    fn a_password_psqlodbc_would_decode_is_braced_for_it_alone() {
        for driver in [
            "/opt/homebrew/lib/psqlodbcw.so",
            "C:\\psqlODBC\\bin\\psqlodbc35w.dll",
            "{PostgreSQL Unicode(x64)}",
        ] {
            let base = format!("DRIVER={driver}");
            assert_eq!(
                string(&base, "u", "a+b"),
                format!("{base};UID=u;PWD={{a+b}}")
            );
            assert_eq!(
                string(&base, "u", "%41}"),
                format!("{base};UID=u;PWD={{%41}}}}}}")
            );
            assert_eq!(
                string(&base, "u", "plain"),
                format!("{base};UID=u;PWD=plain")
            );
        }
        assert_eq!(
            string("DRIVER=/a/libdb2.dylib", "u", "a+b%41"),
            "DRIVER=/a/libdb2.dylib;UID=u;PWD=a+b%41"
        );
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
    fn the_driver_is_read_as_unix_odbc_reads_it() {
        let driver = |text| driver_named(text);
        assert_eq!(driver("DRIVER=/a/b.so;X=1").as_deref(), Some("/a/b.so"));
        assert_eq!(driver(" ;driver={a;b}}c};X=1").as_deref(), Some("a;b}c"));
        assert_eq!(driver("DRIVER={unclosed").as_deref(), Some("unclosed"));
        assert_eq!(driver("DRIVER=;X=1").as_deref(), Some(""));
        assert_eq!(driver("DRIVER=a;Driver=b").as_deref(), Some("b"));
        assert_eq!(driver("DSN=d;DRIVER=a"), None);
        assert_eq!(driver("DRIVER=a;DSN=d").as_deref(), Some("a"));
        assert_eq!(driver("DRIVER =a"), None);
        assert_eq!(driver("Server=h"), None);
    }

    #[test]
    fn a_blank_or_overlong_driver_is_refused_before_the_driver_manager() {
        for driver in ["", "  ", &"x".repeat(MOST_DRIVER + 1)] {
            let Err(Response::Error { message, .. }) = check_driver(driver) else {
                panic!("a refusal of {driver:?}");
            };
            assert!(message.starts_with("The connection string's DRIVER="));
        }
    }

    #[test]
    fn only_ibm_s_driver_is_told_to_report_a_silent_reconnect() {
        assert!(is_ibm_cli("libdb2.a"));
        for other in ["psqlodbcw.so", "ivdb228.so", "libcwbodbc.so", ""] {
            assert!(!is_ibm_cli(other), "{other}");
        }
    }

    #[test]
    fn a_string_without_an_equals_sign_is_a_dsn() {
        assert!(is_dsn("Db2Dev"));
        assert!(!is_dsn("DRIVER={x};HOSTNAME=h"));
    }
}

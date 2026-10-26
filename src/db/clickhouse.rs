//! ClickHouse, over its HTTP interface: one POST per statement, the statement
//! as the body byte for byte, every result asked for as
//! `TabSeparatedWithNamesAndTypes`.
//!
//! The user's statements share one server session, so `USE`, `SET` and
//! temporary tables carry from one run to the next. DBDelve's own go without
//! one: a session runs one query at a time, and a preview or a catalog load
//! must neither queue behind a long run nor take its lock.

mod http;
#[cfg(test)]
mod mock;
mod tsv;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use ring::rand::{SecureRandom, SystemRandom};
use ureq::http::{HeaderMap, Response};
use ureq::{Agent, Body};

use self::http::{Bounds, Scheme, Watch};
use self::tsv::{Framed, Next, Reader, Untagged};
use super::ssh::tunnelled;
use super::{
    CancelToken, Cancelling, Catalog, Databases, DbError, Fetch, QueryResult, RelationKind,
    RowBuilder, ServerConfig, Structure, plain_error, required_cell,
};
use crate::tls::SslMode;

/// The HTTP interface's plaintext port; its TLS one is 8443.
pub(super) const DEFAULT_PORT: u16 = 8123;

/// What a Cancel's error says, which is how `is_cancel` knows it.
pub(super) const CANCELLED: &str = "Cancelled.";

const FORMAT: &str = "TabSeparatedWithNamesAndTypes";

/// `max_session_timeout`'s default; a server set lower refuses it (374) and
/// the session opens with the server's own default instead.
const SESSION_TIMEOUT: &str = "3600";

const SESSION_NOT_FOUND: i64 = 372;
const INVALID_SESSION_TIMEOUT: i64 = 374;

const SESSION_ENDED: &str = "The server ended the session after it sat idle; USE, SET and \
                             temporary tables from earlier runs are gone. Run it again to run it \
                             in a new session.";

/// The server's version, and the settings a request may carry, with whether
/// this user may change each.
const FACTS_SQL: &str = "SELECT version() AS version, name, value, readonly FROM system.settings \
                         WHERE name IN ('readonly', 'max_execution_time', \
                         'format_tsv_null_representation', 'enable_http_compression')";

/// Carries exactly what every later request will, so a setting this user may
/// not change fails the connect rather than the first run.
const VALIDATION_SQL: &str = "SELECT 1";

/// TSV's NULL, the only spelling the reader tells from text.
const NULL: &str = "\\N";

/// How much of a failed response's body is read for its message.
const ERROR_BODY_LIMIT: u64 = 1024 * 1024;

/// A connection in name only, as Snowflake's is: there is no socket to keep,
/// only the profile, an agent, and the session the user's statements share.
#[derive(Clone)]
pub struct Connection {
    config: Arc<ServerConfig>,
    /// Holds the tunnel, when there is one, in its resolver.
    agent: Agent,
    /// `scheme://host:port/`, with the profile's host even through a tunnel,
    /// so TLS names the real server.
    base: Arc<str>,
    server: Arc<ServerFacts>,
    read_only: Arc<AtomicBool>,
    /// Locked for the whole of a user's request, body and all: the server
    /// holds the session for as long, and refuses a second request on it.
    session: Arc<Mutex<Session>>,
}

/// What the connect learned, fixed for the connection's life.
#[derive(Debug, Default, PartialEq, Eq)]
struct ServerFacts {
    /// `version()`, which an untagged late exception ends in.
    version: String,
    /// The user's own `readonly`. Above 0 the server already holds them to
    /// reads, and refuses any change to `readonly`, so `readonly=2` is never
    /// sent.
    readonly: u8,
    /// Sent with every request.
    settings: Vec<(&'static str, String)>,
}

#[derive(Default)]
struct Session {
    /// Made on the first request, and again after the server lost it.
    id: Option<String>,
    /// The server has it, so a request checks it is still there rather than
    /// quietly opening a new one, and leaves its database to the user's `USE`.
    opened: bool,
    /// The server refused [`SESSION_TIMEOUT`].
    default_timeout: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    /// On the profile's session.
    User,
    /// DBDelve's, run at the user's ask: a preview, an edit.
    Generated,
    /// DBDelve's, unseen: the connect, the catalog, a structure.
    Internal,
    /// A statement of a queue run at once, under that run's hold.
    Alongside { read_only: bool },
}

/// How a request failed: before any answer, kept whole so the connect can
/// tell a plaintext answer to a TLS handshake, or in the server's words.
enum Failed {
    Unreached(ureq::Error),
    Error(DbError),
}

impl From<DbError> for Failed {
    fn from(error: DbError) -> Self {
        Self::Error(error)
    }
}

/// Takes a query id back out of its run's handles however the request ends.
struct Running<'a> {
    running: &'a Mutex<Cancelling>,
    query_id: String,
}

impl<'a> Running<'a> {
    /// Registered before the request is sent, so a Cancel from then on finds
    /// it; a Cancel already asked drops it unsent.
    fn register(cancel: &'a CancelToken, query_id: &str) -> Result<Self, DbError> {
        let mut running = cancel.0.lock().unwrap_or_else(PoisonError::into_inner);
        if running.asked {
            return Err(plain_error(CANCELLED.into()));
        }
        running.handles.push(query_id.to_string());
        Ok(Self {
            running: &cancel.0,
            query_id: query_id.to_string(),
        })
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.handles.retain(|handle| *handle != self.query_id);
    }
}

fn unwired<T>() -> Result<T, DbError> {
    Err(plain_error("ClickHouse is not wired yet.".into()))
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        let port = http::port(server);
        tunnelled(server, port, |tunnel| {
            let unchecked = |scheme| {
                let agent = http::agent(server, tunnel.clone(), scheme, Bounds::internal())?;
                Ok::<_, DbError>(Self::unchecked(server, port, agent, scheme))
            };
            let mut connection = unchecked(Scheme::of(server.sslmode))?;
            let cancel = CancelToken::default();
            let read = match connection.run(FACTS_SQL, Kind::Internal, &cancel, Fetch::default()) {
                Err(Failed::Unreached(error))
                    if server.sslmode == SslMode::Prefer && answered_in_plaintext(&error) =>
                {
                    connection = unchecked(Scheme::Http)?;
                    connection.internal_query(FACTS_SQL)?
                }
                read => read.map_err(|failed| connection.explain(failed))?,
            };
            let connection = Self {
                server: Arc::new(facts(&read, server.statement_timeout)?),
                ..connection
            };
            if !connection.server.settings.is_empty() {
                connection.internal_query(VALIDATION_SQL)?;
            }
            Ok(connection)
        })
    }

    fn unchecked(server: &ServerConfig, port: u16, agent: Agent, scheme: Scheme) -> Self {
        let host = server.host.trim();
        let host = match host.contains(':') && !host.starts_with('[') {
            true => format!("[{host}]"),
            false => host.to_string(),
        };
        Self {
            config: Arc::new(server.clone()),
            agent,
            base: format!("{}://{host}:{port}/", scheme.prefix()).into(),
            server: Arc::default(),
            read_only: Arc::default(),
            session: Arc::default(),
        }
    }

    pub fn query(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        self.request(sql, Kind::User, cancel, fetch)
    }

    pub fn generated(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        self.request(sql, Kind::Generated, cancel, fetch)
    }

    /// Without a session, so the statements of a run at once go out side by
    /// side instead of one at a time on the session's lock.
    pub fn query_alongside(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
        read_only: bool,
    ) -> Result<QueryResult, DbError> {
        self.request(sql, Kind::Alongside { read_only }, cancel, fetch)
    }

    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.request(
            sql,
            Kind::Internal,
            &CancelToken::default(),
            Fetch::default(),
        )
    }

    /// A session of its own, and the profile's hold with it.
    pub fn apart(&self) -> Self {
        Self {
            session: Arc::default(),
            ..self.clone()
        }
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        unwired()
    }

    pub fn databases(&self) -> Result<Databases, DbError> {
        unwired()
    }

    pub fn structure(&self, _schema: &str, _relation: &str) -> Result<Structure, DbError> {
        unwired()
    }

    pub fn ddl(
        &self,
        _schema: &str,
        _relation: &str,
        _kind: RelationKind,
    ) -> Result<String, DbError> {
        unwired()
    }

    pub fn cancel(&self, _cancel: &CancelToken) -> Result<(), DbError> {
        unwired()
    }

    /// Every request from here on carries `readonly=2`, which the user's own
    /// `SET readonly = 0` cannot lift.
    pub fn set_read_only(&self, read_only: bool) -> Result<(), DbError> {
        self.read_only.store(read_only, Ordering::SeqCst);
        Ok(())
    }

    fn request(
        &self,
        sql: &str,
        kind: Kind,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        self.run(sql, kind, cancel, fetch)
            .map_err(|failed| self.explain(failed))
    }

    fn run(
        &self,
        sql: &str,
        kind: Kind,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, Failed> {
        let started = Instant::now();
        let mut session = matches!(kind, Kind::User).then(|| lock(&self.session));
        fetch.hold()?;
        let query_id = random_id()?;
        let _running = Running::register(cancel, &query_id)?;
        let read_only = match kind {
            Kind::Alongside { read_only } => read_only,
            _ => self.read_only.load(Ordering::SeqCst),
        };
        let bounds = match kind {
            Kind::Internal => Bounds::internal(),
            _ => Bounds::user(self.config.statement_timeout),
        };
        let watch = Watch::new(Arc::default(), bounds.limit());
        let _watching = watch.enter();
        loop {
            if let Some(session) = session.as_deref_mut()
                && session.id.is_none()
            {
                *session = Session {
                    id: Some(random_id()?),
                    opened: false,
                    default_timeout: session.default_timeout,
                };
            }
            let params = self.params(&query_id, session.as_deref(), read_only);
            let response = self.post(sql, &params, bounds).map_err(Failed::Unreached)?;
            let status = response.status().as_u16();
            if !http::failed(status, response.headers()) {
                if let Some(session) = session.as_deref_mut() {
                    session.opened = true;
                }
                return Ok(self.read(response, sql, fetch, started)?);
            }
            if let Some(session) = session.as_deref_mut() {
                match http::exception_code(response.headers()) {
                    // Refused before the statement ran, so it is sent again.
                    Some(INVALID_SESSION_TIMEOUT) if !session.default_timeout => {
                        session.default_timeout = true;
                        continue;
                    }
                    // Not run either, but the session it was written for is
                    // gone: run silently in a new one, a `TRUNCATE` could land
                    // in the default database instead of the one `USE`d.
                    Some(SESSION_NOT_FOUND) if session.opened => {
                        session.id = None;
                        session.opened = false;
                        return Err(plain_error(SESSION_ENDED.into()).into());
                    }
                    _ => {}
                }
            }
            return Err(failure(response, sql).into());
        }
    }

    /// The URL's query: what to answer in, which session, and the settings
    /// the connect decided on.
    fn params(&self, query_id: &str, session: Option<&Session>, read_only: bool) -> String {
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        params.append_pair("default_format", FORMAT);
        params.append_pair("query_id", query_id);
        let database = self.config.database.trim();
        let mut send_database = !database.is_empty();
        if let Some(session) = session {
            params.append_pair("session_id", session.id.as_deref().unwrap_or_default());
            if !session.default_timeout {
                params.append_pair("session_timeout", SESSION_TIMEOUT);
            }
            if session.opened {
                params.append_pair("session_check", "1");
                // A per-request database would override the user's `USE`.
                send_database = false;
            }
        }
        if send_database {
            params.append_pair("database", database);
        }
        for (name, value) in &self.server.settings {
            params.append_pair(name, value);
        }
        if read_only && self.server.readonly == 0 {
            params.append_pair("readonly", "2");
        }
        params.finish()
    }

    fn post(&self, sql: &str, params: &str, bounds: Bounds) -> Result<Response<Body>, ureq::Error> {
        let mut request = self
            .agent
            .post(&format!("{}?{params}", self.base))
            .config()
            .timeout_recv_response(bounds.limit())
            .build()
            // Headers rather than the URL, so an `@` in a user name needs no
            // escaping and the password stays out of any proxy's log.
            .header("X-ClickHouse-Key", self.config.password.as_str());
        if !self.config.user.is_empty() {
            request = request.header("X-ClickHouse-User", self.config.user.as_str());
        }
        request.send(sql.as_bytes())
    }

    fn read(
        &self,
        response: Response<Body>,
        sql: &str,
        fetch: Fetch,
        started: Instant,
    ) -> Result<QueryResult, DbError> {
        let headers = response.headers();
        let tag = header(headers, "X-ClickHouse-Exception-Tag").filter(|tag| !tag.is_empty());
        let format = header(headers, "X-ClickHouse-Format").filter(|format| format != FORMAT);
        let written = written_rows(headers);
        let statement: Arc<str> = sql.into();
        let body = response.into_body().into_reader();
        let framed = match &tag {
            Some(tag) => Framed::tagged(body, tag, Arc::clone(&statement)),
            None => Framed::plain(body),
        };
        let source = BufReader::new(framed);
        let mut reader = match format {
            Some(format) => Reader::foreign(source, &format),
            None => Reader::open(
                source,
                tag.is_none().then(|| Untagged {
                    version: self.server.version.clone(),
                    statement,
                }),
            )?,
        };
        if reader.columns().is_empty() {
            return Ok(QueryResult {
                rows_affected: written,
                elapsed: started.elapsed(),
                ..QueryResult::default()
            });
        }
        collect(&mut reader, fetch, started)
    }

    fn explain(&self, failed: Failed) -> DbError {
        let error = match failed {
            Failed::Error(error) => return error,
            Failed::Unreached(error) => error,
        };
        let endpoint = format!("{}:{}", self.config.host.trim(), http::port(&self.config));
        match error {
            ureq::Error::Other(inner) => match inner.downcast::<DbError>() {
                Ok(error) => *error,
                Err(other) => plain_error(format!("{endpoint} was not reached: {other}.")),
            },
            ureq::Error::Protocol(_) => {
                plain_error(format!("{endpoint} did not answer as an HTTP server."))
            }
            error => plain_error(format!("{endpoint} was not reached: {error}.")),
        }
    }
}

/// The rows of a body, the first set into `fetch`'s feed, and the sets after
/// an empty line (`WITH TOTALS`, `extremes`) as further results. Past the
/// limit the first set is drained and counted, never kept.
fn collect<R: BufRead>(
    reader: &mut Reader<R>,
    fetch: Fetch,
    started: Instant,
) -> Result<QueryResult, DbError> {
    let columns = reader.columns().to_vec();
    if let Some(feed) = fetch.feed {
        feed.begin(columns.clone());
    }
    let open = || QueryResult {
        columns: columns.clone(),
        ..QueryResult::default()
    };
    let mut sets = vec![open()];
    let (mut returned, mut kept) = (0, 0);
    let mut filled = None;
    let mut builder = RowBuilder::default();
    loop {
        match reader.next_row(&mut builder)? {
            Next::End => break,
            Next::Section => {
                cap(&mut sets, returned, kept);
                (returned, kept) = (0, 0);
                sets.push(open());
            }
            Next::Row => {
                let row = builder.finish()?;
                let first = sets.len() == 1;
                returned += 1;
                if fetch.limit.is_some_and(|limit| kept >= limit) {
                    if first {
                        returned += reader.drain()?;
                        break;
                    }
                    continue;
                }
                kept += 1;
                if first && fetch.limit == Some(kept) {
                    filled = Some(started.elapsed());
                    if let Some(feed) = fetch.feed {
                        feed.fill();
                    }
                }
                let set = sets.last_mut().expect("a set is open");
                set.bytes += row.text_len();
                match fetch.feed.filter(|_| first) {
                    Some(feed) => feed.push(row),
                    None => set.rows.push(row),
                }
            }
        }
    }
    cap(&mut sets, returned, kept);
    let mut sets = sets.into_iter();
    let mut result = sets.next().expect("the first set");
    result.rest = sets.collect();
    result.elapsed = match filled {
        Some(filled) if result.capped_from.is_some() => filled,
        _ => started.elapsed(),
    };
    Ok(result)
}

fn cap(sets: &mut [QueryResult], returned: usize, kept: usize) {
    if let Some(set) = sets.last_mut() {
        set.capped_from = (returned > kept).then_some(returned);
    }
}

/// What every request will carry, from the connect's read of the server.
///
/// - `max_execution_time` is sent unless it already is the profile's timeout.
///   A user who may not change it and has another value fails the validation
///   request, in the server's words.
/// - `format_tsv_null_representation=\N` is sent wherever it may be, so a
///   user's `SET` of it cannot make NULL and the text `NULL` the same bytes.
///   Where it may not be, it has to be `\N` already.
/// - `enable_http_compression=1` where it is off and may be turned on.
fn facts(read: &QueryResult, statement_timeout: u32) -> Result<ServerFacts, DbError> {
    let mut version = String::new();
    let mut found = HashMap::new();
    for row in &read.rows {
        version = required_cell(read, row, "version")?.to_string();
        let changeable = required_cell(read, row, "readonly")? == "0";
        found.insert(
            required_cell(read, row, "name")?,
            (required_cell(read, row, "value")?, changeable),
        );
    }
    let readonly = found
        .get("readonly")
        .and_then(|(value, _)| value.parse().ok())
        .unwrap_or(0);
    let mut settings = Vec::new();
    if statement_timeout > 0 {
        let current = found
            .get("max_execution_time")
            .and_then(|(value, _)| value.parse::<f64>().ok());
        if current != Some(f64::from(statement_timeout)) {
            settings.push(("max_execution_time", statement_timeout.to_string()));
        }
    }
    match found.get("format_tsv_null_representation") {
        Some((value, false)) if *value == NULL => {}
        Some((value, false)) => {
            return Err(plain_error(format!(
                "The server's format_tsv_null_representation is '{value}' and this user cannot \
                 change it, so NULL cannot be told from text."
            )));
        }
        _ => settings.push(("format_tsv_null_representation", NULL.to_string())),
    }
    if found.get("enable_http_compression") == Some(&("0", true)) {
        settings.push(("enable_http_compression", "1".to_string()));
    }
    Ok(ServerFacts {
        version,
        readonly,
        settings,
    })
}

/// A plain HTTP server sent a TLS handshake answers with an HTTP error, which
/// rustls reads as a record that is no TLS. An alert, a refusal or a closed
/// socket is not that, and `prefer` does not fall back from it.
fn answered_in_plaintext(error: &ureq::Error) -> bool {
    let rustls = match error {
        ureq::Error::Rustls(error) => Some(error),
        ureq::Error::Io(error) => error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>()),
        _ => None,
    };
    matches!(rustls, Some(rustls::Error::InvalidMessage(_)))
}

/// The response's error, from its body.
fn failure(mut response: Response<Body>, sql: &str) -> DbError {
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(ERROR_BODY_LIMIT)
        .read_to_vec()
        .unwrap_or_default();
    http::exception(status, response.headers(), &body, sql)
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    Some(headers.get(name)?.to_str().ok()?.trim().to_string())
}

/// `X-ClickHouse-Summary`, a JSON object whose numbers are strings.
fn written_rows(headers: &HeaderMap) -> Option<u64> {
    let summary: serde_json::Value =
        serde_json::from_str(&header(headers, "X-ClickHouse-Summary")?).ok()?;
    summary["written_rows"].as_str()?.parse().ok()
}

/// `dbdelve-` and 32 hex digits, for a query id or a session id.
fn random_id() -> Result<String, DbError> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| plain_error("The system's random number generator failed.".into()))?;
    let mut id = String::from("dbdelve-");
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    Ok(id)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::mock::{Mock, Request, Response};
    use super::*;
    use crate::db::{Column, Feed};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    const FACTS_COLUMNS: [(&str, &str); 4] = [
        ("version", "String"),
        ("name", "String"),
        ("value", "String"),
        ("readonly", "UInt8"),
    ];

    /// `system.settings` as the connect reads it: name, value (in TSV's
    /// spelling) and whether this user may not change it.
    fn settings(rows: &[[&str; 3]]) -> Response {
        let rows: Vec<Vec<&str>> = rows
            .iter()
            .map(|[name, value, readonly]| vec!["26.8.1.1", name, value, readonly])
            .collect();
        let rows: Vec<&[&str]> = rows.iter().map(Vec::as_slice).collect();
        Response::rows(&FACTS_COLUMNS, &rows)
    }

    /// A user who may change everything, with the server's defaults.
    fn writer() -> Response {
        settings(&[
            ["readonly", "0", "0"],
            ["max_execution_time", "0", "0"],
            ["format_tsv_null_representation", "\\\\N", "0"],
            ["enable_http_compression", "0", "0"],
        ])
    }

    /// A `readonly=1` user, who may change none of them.
    fn reader(max_execution_time: &str) -> Response {
        settings(&[
            ["readonly", "1", "1"],
            ["max_execution_time", max_execution_time, "1"],
            ["format_tsv_null_representation", "\\\\N", "1"],
            ["enable_http_compression", "0", "1"],
        ])
    }

    fn profile(mock: &Mock) -> ServerConfig {
        ServerConfig {
            user: "me@example.com".into(),
            database: "dbdelve_dev".into(),
            ..mock.server()
        }
    }

    fn open_with(
        mock: &Mock,
        facts: Response,
        server: &ServerConfig,
    ) -> Result<Connection, DbError> {
        mock.on_statement("system.settings", [facts]);
        mock.on_statement(
            VALIDATION_SQL,
            [Response::rows(&[("1", "UInt8")], &[&["1"]])],
        );
        Connection::open(server)
    }

    fn open(mock: &Mock) -> Connection {
        open_with(mock, writer(), &profile(mock)).unwrap()
    }

    fn numbers(mock: &Mock) {
        mock.on_statement(
            "SELECT n",
            [Response::rows(&[("n", "UInt8")], &[&["1"], &["2"]])],
        );
    }

    fn run(connection: &Connection, sql: &str) -> Result<QueryResult, DbError> {
        connection.query(sql, &CancelToken::default(), Fetch::default())
    }

    /// The requests that carried `statement`, in order.
    fn sent(mock: &Mock, statement: &str) -> Vec<Request> {
        mock.requests()
            .into_iter()
            .filter(|request| request.statement.contains(statement))
            .collect()
    }

    fn names(request: &Request) -> Vec<&str> {
        let mut names: Vec<&str> = request
            .params
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    fn readonly(mock: &Mock) -> Vec<Option<String>> {
        sent(mock, "SELECT n")
            .iter()
            .map(|request| request.param("readonly").map(str::to_string))
            .collect()
    }

    #[test]
    fn the_connect_reads_without_a_session_or_settings_then_validates_them() {
        let mock = Mock::start();
        open(&mock);
        let [read] = sent(&mock, "system.settings").try_into().unwrap();
        assert_eq!(names(&read), ["database", "default_format", "query_id"]);
        assert_eq!(read.param("database"), Some("dbdelve_dev"));
        assert_eq!(read.param("default_format"), Some(FORMAT));
        let [validation] = sent(&mock, VALIDATION_SQL).try_into().unwrap();
        assert_eq!(validation.statement, VALIDATION_SQL);
        assert_eq!(
            validation.param("format_tsv_null_representation"),
            Some("\\N")
        );
        assert_eq!(validation.param("enable_http_compression"), Some("1"));
        assert_eq!(validation.param("session_id"), None);
    }

    #[test]
    fn the_user_is_sent_in_headers_and_a_blank_password_blank() {
        let mock = Mock::start();
        open(&mock);
        let request = &mock.requests()[0];
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/")
        );
        assert_eq!(request.header("X-ClickHouse-User"), Some("me@example.com"));
        assert_eq!(request.header("X-ClickHouse-Key"), Some(""));
    }

    #[test]
    fn a_users_statements_open_one_session_then_check_it_is_still_there() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        run(&connection, "SELECT n").unwrap();
        let [first, second] = sent(&mock, "SELECT n").try_into().unwrap();
        let session = first.param("session_id").unwrap();
        assert!(session.starts_with("dbdelve-"), "{session}");
        assert_eq!(second.param("session_id"), Some(session));
        assert_eq!(
            names(&first),
            [
                "database",
                "default_format",
                "enable_http_compression",
                "format_tsv_null_representation",
                "query_id",
                "session_id",
                "session_timeout",
            ]
        );
        assert_eq!(first.param("session_timeout"), Some(SESSION_TIMEOUT));
        // The database opens the session only; after that the user's `USE`
        // says.
        assert_eq!(second.param("database"), None);
        assert_eq!(second.param("session_check"), Some("1"));
    }

    #[test]
    fn dbdelves_own_statements_go_without_a_session() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        connection
            .generated("SELECT n", &CancelToken::default(), Fetch::default())
            .unwrap();
        connection
            .query_alongside(
                "SELECT n",
                &CancelToken::alongside(),
                Fetch::default(),
                false,
            )
            .unwrap();
        let requests = sent(&mock, "SELECT n");
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(request.param("session_id"), None);
            assert_eq!(request.param("session_check"), None);
            assert_eq!(request.param("database"), Some("dbdelve_dev"));
        }
    }

    #[test]
    fn a_blank_database_is_left_to_the_server() {
        let mock = Mock::start();
        let server = ServerConfig {
            database: String::new(),
            ..profile(&mock)
        };
        let connection = open_with(&mock, writer(), &server).unwrap();
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        assert!(
            mock.requests()
                .iter()
                .all(|request| request.param("database").is_none())
        );
    }

    #[test]
    fn read_only_is_readonly_2_on_every_request_while_it_is_held() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        let cancel = CancelToken::default();
        connection.set_read_only(true).unwrap();
        run(&connection, "SELECT n").unwrap();
        connection
            .generated("SELECT n", &cancel, Fetch::default())
            .unwrap();
        connection
            .apart()
            .query("SELECT n", &cancel, Fetch::default())
            .unwrap();
        connection.set_read_only(false).unwrap();
        run(&connection, "SELECT n").unwrap();
        let held = Some("2".to_string());
        assert_eq!(readonly(&mock), [held.clone(), held.clone(), held, None]);
    }

    #[test]
    fn a_run_at_once_carries_its_own_hold() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        for read_only in [true, false] {
            connection
                .query_alongside(
                    "SELECT n",
                    &CancelToken::alongside(),
                    Fetch::default(),
                    read_only,
                )
                .unwrap();
        }
        assert_eq!(readonly(&mock), [Some("2".to_string()), None]);
    }

    #[test]
    fn a_user_the_server_holds_to_reads_is_never_sent_readonly() {
        let mock = Mock::start();
        let connection = open_with(&mock, reader("0"), &profile(&mock)).unwrap();
        numbers(&mock);
        connection.set_read_only(true).unwrap();
        run(&connection, "SELECT n").unwrap();
        assert_eq!(readonly(&mock), [None]);
    }

    #[test]
    fn a_query_id_is_dbdelve_and_32_hex_digits_and_never_repeats() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        run(&connection, "SELECT n").unwrap();
        let ids: Vec<String> = mock
            .requests()
            .iter()
            .map(|request| request.query_id().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 4);
        for id in &ids {
            let hex = id.strip_prefix("dbdelve-").unwrap();
            assert_eq!(hex.len(), 32, "{id}");
            assert!(
                hex.bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
                "{id}"
            );
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn the_statement_is_sent_byte_for_byte() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        let sql = "SELECT n FROM t; -- the user's ;\n";
        run(&connection, sql).unwrap();
        assert_eq!(sent(&mock, "SELECT n")[0].statement, sql);
    }

    #[test]
    fn a_writer_is_sent_the_timeout_the_null_spelling_and_compression() {
        let mock = Mock::start();
        let server = ServerConfig {
            statement_timeout: 30,
            ..profile(&mock)
        };
        let connection = open_with(&mock, writer(), &server).unwrap();
        assert_eq!(
            *connection.server,
            ServerFacts {
                version: "26.8.1.1".into(),
                readonly: 0,
                settings: vec![
                    ("max_execution_time", "30".into()),
                    ("format_tsv_null_representation", "\\N".into()),
                    ("enable_http_compression", "1".into()),
                ],
            }
        );
    }

    #[test]
    fn a_readonly_user_is_sent_nothing_it_may_not_change_where_nothing_differs() {
        let mock = Mock::start();
        mock.readonly(&[
            "max_execution_time",
            "format_tsv_null_representation",
            "enable_http_compression",
        ]);
        let server = ServerConfig {
            statement_timeout: 30,
            ..profile(&mock)
        };
        let connection = open_with(&mock, reader("30"), &server).unwrap();
        assert!(connection.server.settings.is_empty());
        assert_eq!(connection.server.readonly, 1);
        assert!(sent(&mock, VALIDATION_SQL).is_empty());
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
    }

    #[test]
    fn a_readonly_user_whose_timeout_differs_fails_the_connect_in_the_servers_words() {
        let mock = Mock::start();
        mock.readonly(&["max_execution_time"]);
        let server = ServerConfig {
            statement_timeout: 30,
            ..profile(&mock)
        };
        let error = open_with(&mock, reader("60"), &server).err().unwrap();
        assert_eq!(
            error.message,
            "Cannot modify 'max_execution_time' setting in readonly mode. (READONLY)"
        );
        let [validation] = sent(&mock, VALIDATION_SQL).try_into().unwrap();
        assert_eq!(validation.param("max_execution_time"), Some("30"));
    }

    #[test]
    fn a_null_spelling_that_cannot_be_changed_back_fails_the_connect() {
        let mock = Mock::start();
        let facts = settings(&[
            ["readonly", "1", "1"],
            ["format_tsv_null_representation", "NULL", "1"],
        ]);
        let error = open_with(&mock, facts, &profile(&mock)).err().unwrap();
        assert_eq!(
            error.message,
            "The server's format_tsv_null_representation is 'NULL' and this user cannot change \
             it, so NULL cannot be told from text."
        );
    }

    fn decided(statement_timeout: u32, rows: &[[&str; 3]]) -> Vec<&'static str> {
        let mut builder = RowBuilder::default();
        let read = QueryResult {
            columns: FACTS_COLUMNS
                .iter()
                .map(|(name, _)| Column {
                    name: name.to_string(),
                    data_type: None,
                })
                .collect(),
            rows: rows
                .iter()
                .map(|row| {
                    builder.push(Some("26.8.1.1"));
                    for cell in row {
                        builder.push(Some(cell));
                    }
                    builder.finish().unwrap()
                })
                .collect(),
            ..QueryResult::default()
        };
        facts(&read, statement_timeout)
            .unwrap()
            .settings
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn only_a_setting_that_differs_is_asked_for() {
        let timeout = |value| decided(30, &[["max_execution_time", value, "0"]]);
        assert!(!timeout("30").contains(&"max_execution_time"));
        assert!(timeout("0").contains(&"max_execution_time"));
        assert!(timeout("31").contains(&"max_execution_time"));
        assert!(!decided(0, &[["max_execution_time", "10", "0"]]).contains(&"max_execution_time"));
        let compression = |value, readonly| {
            decided(0, &[["enable_http_compression", value, readonly]])
                .contains(&"enable_http_compression")
        };
        assert!(compression("0", "0"));
        assert!(!compression("1", "0"));
        assert!(!compression("0", "1"));
    }

    #[test]
    fn an_expired_session_fails_the_run_and_the_next_opens_a_new_one() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        mock.expire_sessions();
        let error = run(&connection, "SELECT n").unwrap_err();
        assert_eq!(error.message, SESSION_ENDED);
        run(&connection, "SELECT n").unwrap();
        let [first, expired, fresh] = sent(&mock, "SELECT n").try_into().unwrap();
        assert_eq!(expired.param("session_id"), first.param("session_id"));
        assert_ne!(fresh.param("session_id"), first.param("session_id"));
        assert_eq!(fresh.param("session_check"), None);
        assert_eq!(fresh.param("database"), Some("dbdelve_dev"));
    }

    #[test]
    fn a_session_timeout_over_the_servers_ceiling_is_dropped_and_the_run_sent_again() {
        let mock = Mock::start();
        mock.max_session_timeout(60);
        let connection = open(&mock);
        numbers(&mock);
        assert_eq!(run(&connection, "SELECT n").unwrap().rows.len(), 2);
        run(&connection, "SELECT n").unwrap();
        let [refused, retried, later] = sent(&mock, "SELECT n").try_into().unwrap();
        assert_eq!(refused.param("session_timeout"), Some(SESSION_TIMEOUT));
        assert_eq!(retried.param("session_timeout"), None);
        assert_eq!(retried.query_id(), refused.query_id());
        assert_eq!(retried.param("session_check"), None);
        assert_eq!(later.param("session_timeout"), None);
        assert_eq!(later.param("session_check"), Some("1"));
    }

    #[test]
    fn a_locked_session_is_reported_in_the_servers_words() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        let session = sent(&mock, "SELECT n")[0]
            .param("session_id")
            .unwrap()
            .to_string();
        mock.on_statement(
            "SELECT sleep",
            [Response::body("x").delayed(Duration::from_millis(400))],
        );
        let port = mock.server().port.unwrap();
        let other = std::thread::spawn(move || {
            ureq::post(&format!("http://127.0.0.1:{port}/?session_id={session}"))
                .send("SELECT sleep(1)")
                .map(|_| ())
        });
        std::thread::sleep(Duration::from_millis(100));
        let error = run(&connection, "SELECT n").unwrap_err();
        assert!(
            error
                .message
                .ends_with("is locked by a concurrent client. (SESSION_IS_LOCKED)"),
            "{error}"
        );
        other.join().unwrap().unwrap();
        run(&connection, "SELECT n").unwrap();
        let requests = sent(&mock, "SELECT n");
        assert_eq!(
            requests[2].param("session_id"),
            requests[0].param("session_id")
        );
    }

    #[test]
    fn a_nonzero_exception_code_on_a_200_is_an_error() {
        let mock = Mock::start();
        let connection = open(&mock);
        // A proxy rewrote the status; the code header still says.
        mock.on_statement(
            "SELECT n",
            [Response::error(
                404,
                60,
                "UNKNOWN_TABLE",
                "Unknown table expression identifier 't'",
            )
            .status(200)],
        );
        let error = run(&connection, "SELECT n FROM t").unwrap_err();
        assert_eq!(
            error.message,
            "Unknown table expression identifier 't' (UNKNOWN_TABLE)"
        );
    }

    #[test]
    fn a_statement_with_a_format_of_its_own_is_one_text_column() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "FORMAT JSONEachRow",
            [Response::body("{\"n\":1}\n{\"n\":2}\n").format("JSONEachRow")],
        );
        let result = run(&connection, "SELECT n FORMAT JSONEachRow").unwrap();
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].name, "JSONEachRow");
        let rows: Vec<_> = result.rows.iter().map(|row| row.get(0).unwrap()).collect();
        assert_eq!(rows, ["{\"n\":1}", "{\"n\":2}"]);
    }

    #[test]
    fn a_commands_written_rows_are_its_rows_affected() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "INSERT",
            [Response::empty().summary(r#"{"read_rows":"3","written_rows":"3"}"#)],
        );
        let result = run(&connection, "INSERT INTO t SELECT * FROM u").unwrap();
        assert_eq!(result.rows_affected, Some(3));
        assert!(result.columns.is_empty());
        numbers(&mock);
        assert_eq!(run(&connection, "SELECT n").unwrap().rows_affected, None);
    }

    #[test]
    fn totals_are_a_further_set() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "WITH TOTALS",
            [Response::rows(
                &[("k", "String"), ("n", "UInt64")],
                &[&["a", "1"], &["b", "2"], &[""], &["", "3"]],
            )],
        );
        let result = run(&connection, "SELECT k, n WITH TOTALS").unwrap();
        assert_eq!(result.rows.len(), 2);
        let [totals] = result.rest.try_into().unwrap();
        assert_eq!(totals.rows.len(), 1);
        assert_eq!(totals.rows[0].get(1), Some("3"));
    }

    #[test]
    fn past_the_limit_the_rows_are_counted_and_the_kept_ones_fed() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [Response::rows(
                &[("n", "UInt8"), ("m", "UInt8")],
                &[
                    &["1", "1"],
                    &["2", "2"],
                    &["3", "3"],
                    &["4", "4"],
                    &[""],
                    &["10", "10"],
                ],
            )],
        );
        let feed = Feed::default();
        let result = connection
            .query(
                "SELECT n, m",
                &CancelToken::default(),
                Fetch {
                    limit: Some(2),
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(result.capped_from, Some(4));
        assert!(result.rows.is_empty());
        let fed = feed.take();
        assert_eq!(fed.rows.len(), 2);
        assert!(fed.filled.is_some());
    }

    #[test]
    fn a_late_error_on_a_server_without_the_tag_is_still_an_error() {
        let mock = Mock::start();
        mock.tagged(false);
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [Response::rows(&[("n", "UInt8")], &[&["1"]]).then_fails(
                395,
                "FUNCTION_THROW_IF_VALUE_IS_NON_ZERO",
                "Value passed to 'throwIf' function is non-zero",
            )],
        );
        let error = run(&connection, "SELECT n").unwrap_err();
        assert_eq!(
            error.message,
            "Value passed to 'throwIf' function is non-zero (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)"
        );
    }

    #[test]
    fn a_hold_that_lets_go_drops_the_statement_unsent() {
        let mock = Mock::start();
        let connection = open(&mock);
        let before = mock.requests().len();
        let held = || false;
        let error = connection
            .query(
                "SELECT n",
                &CancelToken::default(),
                Fetch {
                    held: Some(&held),
                    ..Fetch::default()
                },
            )
            .unwrap_err();
        assert_eq!(error.message, "Cancelled before it was sent.");
        assert_eq!(mock.requests().len(), before);
    }

    #[test]
    fn a_query_id_is_on_the_token_while_it_runs_and_off_it_after() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [Response::rows(&[("n", "UInt8")], &[&["1"]]).delayed(Duration::from_millis(300))],
        );
        let cancel = CancelToken::default();
        let handles = |cancel: &CancelToken| cancel.0.lock().unwrap().handles.clone();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| connection.query("SELECT n", &cancel, Fetch::default()));
            std::thread::sleep(Duration::from_millis(100));
            let ids = handles(&cancel);
            assert_eq!(ids.len(), 1);
            running.join().unwrap().unwrap();
            assert_eq!(sent(&mock, "SELECT n")[0].query_id(), Some(ids[0].as_str()));
        });
        assert!(handles(&cancel).is_empty());
    }

    /// Answers whatever arrives with a plain HTTP error, as an HTTP server
    /// sent a TLS handshake does, or closes without a word.
    fn plaintext_listener(answer: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = [0; 1024];
            let _ = stream.read(&mut seen);
            if answer {
                let _ = stream.write_all(
                    b"HTTP/1.0 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        port
    }

    #[test]
    fn prefer_falls_back_only_from_a_handshake_answered_in_plaintext() {
        let attempt = |answer| {
            let port = plaintext_listener(answer);
            let server = ServerConfig {
                host: "127.0.0.1".into(),
                port: Some(port),
                sslmode: SslMode::Prefer,
                ..ServerConfig::default()
            };
            let agent = http::agent(&server, None, Scheme::Https, Bounds::internal()).unwrap();
            let error = agent
                .post(&format!("https://127.0.0.1:{port}/"))
                .send("SELECT 1")
                .unwrap_err();
            answered_in_plaintext(&error)
        };
        assert!(attempt(true));
        assert!(!attempt(false));
    }

    #[test]
    fn a_server_that_is_not_listening_is_named() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = ServerConfig {
            host: "127.0.0.1".into(),
            port: Some(port),
            sslmode: SslMode::Disable,
            ..ServerConfig::default()
        };
        let error = Connection::open(&server).err().unwrap();
        assert!(
            error
                .message
                .starts_with(&format!("127.0.0.1:{port} was not reached: ")),
            "{error}"
        );
    }
}

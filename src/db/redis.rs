//! The Redis boundary, for Redis and what speaks its protocol (Valkey,
//! KeyDB, Dragonfly).
//!
//! redis-rs's blocking client, so like `mysql.rs` there is one socket behind a
//! mutex and no runtime. The session is RESP3, so a hash arrives as a map;
//! a server that refuses `HELLO` (before Redis 6) is spoken to in RESP2.
//!
//! A command is sent as the words `redis_cmd` split it into, never rewritten,
//! with one exception: a `SCAN`, `HSCAN`, `SSCAN` or `ZSCAN` reply is a cursor
//! and one page, and the page is rarely what was asked for, so the cursor is
//! followed (as `redis-cli --scan` does) by sending the same command again
//! with each cursor the server hands back, until it hands back `0` or the
//! rows reach the limit. A `SCAN`'s keys are shown with their type and TTL,
//! read in one pipelined round trip per page. That reshaping is the only one.
//!
//! The session is dropped whenever a command does not come back whole -- a
//! Cancel, which kills it on the server, a statement timeout, a broken socket
//! -- and the next command dials a fresh one. Whatever the old one held, a
//! `MULTI` or a `WATCH`, goes with it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ::redis::{
    Client, ConnectionAddr, ConnectionLike, ErrorKind, IntoConnectionInfo, ProtocolVersion,
    RedisConnectionInfo, RedisError, ServerError, TlsCertificates, Value,
};

use super::ssh::{Tunnel, tunnelled};
use super::{
    Catalog, Cell, Column, DbError, Fetch, QueryResult, Row, RowBuilder, ServerConfig, SslMode,
    Stopped, Structure, plain_error,
};
use crate::redis_cmd::{self, Command};

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 6379;

/// How long a connect waits for the server, TLS and login included.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// What a Cancel's error says, which is how `is_cancel` knows it.
pub(super) const CANCELLED: &str = "Cancelled.";

/// How many databases a server has when it will not say: Redis's default.
const DEFAULT_DATABASES: u32 = 16;

/// A `redis://` or `rediss://` URL. The scheme says whether to use TLS: plain
/// and verified respectively, unless `sslmode` says otherwise. The user is an
/// ACL user and may be absent, as may the password; the path is the database
/// number.
pub fn config_from_url(url: &str) -> Result<ServerConfig, String> {
    let parsed =
        url::Url::parse(url).map_err(|error| format!("Connection URL is invalid: {error}"))?;
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "Connection URL does not contain a host.".to_string())?
        .to_string();
    if let Some(fragment) = parsed.fragment() {
        return Err(format!(
            "Connection URL fragment #{fragment} is not one dbdelve can pass to Redis."
        ));
    }
    let database = super::percent_decoded(parsed.path().trim_start_matches('/'))?;
    database_number(&database).map_err(|error| error.message)?;

    let tls = parsed.scheme().eq_ignore_ascii_case("rediss");
    let mut sslmode = None;
    let mut root_certificate = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "sslmode" => sslmode = Some(SslMode::parse(value.as_ref())?),
            "sslrootcert" => {
                root_certificate = Some(value.trim().to_string()).filter(|path| !path.is_empty());
            }
            other => {
                return Err(format!(
                    "Connection URL parameter {other} is not one dbdelve can pass to Redis."
                ));
            }
        }
    }
    let sslmode = match (tls, sslmode) {
        (true, Some(SslMode::Disable)) => {
            return Err("Connection URL is rediss:// but sets sslmode=disable.".into());
        }
        (_, Some(mode)) => mode,
        (true, None) => SslMode::VerifyFull,
        (false, None) => SslMode::Disable,
    };

    Ok(ServerConfig {
        host,
        port: parsed.port(),
        database,
        user: super::percent_decoded(parsed.username())?,
        password: parsed
            .password()
            .map(super::percent_decoded)
            .transpose()?
            .unwrap_or_default(),
        sslmode,
        root_certificate,
        // A URL has nowhere to say either; the form is where they are set.
        statement_timeout: 0,
        ssh: None,
    })
}

/// The database a profile names, as the number Redis knows it by. Blank is 0,
/// and so is `db0`, the spelling [`Connection::databases`] lists.
fn database_number(database: &str) -> Result<i64, DbError> {
    let digits = database.trim();
    let digits = digits.strip_prefix("db").unwrap_or(digits);
    match digits {
        "" => Ok(0),
        digits => digits
            .parse::<u32>()
            .map(i64::from)
            .map_err(|_| plain_error(format!("Database {database} is not a database number."))),
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
#[derive(Clone)]
pub struct Connection {
    /// `None` once a command did not come back whole; the next one dials.
    session: Arc<Mutex<Option<Session>>>,
    /// Apart from the session's mutex, which the statement being cancelled
    /// holds -- see [`super::Connection::cancel`].
    running: Arc<Mutex<Running>>,
    /// What the session, a redial and a cancel's side connection are opened
    /// with: the password included, as on MySQL, so nothing above `src/db/`
    /// learns which engine needs a second socket.
    server: ServerConfig,
    /// What every socket dials, and held so ssh runs as long as any clone does.
    tunnel: Option<Arc<Tunnel>>,
}

struct Session {
    client: ::redis::Connection,
    /// The server's name for this session, for `CLIENT KILL ID`. `None` where
    /// the server would not say, which leaves Cancel nothing to name.
    id: Option<i64>,
}

#[derive(Default)]
struct Running {
    /// A user's commands are out, so Cancel has something to stop. Never
    /// DBDelve's own: a catalog load is not what Cancel is for.
    out: bool,
    /// The id of the session they went out on, once it is dialled.
    id: Option<i64>,
    /// Cancel was asked for, which also stops a run before its next round
    /// trip: the next page of a `SCAN`, the next command of several.
    asked: bool,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        database_number(&server.database)?;
        // Before the tunnel, which may have cost a hardware-key touch.
        if server.ssh.is_some() && server.sslmode == SslMode::VerifyFull {
            return Err(plain_error(format!(
                "sslmode=verify-full cannot be honoured through an SSH tunnel on Redis: the \
                 driver checks the certificate against the address it dials, which is the \
                 tunnel's loopback address rather than {}.",
                server.host
            )));
        }
        tunnelled(server, DEFAULT_PORT, |tunnel| {
            let connection = Self {
                session: Arc::default(),
                running: Arc::default(),
                server: server.clone(),
                tunnel,
            };
            *lock(&connection.session) = Some(connection.connect()?);
            Ok(connection)
        })
    }

    /// A session of its own, for a statement to run on beside this one's; see
    /// [`super::Connection::query_alongside`].
    pub fn alongside(&self) -> Result<Self, DbError> {
        let connection = Self {
            session: Arc::default(),
            running: Arc::default(),
            ..self.clone()
        };
        *lock(&connection.session) = Some(connection.connect()?);
        Ok(connection)
    }

    fn connect(&self) -> Result<Session, DbError> {
        let server = &self.server;
        let dial = self.tunnel.as_deref().map(Tunnel::dial).transpose()?;
        let mut client = connect(server, dial)?;
        let id = ::redis::cmd("CLIENT")
            .arg("ID")
            .query::<i64>(&mut client)
            .ok();
        let timeout = (server.statement_timeout > 0)
            .then(|| Duration::from_secs(u64::from(server.statement_timeout)));
        client
            .set_read_timeout(timeout)
            .map_err(|error| plain_error(message(&error)))?;
        Ok(Session { client, id })
    }

    /// `CLIENT KILL ID` over a connection of its own, because the session
    /// being stopped is busy holding the mutex. It ends the session, not just
    /// the command, which Redis has no other way to stop; the next command
    /// dials a new one. A Lua script or a `DEBUG SLEEP` holds the whole
    /// server, so the kill waits behind it like everything else.
    pub fn cancel(&self) -> Result<(), DbError> {
        let id = {
            let mut running = lock(&self.running);
            if !running.out {
                return Ok(());
            }
            running.asked = true;
            running.id
        };
        let Some(id) = id else {
            return Ok(());
        };
        let mut side = self.connect()?;
        // ponytail: a run that ends between this check and the kill loses its
        // idle session to it, and the next command reports that. Closing the
        // gap needs the kill sent under the session's own lock, which the
        // running command holds.
        if !lock(&self.running).out {
            return Ok(());
        }
        ::redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(id)
            .query::<i64>(&mut side.client)
            .map(|_| ())
            .map_err(|error| {
                plain_error(format!(
                    "Could not ask the server to cancel: {}",
                    message(&error)
                ))
            })
    }

    /// Run the commands in `text` in turn on the one session, so a `MULTI`
    /// reaches its `EXEC`, and keep the last one's result. Nothing is sent
    /// from a text holding a line that cannot be read or may not be sent.
    pub fn query(&self, text: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        let commands = redis_cmd::parse(text).map_err(|error| DbError {
            message: error.message,
            position: Some(error.at),
        })?;
        let mut session = lock(&self.session);
        fetch.hold()?;
        let started = Instant::now();
        *lock(&self.running) = Running {
            out: true,
            ..Running::default()
        };
        let mut run = || {
            let mut result = QueryResult::default();
            for (at, command) in commands.iter().enumerate() {
                result =
                    self.command(&mut session, command, fetch.limit)
                        .map_err(|error| match at {
                            0 => error,
                            ran => DbError {
                                message: format!(
                                    "Command {} of {} failed after {} had run: {}",
                                    ran + 1,
                                    commands.len(),
                                    match ran {
                                        1 => "the first".to_string(),
                                        ran => format!("the first {ran}"),
                                    },
                                    error.message
                                ),
                                ..error
                            },
                        })?;
            }
            Ok(result)
        };
        let outcome = run();
        *lock(&self.running) = Running::default();
        let mut result = outcome?;
        result.elapsed = started.elapsed();
        Ok(result)
    }

    fn command(
        &self,
        session: &mut Option<Session>,
        command: &Command,
        limit: Option<usize>,
    ) -> Result<QueryResult, DbError> {
        let cursor_at = match command.name().as_str() {
            "SCAN" => Some(1),
            "HSCAN" | "SSCAN" | "ZSCAN" => Some(2),
            _ => None,
        };
        let request = |words: &[Vec<u8>]| {
            let mut request = ::redis::cmd(&command.name());
            for word in &words[1..] {
                request.arg(word.as_slice());
            }
            request
        };
        let reply = self.send(session, |client| {
            client.req_command(&request(&command.words))
        })?;
        // Inside a `MULTI` the reply is `QUEUED`, and there is no cursor.
        let (Some(at), Some((mut cursor, mut page))) = (cursor_at, scanned(&reply)) else {
            return rendered(command, reply, limit);
        };
        let shape = Scanned::of(&command.name());
        let mut rows = Vec::new();
        loop {
            shape.rows(self, session, &page, &mut rows)?;
            if cursor == b"0" {
                return Ok(table(shape.columns(), rows, limit, None));
            }
            if limit.is_some_and(|limit| rows.len() >= limit) {
                return Ok(table(shape.columns(), rows, limit, Some(Stopped::AtLimit)));
            }
            let mut words = command.words.clone();
            words[at] = cursor;
            let reply = self.send(session, |client| client.req_command(&request(&words)))?;
            (cursor, page) = scanned(&reply).ok_or_else(|| {
                plain_error(
                    "The server answered a cursor with something other than the next page.".into(),
                )
            })?;
        }
    }

    /// One round trip on the session, dialled first if the last one was lost,
    /// and not sent at all once Cancel was asked for. A command that does not
    /// come back whole takes the session with it.
    fn send<T>(
        &self,
        session: &mut Option<Session>,
        call: impl FnOnce(&mut ::redis::Connection) -> Result<T, RedisError>,
    ) -> Result<T, DbError> {
        if session.is_none() {
            *session = Some(self.connect()?);
        }
        let live = session.as_mut().expect("dialled above");
        {
            let mut running = lock(&self.running);
            if running.asked {
                return Err(plain_error(CANCELLED.into()));
            }
            running.id = live.id;
        }
        let outcome = call(&mut live.client);
        let asked = lock(&self.running).asked;
        let error = match outcome {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let lost = error.is_io_error() || error.is_unrecoverable_error() || !live.client.is_open();
        if lost {
            *session = None;
        }
        let timeout = self.server.statement_timeout;
        Err(plain_error(match (asked, lost) {
            (true, _) => CANCELLED.into(),
            (false, true) if error.is_timeout() => format!(
                "Stopped after the statement timeout of {timeout} seconds. The session closed \
                 with it, and anything it held, a MULTI or a WATCH, is gone."
            ),
            (false, true) => format!(
                "{} The session closed with it, and anything it held, a MULTI or a WATCH, is gone.",
                message(&error)
            ),
            (false, false) => message(&error),
        }))
    }

    /// The databases `CONFIG GET databases` says there are, as `db0` and on.
    /// A server that disables `CONFIG`, as managed services do, is taken to
    /// have Redis's default number.
    pub fn databases(&self) -> Result<super::Databases, DbError> {
        let mut session = lock(&self.session);
        let count = self
            .send(&mut session, |client| {
                ::redis::cmd("CONFIG")
                    .arg("GET")
                    .arg("databases")
                    .query::<Value>(client)
            })
            .ok()
            .and_then(|reply| {
                let pairs = match reply {
                    Value::Map(pairs) => pairs,
                    Value::Array(items) => {
                        let mut items = items.into_iter();
                        std::iter::from_fn(|| Some((items.next()?, items.next()?))).collect()
                    }
                    _ => return None,
                };
                let (_, count) = pairs.into_iter().next()?;
                text(&count)?.parse::<u32>().ok()
            })
            .unwrap_or(DEFAULT_DATABASES);
        Ok(super::Databases {
            names: (0..count).map(|index| format!("db{index}")).collect(),
            current: Some(format!("db{}", database_number(&self.server.database)?)),
        })
    }

    // ponytail: the keyspace browser, its structure and its preview arrive
    // with phase 2 of the Redis engine; until then the explorer lists nothing.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    pub fn structure(&self, _schema: &str, _relation: &str) -> Result<Structure, DbError> {
        Ok(Structure::default())
    }

    pub fn ddl(&self, _schema: &str, _relation: &str) -> Result<String, DbError> {
        Err(plain_error(
            "A Redis keyspace has no definition to copy.".into(),
        ))
    }
}

/// Dial and log in, in RESP3 unless the server predates it, under `sslmode`.
fn connect(
    server: &ServerConfig,
    dial: Option<SocketAddr>,
) -> Result<::redis::Connection, DbError> {
    let (host, port) = match dial {
        Some(dial) => (dial.ip().to_string(), dial.port()),
        None => (
            server.host.trim().to_string(),
            server.port.unwrap_or(DEFAULT_PORT),
        ),
    };
    let attempt = |tls: Option<SslMode>| {
        let open = |protocol| {
            client(server, &host, port, tls, protocol)?
                .get_connection_with_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
        };
        match open(ProtocolVersion::RESP3) {
            Err(error) if error.kind() == ErrorKind::RESP3NotSupported => {
                open(ProtocolVersion::RESP2)
            }
            other => other,
        }
    };
    let connected = match server.sslmode {
        SslMode::Disable => attempt(None),
        SslMode::Prefer if speaks_plaintext(&host, port) => attempt(None),
        mode => attempt(Some(mode)),
    };
    connected.map_err(|error| {
        let tls = server.sslmode != SslMode::Disable;
        match error.is_timeout() && tls && speaks_plaintext(&host, port) {
            true => plain_error(format!(
                "{} answers in the clear and speaks no TLS, which sslmode={} requires.",
                server.endpoint(),
                server.sslmode.as_str()
            )),
            false => connect_error(&error, server),
        }
    })
}

/// Whether `host:port` answers a `PING` in the clear, which is how `prefer`
/// learns that TLS is not on offer. Redis listens for TLS or plaintext on a
/// port, never both, and a plaintext one sent a TLS handshake waits for the
/// rest of a command that never comes, so trying TLS first would cost the
/// whole connect timeout. The `PING` carries nothing secret; a TLS port
/// refuses it at once.
fn speaks_plaintext(host: &str, port: u16) -> bool {
    let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECONDS);
    let Some(address) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
    else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return false;
    };
    let mut first = [0];
    stream.set_read_timeout(Some(timeout)).is_ok()
        && stream.write_all(b"PING\r\n").is_ok()
        && stream.read_exact(&mut first).is_ok()
        && matches!(first[0], b'+' | b'-')
}

/// The client for one attempt. `require` (and `prefer`'s TLS attempt) checks
/// no certificate, `verify-ca` the chain and not the name, `verify-full`
/// both, against the platform's trust store or the profile's root
/// certificate, which replaces it.
fn client(
    server: &ServerConfig,
    host: &str,
    port: u16,
    tls: Option<SslMode>,
    protocol: ProtocolVersion,
) -> Result<Client, RedisError> {
    let address = match tls {
        None => ConnectionAddr::Tcp(host.to_string(), port),
        Some(mode) => ConnectionAddr::TcpTls {
            host: host.to_string(),
            port,
            insecure: !mode.checks_certificate(),
            tls_params: None,
        },
    };
    let mut login = RedisConnectionInfo::default()
        .set_protocol(protocol)
        .set_lib_name("DBDelve", env!("CARGO_PKG_VERSION"));
    if !server.user.is_empty() {
        login = login.set_username(&server.user);
    }
    if !server.password.is_empty() {
        login = login.set_password(&server.password);
    }
    let info = address
        .into_connection_info()?
        .set_redis_settings(
            login.set_db(database_number(&server.database).map_err(|error| {
                RedisError::from((ErrorKind::InvalidClientConfig, "database", error.message))
            })?),
        );
    let Some(mode) = tls.filter(|mode| mode.checks_certificate()) else {
        return Client::open(info);
    };
    let client = match &server.root_certificate {
        Some(path) => {
            let root = std::fs::read(crate::store::home_expanded(path)).map_err(|error| {
                RedisError::from((
                    ErrorKind::InvalidClientConfig,
                    "root certificate",
                    format!("Could not read {path}: {error}"),
                ))
            })?;
            Client::build_with_tls(
                info,
                TlsCertificates {
                    client_tls: None,
                    root_cert: Some(root),
                },
            )?
        }
        None => Client::open(info)?,
    };
    if mode != SslMode::VerifyCa {
        return Ok(client);
    }
    let info = client.get_connection_info().clone();
    let mut address = info.addr().clone();
    address.set_danger_accept_invalid_hostnames(true);
    Client::open(info.set_addr(address))
}

fn connect_error(error: &RedisError, server: &ServerConfig) -> DbError {
    let endpoint = server.endpoint();
    plain_error(if error.is_connection_refusal() {
        format!("Connection refused: nothing is listening on {endpoint}")
    } else if error.is_timeout() {
        format!("No answer from {endpoint} within {CONNECT_TIMEOUT_SECONDS} seconds.")
    } else if error.code().is_some() {
        message(error)
    } else {
        format!("{endpoint}: {}", message(error))
    })
}

/// The server's own words for its errors (`WRONGTYPE Operation against a key
/// holding the wrong kind of value`), and the driver's for the rest.
fn message(error: &RedisError) -> String {
    match (error.code(), error.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        (Some(code), None) => code.to_string(),
        (None, Some(detail)) if error.kind() == ErrorKind::InvalidClientConfig => {
            detail.to_string()
        }
        _ => error.to_string(),
    }
}

fn server_message(error: &ServerError) -> String {
    match error.details() {
        Some(detail) => format!("{} {detail}", error.code()),
        None => error.code().to_string(),
    }
}

/// A `*SCAN` reply's cursor and page.
fn scanned(reply: &Value) -> Option<(Vec<u8>, Vec<Value>)> {
    let Value::Array(parts) = reply else {
        return None;
    };
    let [cursor, Value::Array(page)] = parts.as_slice() else {
        return None;
    };
    Some((text(cursor)?.into_bytes(), page.clone()))
}

/// Column names and the rows under them.
type Grid = (Vec<String>, Vec<Vec<Cell>>);

/// The rows each `*SCAN` page holds.
enum Scanned {
    /// `SCAN`: keys, each shown with its type and TTL.
    Keys,
    /// `HSCAN`: field, value, field, value.
    Fields,
    /// `SSCAN`: members.
    Members,
    /// `ZSCAN`: member, score, member, score.
    Scores,
}

impl Scanned {
    fn of(name: &str) -> Self {
        match name {
            "SCAN" => Self::Keys,
            "HSCAN" => Self::Fields,
            "SSCAN" => Self::Members,
            _ => Self::Scores,
        }
    }

    fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::Keys => &["key", "type", "ttl"],
            Self::Fields => &["field", "value"],
            Self::Members => &["member"],
            Self::Scores => &["member", "score"],
        }
    }

    fn rows(
        &self,
        connection: &Connection,
        session: &mut Option<Session>,
        page: &[Value],
        rows: &mut Vec<Vec<Cell>>,
    ) -> Result<(), DbError> {
        match self {
            Self::Members => rows.extend(page.iter().map(|member| vec![cell(member)])),
            Self::Fields | Self::Scores => rows.extend(
                page.chunks(2)
                    .map(|pair| pair.iter().map(cell).collect::<Vec<_>>()),
            ),
            Self::Keys if page.is_empty() => {}
            Self::Keys => {
                let mut pipe = ::redis::pipe();
                for key in page {
                    let key = bytes(key).unwrap_or_default();
                    pipe.cmd("TYPE").arg(key).cmd("TTL").arg(key);
                }
                let described: Vec<Value> =
                    connection.send(session, |client| pipe.query(client))?;
                for (key, described) in page.iter().zip(described.chunks(2)) {
                    let ttl = described.get(1).and_then(text).filter(|ttl| ttl != "-1");
                    rows.push(vec![cell(key), described.first().and_then(text), ttl]);
                }
            }
        }
        Ok(())
    }
}

/// `reply` as a grid: by the command's shape where it has one worth reading,
/// else by the reply's own.
fn rendered(command: &Command, reply: Value, limit: Option<usize>) -> Result<QueryResult, DbError> {
    let name = command.name();
    let reply = match reply {
        Value::ServerError(error) => return Err(plain_error(server_message(&error))),
        Value::Attribute { data, .. } => *data,
        reply => reply,
    };
    let affected = match (&reply, COUNTS.contains(&name.as_str())) {
        (Value::Int(count), true) => u64::try_from(*count).ok(),
        _ => None,
    };
    let scores = command
        .words
        .iter()
        .any(|word| word.eq_ignore_ascii_case(b"WITHSCORES"))
        || matches!(
            name.as_str(),
            "ZPOPMIN" | "ZPOPMAX" | "BZPOPMIN" | "BZPOPMAX"
        );
    let named = |columns: &[&str], rows: Vec<Vec<Cell>>| -> Grid {
        (columns.iter().map(|name| name.to_string()).collect(), rows)
    };
    let shaped = match (&reply, name.as_str(), command.subcommand().as_str()) {
        (_, "INFO", _) => {
            text(&reply).map(|info| named(&["section", "key", "value"], info_rows(&info)))
        }
        (_, "CLIENT", "LIST" | "INFO") => text(&reply).map(|list| client_rows(&list)),
        (Value::Array(entries), "XRANGE" | "XREVRANGE", _) => stream_rows(entries),
        (Value::Array(items), _, _) if scores => {
            paired(items).map(|rows| named(&["member", "score"], rows))
        }
        (Value::Array(items), "HGETALL" | "HRANDFIELD", _)
        | (Value::Array(items), "CONFIG", "GET") => {
            paired(items).map(|rows| named(&["field", "value"], rows))
        }
        _ => None,
    };
    let mut result = match shaped {
        Some((columns, rows)) => table(&columns, rows, limit, None),
        None => generic(reply, limit),
    };
    result.rows_affected = affected;
    Ok(result)
}

fn table<C: AsRef<str>>(
    columns: &[C],
    rows: Vec<Vec<Cell>>,
    limit: Option<usize>,
    stopped: Option<Stopped>,
) -> QueryResult {
    let total = rows.len();
    let kept = limit.map_or(total, |limit| limit.min(total));
    let mut builder = RowBuilder::default();
    let mut bytes = 0;
    let rows: Vec<Row> = rows
        .into_iter()
        .take(kept)
        .filter_map(|row| {
            for value in &row {
                bytes += value.as_ref().map_or(0, String::len);
                builder.push(value.as_deref());
            }
            builder.finish().ok()
        })
        .collect();
    QueryResult {
        columns: columns
            .iter()
            .map(|name| Column {
                name: name.as_ref().to_string(),
                data_type: None,
            })
            .collect(),
        rows,
        bytes,
        capped_from: (kept < total && stopped.is_none()).then_some(total),
        stopped,
        ..QueryResult::default()
    }
}

/// A reply read by its own shape: a map as fields and values, an array of
/// maps as one row each under the union of their keys, any other array one
/// row per element, and anything else one value.
fn generic(reply: Value, limit: Option<usize>) -> QueryResult {
    match reply {
        Value::Map(pairs) => table(
            &["field", "value"],
            pairs
                .iter()
                .map(|(field, value)| vec![cell(field), cell(value)])
                .collect(),
            limit,
            None,
        ),
        Value::Array(items) | Value::Set(items)
            if !items.is_empty() && items.iter().all(|item| matches!(item, Value::Map(_))) =>
        {
            let records = items
                .iter()
                .map(|item| match item {
                    Value::Map(pairs) => pairs
                        .iter()
                        .map(|(field, value)| (text(field).unwrap_or_default(), cell(value)))
                        .collect(),
                    _ => Vec::new(),
                })
                .collect();
            let (columns, rows) = union(records);
            table(&columns, rows, limit, None)
        }
        Value::Array(items) | Value::Set(items) => table(
            &["value"],
            items.iter().map(|item| vec![cell(item)]).collect(),
            limit,
            None,
        ),
        scalar => table(&["value"], vec![vec![cell(&scalar)]], limit, None),
    }
}

/// `member, score` or `field, value` rows, from RESP3's array of pairs or
/// RESP2's flat run of both.
fn paired(items: &[Value]) -> Option<Vec<Vec<Cell>>> {
    let nested = items
        .iter()
        .all(|item| matches!(item, Value::Array(pair) if pair.len() == 2));
    if nested {
        return Some(
            items
                .iter()
                .filter_map(|item| match item {
                    Value::Array(pair) => Some(pair.iter().map(cell).collect()),
                    _ => None,
                })
                .collect(),
        );
    }
    items.len().is_multiple_of(2).then(|| {
        items
            .chunks(2)
            .map(|pair| pair.iter().map(cell).collect())
            .collect()
    })
}

/// `INFO`'s text: `# Section` headers over `key:value` lines.
fn info_rows(info: &str) -> Vec<Vec<Cell>> {
    let mut section = String::new();
    let mut rows = Vec::new();
    for line in info.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('#') {
            section = name.trim().to_string();
        } else if let Some((key, value)) = line.split_once(':') {
            rows.push(vec![
                Some(section.clone()),
                Some(key.to_string()),
                Some(value.to_string()),
            ]);
        }
    }
    rows
}

/// `CLIENT LIST`'s text: a line per client of `key=value` pairs.
fn client_rows(list: &str) -> Grid {
    let clients: Vec<Vec<(String, Cell)>> = list
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split_whitespace()
                .map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (key.to_string(), Some(value.to_string()))
                })
                .collect()
        })
        .collect();
    union(clients)
}

/// `XRANGE`'s entries: the id, then every field any entry holds. A field
/// list is a flat run of names and values, or a map.
fn stream_rows(entries: &[Value]) -> Option<Grid> {
    let entries = entries
        .iter()
        .map(|entry| {
            let Value::Array(parts) = entry else {
                return None;
            };
            let [id, fields] = parts.as_slice() else {
                return None;
            };
            let fields: Vec<(String, Cell)> = match fields {
                Value::Map(pairs) => pairs
                    .iter()
                    .map(|(field, value)| (text(field).unwrap_or_default(), cell(value)))
                    .collect(),
                Value::Array(flat) => flat
                    .chunks(2)
                    .map(|pair| {
                        (
                            pair.first().and_then(text).unwrap_or_default(),
                            pair.get(1).and_then(cell),
                        )
                    })
                    .collect(),
                _ => return None,
            };
            Some(
                std::iter::once(("id".to_string(), cell(id)))
                    .chain(fields)
                    .collect(),
            )
        })
        .collect::<Option<Vec<_>>>()?;
    Some(union(entries))
}

/// Records of named cells as one grid, under every name any of them has, in
/// the order they first appear.
fn union(records: Vec<Vec<(String, Cell)>>) -> Grid {
    let mut columns: Vec<String> = Vec::new();
    for record in &records {
        for (name, _) in record {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }
    }
    let rows = records
        .into_iter()
        .map(|mut record| {
            columns
                .iter()
                .map(|column| {
                    record
                        .iter_mut()
                        .find(|(name, _)| name == column)
                        .and_then(|(_, value)| value.take())
                })
                .collect()
        })
        .collect();
    (columns, rows)
}

/// The text of a scalar reply, `None` for nil and for anything nested.
fn text(value: &Value) -> Option<String> {
    Some(match value {
        Value::BulkString(bytes) => shown(bytes),
        Value::SimpleString(text) => text.clone(),
        Value::VerbatimString { text, .. } => text.clone(),
        Value::Okay => "OK".into(),
        Value::Int(number) => number.to_string(),
        Value::Double(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::BigNumber(digits) => String::from_utf8_lossy(digits).into_owned(),
        Value::ServerError(error) => server_message(error),
        _ => return None,
    })
}

/// A cell: a scalar's text, nil as NULL, and anything nested as compact JSON.
fn cell(value: &Value) -> Cell {
    match value {
        Value::Nil => None,
        Value::Attribute { data, .. } => cell(data),
        nested @ (Value::Array(_) | Value::Set(_) | Value::Map(_) | Value::Push { .. }) => {
            Some(json(nested).to_string())
        }
        scalar => text(scalar),
    }
}

fn json(value: &Value) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Value::Nil => Json::Null,
        Value::Int(number) => Json::from(*number),
        Value::Boolean(flag) => Json::from(*flag),
        Value::Double(number) => Json::from(*number),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            Json::Array(items.iter().map(json).collect())
        }
        Value::Map(pairs) => Json::Object(
            pairs
                .iter()
                .map(|(key, value)| (text(key).unwrap_or_default(), json(value)))
                .collect(),
        ),
        Value::Attribute { data, .. } => json(data),
        scalar => text(scalar).map_or(Json::Null, Json::from),
    }
}

/// The raw bytes of a bulk or simple string, for sending back as an argument.
fn bytes(value: &Value) -> Option<&[u8]> {
    match value {
        Value::BulkString(bytes) => Some(bytes),
        Value::SimpleString(text) => Some(text.as_bytes()),
        _ => None,
    }
}

/// A value's text where it is UTF-8; otherwise redis-cli's quoted spelling of
/// its bytes, which reads back as the same bytes when typed as an argument.
fn shown(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => redis_cmd::quoted_bytes(bytes),
    }
}

/// Commands whose integer reply counts what they changed.
const COUNTS: &[&str] = &[
    "DEL",
    "UNLINK",
    "HSET",
    "HDEL",
    "SADD",
    "SREM",
    "ZADD",
    "ZREM",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LREM",
    "XDEL",
    "XACK",
    "XTRIM",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
    "ZINTERSTORE",
    "ZUNIONSTORE",
    "ZDIFFSTORE",
    "ZRANGESTORE",
    "JSON.DEL",
    "JSON.FORGET",
];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Engine, is_cancel, ssh::live_bastion};

    #[test]
    fn the_scheme_says_whether_to_use_tls_and_the_path_is_the_database() {
        let plain = config_from_url("redis://127.0.0.1:56379/2").unwrap();
        assert_eq!(plain.sslmode, SslMode::Disable);
        assert_eq!((plain.port, plain.database.as_str()), (Some(56379), "2"));
        assert_eq!((plain.user.as_str(), plain.password.as_str()), ("", ""));

        let tls = config_from_url("rediss://app%40corp:s%3Acret@cache.example.test").unwrap();
        assert_eq!(tls.sslmode, SslMode::VerifyFull);
        assert_eq!(
            (tls.user.as_str(), tls.password.as_str()),
            ("app@corp", "s:cret")
        );
        assert_eq!(tls.database, "");

        let password_only = config_from_url("redis://:secret@h/0?sslmode=require").unwrap();
        assert_eq!(password_only.password, "secret");
        assert_eq!(password_only.sslmode, SslMode::Require);

        for refused in [
            "rediss://h/0?sslmode=disable",
            "redis://h/zero",
            "redis://h/0?protocol=resp3",
            "redis://h/0#insecure",
        ] {
            assert!(config_from_url(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_database_is_its_number_or_its_listed_name() {
        assert_eq!(database_number("").unwrap(), 0);
        assert_eq!(database_number("3").unwrap(), 3);
        assert_eq!(database_number("db3").unwrap(), 3);
        assert!(database_number("-1").is_err());
    }

    fn command(line: &str) -> Command {
        redis_cmd::parse(line).unwrap().remove(0)
    }

    fn grid(line: &str, reply: Value) -> Grid {
        let result = rendered(&command(line), reply, None).unwrap();
        let rows = cells(&result);
        (
            result
                .columns
                .into_iter()
                .map(|column| column.name)
                .collect(),
            rows,
        )
    }

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    fn some(cells: &[&str]) -> Vec<Cell> {
        cells.iter().map(|cell| Some(cell.to_string())).collect()
    }

    #[test]
    fn a_reply_renders_by_its_own_shape() {
        assert_eq!(grid("GET nope", Value::Nil).1, vec![vec![None]]);
        let (columns, rows) = grid(
            "HGETALL h",
            Value::Map(vec![(bulk("a"), bulk("1")), (bulk("b"), Value::Int(2))]),
        );
        assert_eq!(columns, ["field", "value"]);
        assert_eq!(rows, [some(&["a", "1"]), some(&["b", "2"])]);

        let (columns, rows) = grid(
            "FT.SEARCH i *",
            Value::Array(vec![
                Value::Map(vec![(bulk("id"), bulk("1"))]),
                Value::Map(vec![(bulk("name"), bulk("x")), (bulk("id"), bulk("2"))]),
            ]),
        );
        assert_eq!(columns, ["id", "name"]);
        assert_eq!(rows, [vec![Some("1".into()), None], some(&["2", "x"])]);

        let (columns, rows) = grid(
            "EVAL x 0",
            Value::Array(vec![
                bulk("a"),
                Value::Array(vec![Value::Int(1), Value::Nil]),
            ]),
        );
        assert_eq!(columns, ["value"]);
        assert_eq!(rows, [some(&["a"]), some(&["[1,null]"])]);

        // Lossless and typeable: the bytes read back as an argument.
        let (_, rows) = grid("GET bin", Value::BulkString(b"\x00\xffok".to_vec()));
        assert_eq!(rows, [some(&[r#""\x00\xffok""#])]);
    }

    #[test]
    fn scores_pair_up_whether_nested_or_flat() {
        let nested = Value::Array(vec![
            Value::Array(vec![bulk("ada"), Value::Double(100.0)]),
            Value::Array(vec![bulk("grace"), Value::Double(85.5)]),
        ]);
        let flat = Value::Array(vec![bulk("ada"), bulk("100"), bulk("grace"), bulk("85.5")]);
        for reply in [nested, flat] {
            let (columns, rows) = grid("ZRANGE z 0 -1 withscores", reply);
            assert_eq!(columns, ["member", "score"]);
            assert_eq!(rows, [some(&["ada", "100"]), some(&["grace", "85.5"])]);
        }
    }

    #[test]
    fn info_and_client_list_read_as_tables() {
        let (columns, rows) = grid(
            "INFO",
            bulk("# Server\r\nredis_version:8.0.0\r\n\r\n# Clients\r\nconnected_clients:2\r\n"),
        );
        assert_eq!(columns, ["section", "key", "value"]);
        assert_eq!(
            rows,
            [
                some(&["Server", "redis_version", "8.0.0"]),
                some(&["Clients", "connected_clients", "2"])
            ]
        );
        let (columns, rows) = grid(
            "CLIENT LIST",
            bulk("id=3 addr=1.2.3.4:5 name=\nid=4 addr=1.2.3.4:6 name=app lib-name=x\n"),
        );
        assert_eq!(columns, ["id", "addr", "name", "lib-name"]);
        assert_eq!(
            rows[0],
            [
                Some("3".into()),
                Some("1.2.3.4:5".into()),
                Some(String::new()),
                None
            ]
        );
    }

    #[test]
    fn a_stream_is_its_ids_under_every_field_its_entries_hold() {
        let entry = |id: &str, fields: &[&str]| {
            Value::Array(vec![
                bulk(id),
                Value::Array(fields.iter().map(|field| bulk(field)).collect()),
            ])
        };
        let (columns, rows) = grid(
            "XRANGE s - +",
            Value::Array(vec![
                entry("1-0", &["user", "1"]),
                entry("2-0", &["user", "2", "via", "ada"]),
            ]),
        );
        assert_eq!(columns, ["id", "user", "via"]);
        assert_eq!(
            rows,
            [
                vec![Some("1-0".into()), Some("1".into()), None],
                some(&["2-0", "2", "ada"])
            ]
        );
    }

    #[test]
    fn the_limit_keeps_rows_and_says_how_many_there_were() {
        let reply = Value::Array((0..5).map(Value::Int).collect());
        let result = rendered(&command("LRANGE l 0 -1"), reply, Some(2)).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.capped_from, Some(5));
    }

    fn live_config() -> ServerConfig {
        let url = std::env::var("dbdelve_REDIS_URL").expect("dbdelve_REDIS_URL is required");
        config_from_url(&url).expect("dbdelve_REDIS_URL should parse")
    }

    fn live() -> Connection {
        Connection::open(&live_config()).expect("connection should open")
    }

    fn ran(connection: &Connection, text: &str) -> QueryResult {
        connection
            .query(text, Fetch::default())
            .unwrap_or_else(|error| panic!("{text}: {}", error.message))
    }

    fn cells(result: &QueryResult) -> Vec<Vec<Cell>> {
        result
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.map(str::to_string)).collect())
            .collect()
    }

    fn blocked_then_cancelled(connection: &Connection) -> DbError {
        let canceller = connection.clone();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                canceller
                    .cancel()
                    .expect("the kill should reach the server");
            });
            connection
                .query("BLPOP live:never 0", Fetch::default())
                .unwrap_err()
        })
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_development_database_is_fully_seeded() {
        let connection = live();
        assert_eq!(cells(&ran(&connection, "DBSIZE")), [some(&["214"])]);
        assert_eq!(cells(&ran(&connection, "GET item:200")), [some(&["200"])]);
        let databases = connection.databases().unwrap();
        assert_eq!(databases.names.len(), 16);
        assert_eq!(databases.current.as_deref(), Some("db0"));
        let other = Connection::open(&ServerConfig {
            database: "db1".into(),
            ..live_config()
        })
        .unwrap();
        assert_eq!(cells(&ran(&other, "GET other:key")), [some(&["in db 1"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_each_type_reads_back_as_a_grid() {
        let connection = live();
        let hash = ran(&connection, "HGETALL user:1");
        assert_eq!(hash.columns[0].name, "field");
        assert!(cells(&hash).contains(&some(&["name", "Ada Lovelace"])));
        assert_eq!(
            cells(&ran(&connection, "LRANGE queue:emails 0 -1")),
            [some(&["welcome"]), some(&["reminder"]), some(&["receipt"])]
        );
        assert_eq!(ran(&connection, "SMEMBERS tags:redis").rows.len(), 3);
        assert_eq!(
            cells(&ran(&connection, "ZRANGE leaderboard 0 -1 WITHSCORES"))[0],
            some(&["linus", "72"])
        );
        let stream = ran(&connection, "XRANGE events:signup - +");
        let names: Vec<&str> = stream.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "user", "plan", "referrer"]);
        assert_eq!(
            cells(&ran(&connection, "JSON.GET doc:1 $")),
            [some(&[
                r#"[{"name":"Ada","langs":["en","fr"],"active":true}]"#
            ])]
        );
        assert_eq!(
            cells(&ran(&connection, "GET bin:blob")),
            [some(&[r#""\x00\xff\xfebinary\x01""#])]
        );
        assert_eq!(
            cells(&ran(&connection, r#"GET "key with spaces and \"quotes\"""#)),
            [some(&["weird"])]
        );
        assert_eq!(cells(&ran(&connection, "GET nothing:here")), [vec![None]]);
        let info = ran(&connection, "INFO server");
        assert!(
            cells(&info)
                .iter()
                .any(|row| row[1].as_deref() == Some("redis_version"))
        );
        assert!(
            ran(&connection, "CLIENT LIST")
                .columns
                .iter()
                .any(|column| column.name == "id")
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_scan_follows_its_cursor_to_the_end_or_the_limit() {
        let connection = live();
        let keys = ran(&connection, "SCAN 0 MATCH item:* COUNT 10");
        let names: Vec<&str> = keys.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["key", "type", "ttl"]);
        assert_eq!(keys.rows.len(), 200);
        assert_eq!(keys.stopped, None);
        assert!(
            cells(&keys)
                .iter()
                .all(|row| row[1].as_deref() == Some("string") && row[2].is_none())
        );

        let session = ran(&connection, "SCAN 0 MATCH session:*");
        assert!(
            cells(&session)[0][2]
                .as_deref()
                .is_some_and(|ttl| ttl.parse::<u64>().is_ok())
        );

        let capped = connection
            .query(
                "SCAN 0 MATCH item:* COUNT 10",
                Fetch {
                    limit: Some(25),
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(capped.rows.len(), 25);
        assert_eq!(capped.stopped, Some(Stopped::AtLimit));

        assert_eq!(ran(&connection, "HSCAN user:1 0").rows.len(), 3);
        assert_eq!(
            ran(&connection, "SSCAN tags:redis 0").columns[0].name,
            "member"
        );
        assert_eq!(
            cells(&ran(&connection, "ZSCAN leaderboard 0 MATCH ada")),
            [some(&["ada", "100"])]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_errors_are_the_servers_and_refusals_send_nothing() {
        let connection = live();
        let error = connection
            .query("HGETALL greeting", Fetch::default())
            .unwrap_err();
        assert!(error.message.starts_with("WRONGTYPE"), "{}", error.message);

        let refused = connection
            .query("SET refused:marker 1\nSELECT 1", Fetch::default())
            .unwrap_err();
        assert!(refused.message.contains("SELECT"), "{}", refused.message);
        assert_eq!(
            cells(&ran(&connection, "EXISTS refused:marker")),
            [some(&["0"])]
        );

        let Err(wrong) = Connection::open(&ServerConfig {
            password: "wrong".into(),
            ..live_config()
        }) else {
            panic!("a wrong password connected");
        };
        assert!(wrong.message.starts_with("WRONGPASS"), "{}", wrong.message);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_multi_queues_on_the_one_session_until_exec() {
        let connection = live();
        ran(&connection, "DEL live:multi");
        assert_eq!(cells(&ran(&connection, "MULTI")), [some(&["OK"])]);
        assert_eq!(
            cells(&ran(&connection, "SET live:multi 1")),
            [some(&["QUEUED"])]
        );
        assert_eq!(
            cells(&ran(&connection, "INCR live:multi")),
            [some(&["QUEUED"])]
        );
        assert_eq!(
            cells(&ran(&connection, "EXEC")),
            [some(&["OK"]), some(&["2"])]
        );
        assert_eq!(ran(&connection, "DEL live:multi").rows_affected, Some(1));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_cancel_kills_a_blocked_command_and_the_next_one_redials() {
        let connection = live();
        let started = Instant::now();
        let error = blocked_then_cancelled(&connection);
        assert!(is_cancel(Engine::Redis, &error), "{}", error.message);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(cells(&ran(&connection, "PING")), [some(&["PONG"])]);
        // Nothing out: a cancel then stops nothing and costs nothing.
        connection.cancel().unwrap();
        assert_eq!(cells(&ran(&connection, "PING")), [some(&["PONG"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_statement_timeout_stops_a_blocked_command() {
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 1,
            ..live_config()
        })
        .unwrap();
        let error = connection
            .query("BLPOP live:never 0", Fetch::default())
            .unwrap_err();
        assert!(
            error
                .message
                .starts_with("Stopped after the statement timeout"),
            "{}",
            error.message
        );
        assert_eq!(cells(&ran(&connection, "PING")), [some(&["PONG"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_prefer_falls_back_to_a_server_that_speaks_no_tls_and_require_does_not() {
        let connection = Connection::open(&ServerConfig {
            sslmode: SslMode::Prefer,
            ..live_config()
        })
        .expect("prefer should connect in the clear");
        assert_eq!(cells(&ran(&connection, "PING")), [some(&["PONG"])]);
        let Err(refused) = Connection::open(&ServerConfig {
            sslmode: SslMode::Require,
            ..live_config()
        }) else {
            panic!("require fell back to plaintext");
        };
        assert!(
            refused.message.contains("speaks no TLS"),
            "{}",
            refused.message
        );
    }

    #[test]
    #[ignore = "requires the dev bastions and Redis configured through dbdelve_SSH_CONFIG and dbdelve_REDIS_URL"]
    fn live_ssh_a_command_and_a_cancel_run_through_the_bastion() {
        // The compose server as the bastion sees it, by service name and the
        // port inside the network.
        let connection = Connection::open(&ServerConfig {
            host: "redis".into(),
            port: Some(DEFAULT_PORT),
            ssh: live_bastion("dbdelve-bastion"),
            ..live_config()
        })
        .expect("connection should open");
        assert_eq!(cells(&ran(&connection, "GET counter")), [some(&["42"])]);
        let error = blocked_then_cancelled(&connection);
        assert!(is_cancel(Engine::Redis, &error), "{}", error.message);
    }
}

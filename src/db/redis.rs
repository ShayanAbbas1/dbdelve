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
//! rows reach the limit. A `SCAN`'s keys are shown with their type, TTL and
//! value (the start of a string's or JSON document's own, a collection's
//! size), read in two pipelined round trips per page for the keys the limit
//! keeps. That reshaping is the only one.
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
    Catalog, Cell, Column, ColumnDefinition, DbError, EditTarget, Fetch, NamedDefinition,
    QueryResult, Relation, RelationKind, Row, RowBuilder, Schema, ServerConfig, SslMode, Stopped,
    Structure, plain_error,
};
use crate::redis_cmd::browse::PREVIEWED;
use crate::redis_cmd::{self, Command, edit};

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 6379;

/// How long a connect waits for the server, TLS and login included.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// What a Cancel's error says, which is how `is_cancel` knows it.
pub(super) const CANCELLED: &str = "Cancelled.";

/// How much of a string or a document the keys grid holds: what the row
/// inspector shows of a value.
const SHOWN: usize = crate::result_grid::FIELD_DISPLAY_LIMIT;

/// The most characters of a collection's JSON the keys grid shows, past which
/// it shows the size: what a column can show readably, so longer JSON would
/// only be cut off by the column anyway.
const GLANCE: usize = 200;

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
        (true, Some(SslMode::Prefer)) => {
            return Err(
                "Connection URL is rediss:// but sets sslmode=prefer, which can fall back to \
                 plaintext."
                    .into(),
            );
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
        schema: None,
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
    /// A `MULTI` is open, which a query tab may leave so across runs: every
    /// command sent now is only queued.
    multi: bool,
    /// A query tab's `WATCH` holds, which the `EXEC` of a generated run's own
    /// `WATCH` would clear along with its own.
    watching: bool,
}

fn in_multi(session: &Option<Session>) -> bool {
    session.as_ref().is_some_and(|session| session.multi)
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
    /// Which run this is, counted up across the connection's life, so a Cancel
    /// aimed at one cannot land on the next that reuses its session.
    run: u64,
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
        // Browsing and the user's own commands then leave keys' LRU/LFU, which
        // eviction goes by, untouched; 7.2+.
        let _ = ::redis::cmd("CLIENT")
            .arg("NO-TOUCH")
            .arg("ON")
            .query::<()>(&mut client);
        let timeout = (server.statement_timeout > 0)
            .then(|| Duration::from_secs(u64::from(server.statement_timeout)));
        client
            .set_read_timeout(timeout)
            .map_err(|error| plain_error(message(&error)))?;
        Ok(Session {
            client,
            id,
            multi: false,
            watching: false,
        })
    }

    /// `CLIENT KILL ID` over a connection of its own, because the session
    /// being stopped is busy holding the mutex. It ends the session, not just
    /// the command, which Redis has no other way to stop; the next command
    /// dials a new one. A Lua script or a `DEBUG SLEEP` holds the whole
    /// server, so the kill waits behind it like everything else.
    pub fn cancel(&self) -> Result<(), DbError> {
        let (id, run) = {
            let mut running = lock(&self.running);
            if !running.out {
                return Ok(());
            }
            running.asked = true;
            (running.id, running.run)
        };
        let Some(id) = id else {
            return Ok(());
        };
        let mut side = self.connect()?;
        // ponytail: a run that ends between this check and the kill loses its
        // idle session to it, and the next command reports that. Closing the
        // gap needs the kill sent under the session's own lock, which the
        // running command holds.
        let still_out = {
            let running = lock(&self.running);
            running.out && running.run == run
        };
        if !still_out {
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
        self.run(text, fetch, false)
    }

    /// [`Connection::query`] for commands DBDelve wrote, where a reply saying
    /// the write did nothing is an error ([`unapplied`]) and stops the rest.
    pub fn generated(&self, text: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(text, fetch, true)
    }

    fn run(&self, text: &str, fetch: Fetch, generated: bool) -> Result<QueryResult, DbError> {
        let database = format!("db{}", database_number(&self.server.database)?);
        let commands = redis_cmd::parse(text).map_err(|error| DbError {
            message: error.message,
            position: Some(error.at),
        })?;
        let mut session = lock(&self.session);
        if generated && in_multi(&session) {
            return Err(plain_error(
                "A query tab left this session inside a MULTI, so this would only be queued: \
                 run EXEC or DISCARD in that tab first."
                    .into(),
            ));
        }
        let watches = commands.iter().any(|command| command.name() == "WATCH");
        if generated && watches && session.as_ref().is_some_and(|session| session.watching) {
            return Err(plain_error(
                "A query tab left a WATCH on this session, which this edit's EXEC would clear: \
                 run UNWATCH, or the MULTI and EXEC it guards, in that tab first."
                    .into(),
            ));
        }
        fetch.hold()?;
        let started = Instant::now();
        let run = {
            let mut running = lock(&self.running);
            let run = running.run + 1;
            *running = Running {
                out: true,
                run,
                ..Running::default()
            };
            run
        };
        let mut multi = false;
        let mut watching = false;
        let mut step = || {
            let mut result = QueryResult::default();
            // What a collection edit's `ECHO` says the next read must find.
            let mut echoed: Option<Vec<u8>> = None;
            for (at, command) in commands.iter().enumerate() {
                let was_multi = in_multi(&session);
                let expected = echoed.take().filter(|_| {
                    matches!(
                        command.name().as_str(),
                        "HSCAN" | "SSCAN" | "ZSCAN" | "LRANGE"
                    )
                });
                let outcome = self.command(
                    &mut session,
                    command,
                    fetch.limit,
                    generated,
                    expected.as_deref(),
                );
                if generated && command.name() == "ECHO" {
                    echoed = command.words.get(1).cloned();
                }
                let open = match command.name().as_str() {
                    "MULTI" if outcome.is_ok() => Some(true),
                    "EXEC" | "DISCARD" if session.is_some() => Some(false),
                    _ => None,
                };
                if let Some(open) = open {
                    multi = open;
                    if let Some(live) = session.as_mut() {
                        live.multi = open;
                    }
                }
                if let Some(live) = session.as_mut() {
                    match command.name().as_str() {
                        "WATCH" if outcome.is_ok() && !generated => live.watching = true,
                        "UNWATCH" if outcome.is_ok() => live.watching = false,
                        // Outside a MULTI either is an error that leaves a WATCH be.
                        "EXEC" | "DISCARD" if was_multi => live.watching = false,
                        _ => {}
                    }
                }
                match command.name().as_str() {
                    "WATCH" if outcome.is_ok() => watching = true,
                    "UNWATCH" | "EXEC" | "DISCARD" => watching = false,
                    _ => {}
                }
                result = outcome.map_err(|error| match at {
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
        let mut outcome: Result<QueryResult, DbError> = step();
        // The `MULTI` this run opened would answer the next command with
        // QUEUED, though the editor's text no longer shows it open.
        if let (Err(error), true, true) = (&mut outcome, multi, session.is_some()) {
            let note = match self.send(&mut session, |client| {
                client.req_command(&::redis::cmd("DISCARD"))
            }) {
                Ok(_) => {
                    if let Some(live) = session.as_mut() {
                        live.multi = false;
                    }
                    "The MULTI it opened was discarded."
                }
                Err(_) => {
                    *session = None;
                    "The session was closed to drop the MULTI it opened."
                }
            };
            error.message = format!("{} {note}", error.message);
        } else if outcome.is_err() && watching && generated && session.is_some() {
            // DBDelve's own `WATCH`, which would void the user's next `EXEC`.
            if self
                .send(&mut session, |client| {
                    client.req_command(&::redis::cmd("UNWATCH"))
                })
                .is_err()
            {
                *session = None;
            }
        }
        *lock(&self.running) = Running {
            run,
            ..Running::default()
        };
        let mut result = outcome?;
        result.elapsed = started.elapsed();
        if let [command] = commands.as_slice() {
            result.edit = edit_target(command, &result, &database);
        }
        Ok(result)
    }

    fn command(
        &self,
        session: &mut Option<Session>,
        command: &Command,
        limit: Option<usize>,
        generated: bool,
        expected: Option<&[u8]>,
    ) -> Result<QueryResult, DbError> {
        let cursor_at = match command.name().as_str() {
            "SCAN" => Some(1),
            "HSCAN" | "SSCAN" | "ZSCAN" => Some(2),
            _ => None,
        };
        let request = |words: &[Vec<u8>]| {
            let mut request = ::redis::Cmd::new();
            for word in words {
                request.arg(word.as_slice());
            }
            request
        };
        let reply = self.send(session, |client| {
            client.req_command(&request(&command.words))
        })?;
        // A check reads one page, never the cursor followed.
        if let Some(expected) = expected {
            verified(command, &reply, expected)?;
            return Ok(QueryResult::default());
        }
        // Inside a `MULTI` the reply is `QUEUED`, and there is no cursor.
        let (Some(at), Some((mut cursor, mut page))) = (cursor_at, scanned(&reply)) else {
            if generated {
                unapplied(command, &reply)?;
            }
            return rendered(command, reply, limit);
        };
        let shape = Scanned::of(command);
        let mut rows = Vec::new();
        let mut tags = Vec::new();
        loop {
            shape.rows(self, session, &page, limit, &mut rows, &mut tags)?;
            if cursor == b"0" {
                return Ok(tagged(table(shape.columns(), rows, limit, None)?, tags));
            }
            if limit.is_some_and(|limit| rows.len() >= limit) {
                let stopped = table(shape.columns(), rows, limit, Some(Stopped::AtLimit))?;
                return Ok(tagged(stopped, tags));
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
    /// and not sent at all once Cancel was asked for, which closes the session
    /// as a kill would. A command that does not come back whole takes the
    /// session with it.
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
                // The command skipped may have been the `EXEC` or `DISCARD`
                // closing a `MULTI`, which would then queue the next run's.
                *session = None;
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
    /// have Redis's default number, and so is one whose session a query tab
    /// holds in a `MULTI`, which would queue the `CONFIG` into the user's
    /// transaction and, where `CONFIG` is not allowed, abort it.
    pub fn databases(&self) -> Result<super::Databases, DbError> {
        let mut session = lock(&self.session);
        let count = (!in_multi(&session))
            .then(|| {
                self.send(&mut session, |client| {
                    ::redis::cmd("CONFIG")
                        .arg("GET")
                        .arg("databases")
                        .query::<Value>(client)
                })
                .ok()
            })
            .flatten()
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

    /// One schema, named for the database as [`Connection::databases`] lists
    /// it, holding the one relation a Redis database has: its keyspace, with
    /// `DBSIZE` as its row count where the server will say, and outside a
    /// query tab's `MULTI`, as with [`Connection::databases`].
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let name = format!("db{}", database_number(&self.server.database)?);
        let mut session = lock(&self.session);
        let rows = (!in_multi(&session))
            .then(|| {
                self.send(&mut session, |client| {
                    ::redis::cmd("DBSIZE").query::<u64>(client)
                })
                .ok()
            })
            .flatten();
        Ok(Catalog {
            schemas: vec![Schema {
                name,
                relations: vec![Relation {
                    name: redis_cmd::browse::KEYS.into(),
                    kind: RelationKind::Table,
                    partition_of: None,
                    size: None,
                    rows,
                }],
                routines: Vec::new(),
            }],
        })
    }

    /// The keyspace's columns, which are the preview's own: nothing declares
    /// them. A key names one value, so it is the key.
    pub fn structure(&self, _schema: &str, _relation: &str) -> Result<Structure, DbError> {
        let column = |name: &str, data_type: &str, nullable: bool| ColumnDefinition {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            default: None,
        };
        Ok(Structure {
            columns: vec![
                column("key", "string", false),
                column("type", "string", false),
                // NULL where the key never expires.
                column("ttl", "integer", true),
                // NULL for a type the grid has no read for.
                column("value", "string", true),
            ],
            constraints: vec![NamedDefinition {
                name: "key".into(),
                definition: "PRIMARY KEY (key)".into(),
            }],
            ..Structure::default()
        })
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
            Err(error) if refused_hello(&error) => open(ProtocolVersion::RESP2),
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

/// Whether the server turned `HELLO 3` down, as one from before Redis 6 does
/// (unknown command), one that has the command but not the protocol does
/// (`NOPROTO`), and a proxy may with a plain `ERR`.
fn refused_hello(error: &RedisError) -> bool {
    error.kind() == ErrorKind::RESP3NotSupported || matches!(error.code(), Some("NOPROTO" | "ERR"))
}

/// Whether `host:port` answers a `PING` in the clear, which is how `prefer`
/// learns that TLS is not on offer. Redis listens for TLS or plaintext on a
/// port, never both, and a plaintext one sent a TLS handshake waits for the
/// rest of a command that never comes, so trying TLS first would cost the
/// whole connect timeout. The `PING` carries nothing secret; a TLS port
/// refuses it at once.
fn speaks_plaintext(host: &str, port: u16) -> bool {
    let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECONDS);
    let answers = |address: SocketAddr| {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
            return false;
        };
        let mut first = [0];
        stream.set_read_timeout(Some(timeout)).is_ok()
            && stream.write_all(b"PING\r\n").is_ok()
            && stream.read_exact(&mut first).is_ok()
            && matches!(first[0], b'+' | b'-')
    };
    (host, port)
        .to_socket_addrs()
        .is_ok_and(|mut addresses| addresses.any(answers))
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
            // redis-rs offers no ClientConfig of our own (its TlsConnParams
            // are crate-private), so `require` takes its insecure path: any
            // certificate is accepted, handshake signatures go unchecked
            // (which weakens nothing, as the peer is unauthenticated anyway)
            // and SNI is not sent.
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
    // A user with no password is still logged in as that user: sending no
    // credentials at all would leave the session on `default`.
    if !server.password.is_empty() || !server.user.is_empty() {
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
    /// `HSCAN … NOVALUES`: fields.
    FieldNames,
    /// `SSCAN`: members.
    Members,
    /// `ZSCAN`: member, score, member, score.
    Scores,
}

impl Scanned {
    fn of(command: &Command) -> Self {
        let flagged = |flag: &[u8]| scan_flag(&command.words, flag);
        match command.name().as_str() {
            "SCAN" => Self::Keys,
            "HSCAN" if flagged(b"NOVALUES") => Self::FieldNames,
            "HSCAN" => Self::Fields,
            "SSCAN" => Self::Members,
            _ if flagged(b"NOSCORES") => Self::Members,
            _ => Self::Scores,
        }
    }

    fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::Keys => &["key", "type", "ttl", "value"],
            Self::Fields => &["field", "value"],
            Self::FieldNames => &["field"],
            Self::Members => &["member"],
            Self::Scores => &["member", "score"],
        }
    }

    fn rows(
        &self,
        connection: &Connection,
        session: &mut Option<Session>,
        page: &[Value],
        limit: Option<usize>,
        rows: &mut Vec<Vec<Cell>>,
        tags: &mut Vec<Vec<&'static str>>,
    ) -> Result<(), DbError> {
        match self {
            Self::Members | Self::FieldNames => {
                rows.extend(page.iter().map(|member| vec![cell(member)]));
                tags.extend(page.iter().map(|member| vec![tag(member)]));
            }
            Self::Fields | Self::Scores => {
                rows.extend(
                    page.chunks(2)
                        .map(|pair| pair.iter().map(cell).collect::<Vec<_>>()),
                );
                tags.extend(page.chunks(2).map(|pair| match self {
                    Self::Scores => vec![tag(&pair[0]), edit::DOUBLE],
                    _ => pair.iter().map(tag).collect(),
                }));
            }
            Self::Keys => {
                let room = limit.map_or(page.len(), |limit| limit.saturating_sub(rows.len()));
                let (page, past) = page.split_at(room.min(page.len()));
                // A key past the limit is not shown, only counted toward the
                // total a capped result reports, so nothing is read about it.
                let counted = past.iter().map(|_| Vec::new());
                if page.is_empty() {
                    tags.extend(past.iter().map(|_| Vec::new()));
                    rows.extend(counted);
                    return Ok(());
                }
                // Per-command errors stay in the replies: a user allowed to SCAN
                // but not to TYPE or TTL still sees the keys, with those cells NULL.
                let mut pipe = ::redis::pipe();
                pipe.ignore_errors();
                for key in page {
                    let key = bytes(key).unwrap_or_default();
                    pipe.cmd("TYPE").arg(key).cmd("TTL").arg(key);
                }
                let described: Vec<Value> =
                    connection.send(session, |client| pipe.query(client))?;
                let kinds: Vec<Option<String>> = described
                    .chunks(2)
                    .map(|pair| pair.first().and_then(answer))
                    .collect();
                // A string or a document is shown as far as the row inspector
                // shows a value, a collection as its size and first entries
                // ([`shown_value`]).
                let mut values = ::redis::pipe();
                values.ignore_errors();
                let mut read = Vec::new();
                for (key, kind) in page.iter().zip(&kinds) {
                    let summary = kind.as_deref().and_then(summary);
                    let key = bytes(key).unwrap_or_default();
                    match summary {
                        Some(("GETRANGE", _)) => {
                            values.cmd("STRLEN").arg(key);
                            values.cmd("GETRANGE").arg(key).arg(0).arg(SHOWN - 1);
                        }
                        Some((command, _)) => {
                            values.cmd(command).arg(key);
                            if let Some(sample) = sample(command, key) {
                                values.add_command(sample);
                            }
                        }
                        None => {}
                    }
                    read.push(summary);
                }
                let mut replies: std::vec::IntoIter<Value> = match values.is_empty() {
                    true => Vec::new().into_iter(),
                    false => connection
                        .send(session, |client| values.query::<Vec<Value>>(client))?
                        .into_iter(),
                };
                for ((key, described), read) in page.iter().zip(described.chunks(2)).zip(read) {
                    let ttl = described.get(1).and_then(answer).filter(|ttl| ttl != "-1");
                    let (value, shown_as) = match read {
                        Some((command, counted)) => {
                            let length = match command {
                                "GETRANGE" => replies.next(),
                                _ => None,
                            };
                            let reply = replies.next();
                            let sampled = sample(command, &[]).and_then(|_| replies.next());
                            match reply {
                                Some(reply) => shown_value(
                                    command,
                                    counted,
                                    &reply,
                                    length.as_ref(),
                                    sampled.as_ref(),
                                ),
                                None => (None, edit::SUMMARY),
                            }
                        }
                        None => (None, edit::SUMMARY),
                    };
                    rows.push(vec![
                        cell(key),
                        described.first().and_then(answer),
                        ttl,
                        value,
                    ]);
                    tags.push(vec![tag(key), edit::STRING, edit::INTEGER, shown_as]);
                }
                tags.extend(past.iter().map(|_| Vec::new()));
                rows.extend(counted);
            }
        }
        Ok(())
    }
}

/// The keys grid's value of a key, read by `command`, and the tag that says
/// whether it is the whole of it. A string is shown as far as the row
/// inspector shows a value and marked `…` past that, and so is a document;
/// either is tagged [`edit::TRUNCATED`] then, which no edit writes back, so
/// the cut is never written over the value it was cut from. A collection is
/// its JSON where `sampled` read all of it and that fits in [`GLANCE`]
/// characters, tagged by its type so an edit can diff it, unless a byte of it
/// is not text or a score is infinite, which JSON cannot carry back; else its
/// size.
fn shown_value(
    command: &str,
    counted: Option<[&str; 2]>,
    reply: &Value,
    length: Option<&Value>,
    sampled: Option<&Value>,
) -> (Cell, &'static str) {
    match (counted, reply, length) {
        (_, Value::ServerError(_), _) => (None, edit::SUMMARY),
        (Some([one, many]), Value::Int(size), _) => {
            let noun = if *size == 1 { one } else { many };
            let whole = sampled
                .and_then(|sampled| entries(command, sampled))
                .filter(|(entries, ..)| usize::try_from(*size) == Ok(entries.len()))
                .map(|(entries, [open, close], text)| {
                    (format!("{open}{}{close}", entries.join(",")), text)
                })
                .filter(|(json, _)| json.chars().count() <= GLANCE);
            let tag = match command {
                "HLEN" => edit::HASH_JSON,
                "LLEN" => edit::LIST_JSON,
                "SCARD" => edit::SET_JSON,
                _ => edit::ZSET_JSON,
            };
            // A sparse hashtable or set can answer one page with a cursor that
            // is not 0, which the edit's check read refuses.
            let finished = sampled.is_some_and(|sampled| {
                !matches!(command, "HLEN" | "SCARD")
                    || scanned(sampled).is_some_and(|(cursor, _)| cursor == b"0")
            });
            match whole {
                Some((json, true)) if finished => (Some(json), tag),
                Some((json, _)) => (Some(json), edit::SUMMARY),
                None => (Some(format!("{size} {noun}")), edit::SUMMARY),
            }
        }
        (_, Value::BulkString(prefix), Some(Value::Int(length))) => {
            let whole = usize::try_from(*length).is_ok_and(|length| length <= prefix.len());
            let shown_as = match whole {
                true => tag(reply),
                false => edit::TRUNCATED,
            };
            (Some(prefixed(prefix, *length)), shown_as)
        }
        // ponytail: a document has no ranged read, so the server still sends
        // the whole of it and the cut is only what the grid keeps.
        // `JSON.DEBUG MEMORY` first could skip large ones, a round trip a page.
        _ => {
            let text = cell(reply);
            let cut = text
                .as_deref()
                .and_then(|text| text.char_indices().nth(SHOWN))
                .map(|(end, _)| end);
            // A string read without its length may be cut where the read stopped.
            let read_whole = command != "GETRANGE" || bytes(reply).is_some_and(|b| b.len() < SHOWN);
            let shown_as = match (cut, read_whole, command) {
                (None, true, "GETRANGE") => tag(reply),
                (None, true, _) => edit::JSON,
                _ => edit::TRUNCATED,
            };
            let text = text.map(|text| match cut {
                Some(end) => format!("{}…", &text[..end]),
                None => text,
            });
            (text, shown_as)
        }
    }
}

/// How a reply's text stands for its bytes, as [`QueryResult::cell_types`]
/// tags it on a grid that can be written to.
fn tag(value: &Value) -> &'static str {
    match value {
        Value::BulkString(bytes) if std::str::from_utf8(bytes).is_err() => edit::BYTES,
        Value::BulkString(_) | Value::SimpleString(_) | Value::VerbatimString { .. } => {
            edit::STRING
        }
        Value::Int(_) => edit::INTEGER,
        Value::Double(_) => edit::DOUBLE,
        _ => edit::SUMMARY,
    }
}

fn tagged(mut result: QueryResult, mut tags: Vec<Vec<&'static str>>) -> QueryResult {
    tags.truncate(result.rows.len());
    result.cell_types = tags;
    result
}

/// Where the rows of `command`'s result are written back to: the keys grid's
/// keys, or one key's fields, items, members or value, read whole and tagged
/// cell by cell. `None` for any other result, a reply queued inside a `MULTI`
/// among them, and for a list read from its end, whose indexes are unknown.
fn edit_target(command: &Command, result: &QueryResult, database: &str) -> Option<EditTarget> {
    if result.cell_types.len() != result.rows.len() || result.rows.is_empty() {
        return None;
    }
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    let key = || command.words.get(1).map(|key| redis_cmd::spelled(key));
    let (schema, table, columns, keys): (&str, String, &[&str], &[usize]) =
        match (command.name().as_str(), names.as_slice()) {
            ("SCAN", ["key", "type", "ttl", "value"]) => (
                database,
                redis_cmd::browse::KEYS.into(),
                &["key", "", "ttl", "value"],
                &[0],
            ),
            ("HSCAN", ["field", "value"]) => (edit::HASH, key()?, &["field", "value"], &[0]),
            // The value too, since the index alone names whatever sits there.
            ("LRANGE", ["index", "value"]) => (edit::LIST, key()?, &["index", "value"], &[0, 1]),
            ("SSCAN", ["member"]) => (edit::SET, key()?, &["member"], &[0]),
            ("ZSCAN", ["member", "score"]) => (edit::ZSET, key()?, &["member", "score"], &[0, 1]),
            ("GET", ["value"]) => (edit::STRING, key()?, &["value"], &[0]),
            _ => return None,
        };
    Some(EditTarget {
        schema: schema.into(),
        table,
        columns: columns
            .iter()
            .map(|name| Some(name.to_string()).filter(|name| !name.is_empty()))
            .collect(),
        keys: keys.to_vec(),
    })
}

/// A generated write's reply that says it changed nothing, as the error it
/// is: the name a rename or a new row wanted was taken, the key an expiry or
/// a new value was for is gone, as is the member a new score was for, the
/// list item a write was for is no longer the one read, a watched key changed
/// before the `EXEC`, or a command inside the transaction failed. A delete of what is already gone
/// is not one: the key, field or member is gone, as was asked.
fn unapplied(command: &Command, reply: &Value) -> Result<(), DbError> {
    let word = |at: usize| {
        command
            .words
            .get(at)
            .map(|word| shown(word))
            .unwrap_or_default()
    };
    let flag = |at: usize, flag: &[u8]| command.words.get(at).is_some_and(|word| word == flag);
    let nx = |at: usize| flag(at, b"NX");
    let message = match (command.name().as_str(), reply) {
        ("RENAMENX", Value::Int(0)) => format!(
            "{} was not renamed: a key named {} already exists.",
            word(1),
            word(2)
        ),
        ("SET", Value::Nil) if nx(3) => {
            format!("A key named {} already exists, so it was not set.", word(1))
        }
        ("SET", Value::Nil) if flag(3, b"XX") => {
            format!("{} no longer exists, so its value was not set.", word(1))
        }
        ("HSETNX", Value::Int(0)) => format!(
            "{} already has a field {}, so it was not set.",
            word(1),
            word(2)
        ),
        ("SADD", Value::Int(0)) => format!("{} is already a member of {}.", word(2), word(1)),
        ("ZADD", Value::Int(0)) if nx(2) => format!(
            "{} is already a member of {}, so its score was left as it was.",
            word(4),
            word(1)
        ),
        ("ZADD", Value::Int(0)) if flag(2, b"XX") => format!(
            "{} is no longer a member of {}, or already had that score, so nothing changed.",
            word(5),
            word(1)
        ),
        ("EXPIRE", Value::Int(0)) => {
            format!("{} no longer exists, so it has no TTL to set.", word(1))
        }
        ("LPOS", Value::Array(found)) if flag(5, b"MAXLEN") => {
            let last = std::str::from_utf8(command.words.get(6).map_or(&[][..], Vec::as_slice))
                .ok()
                .and_then(|length| length.parse::<i64>().ok())
                .map(|length| Value::Int(length - 1));
            match found.last() == last.as_ref() {
                true => return Ok(()),
                false => format!(
                    "{} changed since it was read, so nothing was written: re-run to refresh.",
                    word(1)
                ),
            }
        }
        ("EXEC", Value::Nil) => {
            "A key changed while the edit was being written, so none of it was: re-run to \
             refresh."
                .into()
        }
        ("EXEC", Value::Array(replies)) => match replies.iter().find_map(|reply| match reply {
            Value::ServerError(error) => Some(server_message(error)),
            _ => None,
        }) {
            Some(error) => {
                format!("A command in the transaction failed, and the rest of it ran: {error}")
            }
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    Err(plain_error(message))
}

/// Whether a collection edit's read still finds the collection as the keys
/// grid showed it, `expected` being that JSON, which the edit was diffed
/// against: else it would write over a change made since. The edit's `WATCH`
/// came first, so a change after this read voids its `EXEC` instead. A
/// cursor that is not `0` means the read was not all of it, which no snapshot
/// the grid edits is.
fn verified(command: &Command, reply: &Value, expected: &[u8]) -> Result<(), DbError> {
    let key = command
        .words
        .get(1)
        .map(|word| shown(word))
        .unwrap_or_default();
    let changed = || {
        plain_error(format!(
            "{key} changed since it was read, so nothing was written: re-run to refresh."
        ))
    };
    let (kind, items) = match (command.name().as_str(), reply) {
        (_, Value::ServerError(error)) => return Err(plain_error(server_message(error))),
        ("LRANGE", Value::Array(items)) => (edit::LIST, items.clone()),
        (name, reply) => {
            let kind = match name {
                "HSCAN" => edit::HASH,
                "SSCAN" => edit::SET,
                "ZSCAN" => edit::ZSET,
                _ => return Err(changed()),
            };
            match scanned(reply) {
                Some((cursor, items)) if cursor == b"0" => (kind, items),
                Some(_) => {
                    return Err(plain_error(format!(
                        "{key} is more than one read holds, so nothing was written: open the \
                         key to edit it."
                    )));
                }
                None => return Err(changed()),
            }
        }
    };
    let shown = std::str::from_utf8(expected)
        .ok()
        .and_then(|text| edit::Collection::parse(kind, text).ok());
    match collection(kind, &items) {
        Some(found) if Some(&found) == shown.as_ref() => Ok(()),
        _ => Err(changed()),
    }
}

/// A collection's read as the entries it holds; `None` where a byte of it is
/// not text or a score not a number, which no JSON the grid showed holds.
fn collection(kind: &str, items: &[Value]) -> Option<edit::Collection> {
    use edit::Collection;
    let flat: Vec<&Value> = items
        .iter()
        .flat_map(|item| match item {
            Value::Array(pair) => pair.iter().collect(),
            item => vec![item],
        })
        .collect();
    let pairs = || {
        flat.chunks(2)
            .map(|pair| match pair {
                [first, second] => Some((*first, *second)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
    };
    Some(match kind {
        edit::LIST => Collection::List(items.iter().map(utf8_text).collect::<Option<_>>()?),
        edit::SET => Collection::Set(items.iter().map(utf8_text).collect::<Option<_>>()?),
        edit::HASH => Collection::Hash(
            pairs()?
                .into_iter()
                .map(|(field, value)| Some((utf8_text(field)?, utf8_text(value)?)))
                .collect::<Option<_>>()?,
        ),
        _ => Collection::Zset(
            pairs()?
                .into_iter()
                .map(|(member, score)| Some((utf8_text(member)?, scored(score)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// Whether a `*SCAN` of a key (`HSCAN key cursor …`) carries `flag` among its
/// options, which are looked at as `MATCH pattern` and `COUNT n` pairs so a
/// pattern that spells the flag is not taken for it.
fn scan_flag(words: &[Vec<u8>], flag: &[u8]) -> bool {
    let mut options = words.iter().skip(3);
    while let Some(option) = options.next() {
        if option.eq_ignore_ascii_case(flag) {
            return true;
        }
        if option.eq_ignore_ascii_case(b"MATCH") || option.eq_ignore_ascii_case(b"COUNT") {
            options.next();
        }
    }
    false
}

/// The command whose reply the keys grid shows as a key of `kind`'s value,
/// and for a collection the nouns its size is counted in. `None` for a type
/// with no read here, whose value is left NULL.
fn summary(kind: &str) -> Option<(&'static str, Option<[&'static str; 2]>)> {
    Some(match kind {
        "string" => ("GETRANGE", None),
        "ReJSON-RL" => ("JSON.GET", None),
        "hash" => ("HLEN", Some(["field", "fields"])),
        "list" => ("LLEN", Some(["item", "items"])),
        "set" => ("SCARD", Some(["member", "members"])),
        "zset" => ("ZCARD", Some(["member", "members"])),
        "stream" => ("XLEN", Some(["entry", "entries"])),
        _ => return None,
    })
}

/// The read of the first [`PREVIEWED`] entries of a collection counted by
/// `counted_by`, never more whatever its size. `None` for a type previewed by
/// its size alone.
fn sample(counted_by: &str, key: &[u8]) -> Option<::redis::Cmd> {
    let mut read = ::redis::cmd(match counted_by {
        "HLEN" => "HSCAN",
        "LLEN" => "LRANGE",
        "SCARD" => "SSCAN",
        "ZCARD" => "ZRANGE",
        _ => return None,
    });
    read.arg(key).arg(0);
    match counted_by {
        "HLEN" | "SCARD" => read.arg("COUNT").arg(PREVIEWED),
        "LLEN" => read.arg(PREVIEWED - 1),
        _ => read.arg(PREVIEWED - 1).arg("WITHSCORES"),
    };
    Some(read)
}

/// A [`sample`]'s entries, each as compact JSON (`"field":"value"`,
/// `"member":score`, `"item"`), the brackets they go between, and whether the
/// JSON reads back as the same entries: every byte text, every score finite.
/// A `*SCAN`'s `COUNT` is only a hint, and a small collection's one page is
/// all of it, so past [`PREVIEWED`] they are dropped here.
fn entries(counted_by: &str, sampled: &Value) -> Option<(Vec<String>, [char; 2], bool)> {
    let items = match (counted_by, sampled) {
        ("HLEN" | "SCARD", sampled) => scanned(sampled)?.1,
        (_, Value::Array(items)) => items.clone(),
        _ => return None,
    };
    let entries = match counted_by {
        "HLEN" | "ZCARD" => {
            // RESP3 nests a sorted set's member and score as a pair.
            let flat: Vec<&Value> = items
                .iter()
                .flat_map(|item| match item {
                    Value::Array(pair) => pair.iter().collect(),
                    item => vec![item],
                })
                .collect();
            if !flat.len().is_multiple_of(2) {
                return None;
            }
            let value = |value: &Value| match counted_by {
                "ZCARD" => score(value),
                _ => json(value).to_string(),
            };
            let pairs = flat.chunks(2).take(PREVIEWED);
            let text = pairs.clone().all(|pair| {
                utf8(pair[0])
                    && match counted_by {
                        "ZCARD" => scored(pair[1]).is_some_and(f64::is_finite),
                        _ => utf8(pair[1]),
                    }
            });
            let entries = pairs.map(|pair| format!("{}:{}", json(pair[0]), value(pair[1])));
            (entries.collect(), ['{', '}'], text)
        }
        _ => {
            let items = items.iter().take(PREVIEWED);
            (
                items.clone().map(|item| json(item).to_string()).collect(),
                ['[', ']'],
                items.clone().all(utf8),
            )
        }
    };
    Some(entries)
}

/// The text of a string reply that is UTF-8, which JSON carries as it is.
fn utf8_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        Value::SimpleString(text) => Some(text.clone()),
        _ => None,
    }
}

fn utf8(value: &Value) -> bool {
    utf8_text(value).is_some()
}

/// A score whichever protocol carried it.
fn scored(value: &Value) -> Option<f64> {
    match value {
        Value::Double(number) => Some(*number),
        value => text(value).and_then(|text| text.parse().ok()),
    }
}

/// A score as a JSON number, spelled as redis-cli spells it (`3`, not
/// `3.0`), whichever protocol carried it; `inf` and `-inf`, which JSON has no
/// number for, as text.
fn score(value: &Value) -> String {
    match scored(value).filter(|score| score.is_finite()) {
        Some(score) => edit::spelled_score(score),
        None => json(value).to_string(),
    }
}

/// A string's first [`SHOWN`] bytes, of `length` in all, marked `…` when that
/// is not all of it. A character the cut runs through is dropped rather than
/// turning the whole prefix into escaped bytes.
fn prefixed(prefix: &[u8], length: i64) -> String {
    if usize::try_from(length).is_ok_and(|length| length <= prefix.len()) {
        return shown(prefix);
    }
    let whole = match std::str::from_utf8(prefix) {
        Err(error) if error.error_len().is_none() => &prefix[..error.valid_up_to()],
        _ => prefix,
    };
    format!("{}…", shown(whole))
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
    let scores = (name.starts_with('Z') && has_option(command, b"WITHSCORES"))
        || matches!(
            name.as_str(),
            "ZPOPMIN" | "ZPOPMAX" | "BZPOPMIN" | "BZPOPMAX"
        );
    let named = |columns: &[&str], rows: Vec<Vec<Cell>>| -> Grid {
        (columns.iter().map(|name| name.to_string()).collect(), rows)
    };
    // Only `LRANGE` and `GET` are opened keys shaped here: one by `listed`,
    // the other by its own scalar.
    let (listed_tags, got_tags) = match (&reply, name.as_str()) {
        (Value::Array(items), "LRANGE") => (
            items
                .iter()
                .map(|item| vec![edit::INTEGER, tag(item)])
                .collect(),
            Vec::new(),
        ),
        (Value::BulkString(_), "GET") => (Vec::new(), vec![vec![tag(&reply)]]),
        _ => (Vec::new(), Vec::new()),
    };
    let shaped = match (&reply, name.as_str(), command.subcommand().as_str()) {
        (_, "INFO", _) => {
            text(&reply).map(|info| named(&["section", "key", "value"], info_rows(&info)))
        }
        (_, "CLIENT", "LIST" | "INFO") => text(&reply).map(|list| client_rows(&list)),
        (Value::Array(entries), "XRANGE" | "XREVRANGE", _) => stream_rows(entries),
        (Value::Array(items), "LRANGE", _) => listed(command, items),
        (Value::Array(items), _, _) if scores => {
            paired(items).map(|rows| named(&["member", "score"], rows))
        }
        (Value::Array(items), "HRANDFIELD", _) if has_option(command, b"WITHVALUES") => {
            paired(items).map(|rows| named(&["field", "value"], rows))
        }
        (Value::Array(items), "HGETALL", _) | (Value::Array(items), "CONFIG", "GET") => {
            paired(items).map(|rows| named(&["field", "value"], rows))
        }
        _ => None,
    };
    let mut result = match shaped {
        Some((columns, rows)) => tagged(table(&columns, rows, limit, None)?, listed_tags),
        None => tagged(generic(reply, limit)?, got_tags),
    };
    result.rows_affected = affected;
    Ok(result)
}

/// Whether `flag` is among the words that follow the key. A key can be named
/// like an option, so the name and the key are not looked at, nor the keys
/// that `numkeys` counts in `ZUNION`, `ZINTER` and `ZDIFF`.
fn has_option(command: &Command, flag: &[u8]) -> bool {
    let keys = match command.name().as_str() {
        "ZUNION" | "ZINTER" | "ZDIFF" => command
            .words
            .get(1)
            .and_then(|count| std::str::from_utf8(count).ok()?.parse::<usize>().ok())
            .map(|count| count + 1),
        _ => Some(1),
    };
    keys.is_some_and(|keys| {
        command
            .words
            .iter()
            .skip(keys + 1)
            .any(|word| word.eq_ignore_ascii_case(flag))
    })
}

fn table<C: AsRef<str>>(
    columns: &[C],
    rows: Vec<Vec<Cell>>,
    limit: Option<usize>,
    stopped: Option<Stopped>,
) -> Result<QueryResult, DbError> {
    let total = rows.len();
    let kept = limit.map_or(total, |limit| limit.min(total));
    let mut builder = RowBuilder::default();
    let mut bytes = 0;
    let rows: Vec<Row> = rows
        .into_iter()
        .take(kept)
        .map(|row| {
            for value in &row {
                bytes += value.as_ref().map_or(0, String::len);
                builder.push(value.as_deref());
            }
            builder.finish()
        })
        .collect::<Result<_, _>>()?;
    Ok(QueryResult {
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
    })
}

/// A reply read by its own shape: a map as fields and values, an array of
/// maps as one row each under the union of their keys, any other array one
/// row per element, and anything else one value.
fn generic(reply: Value, limit: Option<usize>) -> Result<QueryResult, DbError> {
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

/// `LRANGE`'s items under the index each holds in the list, which is what
/// names one to `LSET`. `None` from a negative start, which counts from an end
/// the reply does not say the position of.
fn listed(command: &Command, items: &[Value]) -> Option<Grid> {
    let start = std::str::from_utf8(command.words.get(2)?)
        .ok()?
        .parse::<u64>()
        .ok()?;
    let rows = (start..)
        .zip(items)
        .map(|(index, item)| vec![Some(index.to_string()), cell(item)])
        .collect();
    Some((vec!["index".into(), "value".into()], rows))
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

/// [`text`] of a reply the server did not refuse.
fn answer(value: &Value) -> Option<String> {
    match value {
        Value::ServerError(_) => None,
        value => text(value),
    }
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
    use crate::explorer::{count_sql, open_row_sql, preview_sql};
    use crate::filter::{FilterBar, Operator, derived_filter};
    use crate::redis_cmd::browse::KEYS;
    use crate::sql::is_generated_select;

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
            "rediss://h/0?sslmode=prefer",
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

    #[test]
    fn a_push_replies_the_lists_length_which_is_not_a_count_of_rows_changed() {
        for line in ["LPUSH l a b", "RPUSH l a", "LPUSHX l a", "RPUSHX l a"] {
            let result = rendered(&command(line), Value::Int(5), None).unwrap();
            assert_eq!(result.rows_affected, None, "{line}");
        }
        let deleted = rendered(&command("DEL a b"), Value::Int(2), None).unwrap();
        assert_eq!(deleted.rows_affected, Some(2));
    }

    #[test]
    fn a_key_named_like_an_option_is_not_the_option() {
        let members = Value::Array(vec![bulk("a"), bulk("b")]);
        let (columns, rows) = grid("SMEMBERS WITHSCORES", members.clone());
        assert_eq!(columns, ["value"]);
        assert_eq!(rows.len(), 2);
        let (columns, _) = grid("ZRANGE WITHSCORES 0 -1", members.clone());
        assert_eq!(columns, ["value"]);
        let (columns, _) = grid("ZUNION 1 WITHSCORES", members.clone());
        assert_eq!(columns, ["value"]);
        let (columns, _) = grid("ZUNION 1 WITHSCORES WITHSCORES", members.clone());
        assert_eq!(columns, ["member", "score"]);
        let (columns, _) = grid("ZRANGE z 0 -1 WITHSCORES", members);
        assert_eq!(columns, ["member", "score"]);
    }

    #[test]
    fn hrandfield_pairs_only_with_its_values() {
        let fields = Value::Array(vec![bulk("a"), bulk("b")]);
        let (columns, rows) = grid("HRANDFIELD h 2", fields.clone());
        assert_eq!(columns, ["value"]);
        assert_eq!(rows.len(), 2);
        let (columns, rows) = grid("HRANDFIELD h 1 WITHVALUES", fields);
        assert_eq!(columns, ["field", "value"]);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_opened_string_or_list_is_written_back_to_by_its_name_spelled() {
        let target = |line: &str, reply: Value| {
            let command = command(line);
            let result = rendered(&command, reply, None).unwrap();
            (edit_target(&command, &result, "db0"), result.cell_types)
        };
        let (edit, tags) = target(r#"GET "a key""#, bulk("v"));
        let edit = edit.unwrap();
        assert_eq!(
            (edit.schema.as_str(), edit.table.as_str()),
            ("string", "\"a key\"")
        );
        assert_eq!(tags, [[edit::STRING]]);
        let (edit, tags) = target(r#"GET "\xff""#, Value::BulkString(b"\xfe".to_vec()));
        assert_eq!(edit.unwrap().table, r#""\xff""#);
        assert_eq!(tags, [[edit::BYTES]]);
        // Nothing to write over, and a reply queued inside a session's MULTI.
        assert_eq!(target("GET k", Value::Nil).0, None);
        assert_eq!(
            target("GET k", Value::SimpleString("QUEUED".into())).0,
            None
        );

        let items = Value::Array(vec![bulk("a"), Value::BulkString(b"\xff".to_vec())]);
        let (edit, tags) = target("LRANGE l 3 4", items.clone());
        let edit = edit.unwrap();
        assert_eq!(
            (edit.schema.as_str(), edit.keys.as_slice()),
            ("list", &[0, 1][..])
        );
        assert_eq!(
            tags,
            [[edit::INTEGER, edit::STRING], [edit::INTEGER, edit::BYTES]]
        );
        // Read from the end, where no item's index is known.
        assert_eq!(target("LRANGE l -2 -1", items).0, None);
        assert_eq!(
            target("HGETALL h", Value::Map(vec![(bulk("f"), bulk("v"))])).0,
            None
        );
    }

    #[test]
    fn the_keys_grid_marks_a_value_it_does_not_hold_whole() {
        let long = "x".repeat(SHOWN);
        let shown = |command, counted, reply: Value, length: Option<Value>| {
            shown_value(command, counted, &reply, length.as_ref(), None)
        };
        assert_eq!(
            shown("GETRANGE", None, bulk("hi"), Some(Value::Int(2))),
            (Some("hi".into()), edit::STRING)
        );
        assert_eq!(
            shown(
                "GETRANGE",
                None,
                bulk(&long),
                Some(Value::Int(SHOWN as i64 + 1))
            )
            .1,
            edit::TRUNCATED
        );
        assert_eq!(
            shown(
                "GETRANGE",
                None,
                Value::BulkString(b"\xff".to_vec()),
                Some(Value::Int(1))
            )
            .1,
            edit::BYTES
        );
        // Without its length, a read that filled the range may have been cut.
        assert_eq!(
            shown("GETRANGE", None, bulk(&long), None).1,
            edit::TRUNCATED
        );
        assert_eq!(shown("GETRANGE", None, bulk("hi"), None).1, edit::STRING);
        assert_eq!(shown("JSON.GET", None, bulk("{}"), None).1, edit::JSON);
        let document = format!("{long}!");
        let (text, tag) = shown("JSON.GET", None, bulk(&document), None);
        assert_eq!(
            (text.unwrap().chars().last(), tag),
            (Some('…'), edit::TRUNCATED)
        );
        assert_eq!(
            shown("HLEN", Some(["field", "fields"]), Value::Int(3), None),
            (Some("3 fields".into()), edit::SUMMARY)
        );
    }

    #[test]
    fn a_collection_is_shown_whole_only_when_it_reads_at_a_glance() {
        let array = |items: &[Value]| Value::Array(items.to_vec());
        let scan = |items: &[Value]| array(&[bulk("0"), array(items)]);
        let preview = |counted_by, size: i64, sampled: Value| {
            let counted = summary(match counted_by {
                "HLEN" => "hash",
                "LLEN" => "list",
                "SCARD" => "set",
                _ => "zset",
            })
            .unwrap()
            .1;
            shown_value(counted_by, counted, &Value::Int(size), None, Some(&sampled))
        };
        let hash = scan(&[bulk("name"), bulk("Grace"), bulk("plan"), bulk("free")]);
        assert_eq!(
            preview("HLEN", 2, hash.clone()),
            (
                Some(r#"{"name":"Grace","plan":"free"}"#.into()),
                edit::HASH_JSON
            )
        );
        // A read short of the size is only the size.
        assert_eq!(
            preview("HLEN", 1000, hash),
            (Some("1000 fields".into()), edit::SUMMARY)
        );
        assert_eq!(
            preview("LLEN", 1, array(&[bulk("a")])),
            (Some(r#"["a"]"#.into()), edit::LIST_JSON)
        );
        assert_eq!(
            preview("SCARD", 3, scan(&[bulk("a")])),
            (Some("3 members".into()), edit::SUMMARY)
        );
        assert_eq!(preview("SCARD", 1, scan(&[bulk("a")])).1, edit::SET_JSON);
        // A sparse hashtable answers with a cursor that is not 0: shown, not editable.
        let sparse = |items: &[Value]| array(&[bulk("17"), array(items)]);
        assert_eq!(
            preview("SCARD", 1, sparse(&[bulk("a")])),
            (Some(r#"["a"]"#.into()), edit::SUMMARY)
        );
        assert_eq!(
            preview("HLEN", 1, sparse(&[bulk("f"), bulk("v")])).1,
            edit::SUMMARY
        );
        // Kept in the order read, which is by score, nested (RESP3) or flat.
        let nested = array(&[
            array(&[bulk("low"), Value::Double(1.0)]),
            array(&[bulk("high"), Value::Double(2.5)]),
        ]);
        let flat = array(&[bulk("low"), bulk("1"), bulk("high"), bulk("2.5")]);
        for sampled in [nested, flat] {
            assert_eq!(
                preview("ZCARD", 2, sampled),
                (Some(r#"{"low":1,"high":2.5}"#.into()), edit::ZSET_JSON)
            );
        }
        // An infinite score and bytes that are not UTF-8 show, quoted, but no
        // JSON edit could write them back.
        assert_eq!(
            preview("ZCARD", 1, array(&[bulk("a"), bulk("-inf")])),
            (Some(r#"{"a":"-inf"}"#.into()), edit::SUMMARY)
        );
        let binary = Value::BulkString(b"\xff".to_vec());
        assert_eq!(
            preview("LLEN", 1, array(std::slice::from_ref(&binary))),
            (Some(r#"["\"\\xff\""]"#.into()), edit::SUMMARY)
        );
        assert_eq!(
            preview("HLEN", 1, scan(&[bulk("f"), binary])).1,
            edit::SUMMARY
        );
        // 200 characters show; 201 are only the size.
        let item = |length: usize| array(&[bulk(&"x".repeat(length - 4))]);
        for length in [199, GLANCE] {
            let (shown, tag) = preview("LLEN", 1, item(length));
            assert_eq!(shown.unwrap().chars().count(), length);
            assert_eq!(tag, edit::LIST_JSON);
        }
        assert_eq!(
            preview("LLEN", 1, item(GLANCE + 1)),
            (Some("1 item".into()), edit::SUMMARY)
        );
        // A sample that failed leaves the size.
        assert_eq!(
            preview(
                "HLEN",
                3,
                ::redis::parse_redis_value(b"-NOPERM no\r\n").unwrap()
            )
            .0,
            Some("3 fields".into())
        );
        assert!(Engine::Redis.new_row_omits("type") && !Engine::Redis.new_row_omits("ttl"));
    }

    #[test]
    fn a_collection_edit_writes_only_over_the_collection_it_read() {
        let check = |line: &str, reply: Value, expected: &str| {
            verified(&command(line), &reply, expected.as_bytes())
        };
        let array = |items: &[Value]| Value::Array(items.to_vec());
        let scan = |cursor: &str, items: &[Value]| array(&[bulk(cursor), array(items)]);
        let hash = r#"{"a":"1","b":"2"}"#;
        let read = "HSCAN h 0 COUNT 100";
        let held = [bulk("b"), bulk("2"), bulk("a"), bulk("1")];
        assert!(check(read, scan("0", &held), hash).is_ok());
        // Changed, grown, gone, or more than one page: refused.
        for reply in [
            scan("0", &[bulk("a"), bulk("1"), bulk("b"), bulk("3")]),
            scan("0", &[held.as_slice(), &[bulk("c"), bulk("3")]].concat()),
            scan("0", &[]),
            scan("17", &held),
            ::redis::parse_redis_value(b"-WRONGTYPE no\r\n").unwrap(),
        ] {
            assert!(check(read, reply.clone(), hash).is_err(), "{reply:?}");
        }
        let list = r#"["a","b"]"#;
        let read = "LRANGE l 0 100";
        assert!(check(read, array(&[bulk("a"), bulk("b")]), list).is_ok());
        for items in [
            vec![bulk("b"), bulk("a")],
            vec![bulk("a"), bulk("b"), bulk("c")],
            vec![bulk("a"), Value::BulkString(b"\xff".to_vec())],
        ] {
            assert!(check(read, array(&items), list).is_err());
        }
        // A set and a sorted set compare as what they hold, in any order.
        let set = scan("0", &[bulk("y"), bulk("x")]);
        assert!(check("SSCAN s 0 COUNT 100", set, r#"["x","y"]"#).is_ok());
        let zset = scan("0", &[bulk("m"), bulk("2.5"), bulk("n"), bulk("1")]);
        assert!(check("ZSCAN z 0 COUNT 100", zset.clone(), r#"{"n":1,"m":2.5}"#).is_ok());
        assert!(check("ZSCAN z 0 COUNT 100", zset, r#"{"n":1,"m":2}"#).is_err());
    }

    #[test]
    fn a_generated_write_that_changed_nothing_is_an_error() {
        let refused = |line: &str, reply: Value| unapplied(&command(line), &reply).is_err();
        assert!(refused("RENAMENX a b", Value::Int(0)));
        assert!(!refused("RENAMENX a b", Value::Int(1)));
        assert!(refused("SET k v NX", Value::Nil));
        assert!(refused("SET k v NX EX 5", Value::Nil));
        assert!(!refused("SET k v NX", Value::Okay));
        assert!(refused("SET k v XX KEEPTTL GET", Value::Nil));
        assert!(!refused("SET k v XX KEEPTTL GET", bulk("old")));
        assert!(refused("HSETNX h f v", Value::Int(0)));
        assert!(refused("SADD s m", Value::Int(0)));
        assert!(refused("ZADD z NX 1 m", Value::Int(0)));
        assert!(refused("ZADD z XX CH 1 m", Value::Int(0)));
        assert!(!refused("ZADD z XX CH 1 m", Value::Int(1)));
        assert!(refused("EXPIRE k 5", Value::Int(0)));
        assert!(!refused("SADD s m", Value::SimpleString("QUEUED".into())));
        let failed = ::redis::parse_redis_value(b"-ERR index out of range\r\n").unwrap();
        assert!(refused("EXEC", Value::Array(vec![failed, Value::Int(1)])));
        assert!(refused("EXEC", Value::Nil));
        let lpos = "LPOS l v COUNT 0 MAXLEN 3";
        let at = |found: &[i64]| Value::Array(found.iter().copied().map(Value::Int).collect());
        assert!(!refused(lpos, at(&[0, 2])));
        assert!(refused(lpos, at(&[0, 1])));
        assert!(refused(lpos, at(&[])));
        assert!(!refused(
            "EXEC",
            Value::Array(vec![Value::Okay, Value::Int(1)])
        ));
    }

    #[test]
    fn a_scan_without_values_or_scores_lists_only_what_it_names() {
        let columns = |line: &str| Scanned::of(&command(line)).columns().to_vec();
        assert_eq!(columns("HSCAN h 0"), ["field", "value"]);
        assert_eq!(columns("HSCAN h 0 NOVALUES"), ["field"]);
        assert_eq!(
            columns("HSCAN h 0 MATCH NOVALUES COUNT 5"),
            ["field", "value"]
        );
        assert_eq!(columns("ZSCAN z 0"), ["member", "score"]);
        assert_eq!(columns("ZSCAN z 0 COUNT 5 NOSCORES"), ["member"]);
    }

    #[test]
    fn hello_is_refused_by_an_old_server_a_protocol_it_lacks_or_a_proxy() {
        let refusal = |reply: &str| {
            let Value::ServerError(error) =
                ::redis::parse_redis_value(format!("-{reply}\r\n").as_bytes()).unwrap()
            else {
                panic!("{reply}");
            };
            refused_hello(&RedisError::from(error))
        };
        assert!(refusal(
            "ERR unknown command `HELLO`, with args beginning with:"
        ));
        assert!(refusal(
            "NOPROTO sorry, this protocol version is not supported"
        ));
        assert!(refusal(
            "ERR Unknown subcommand or wrong number of arguments"
        ));
        assert!(!refusal("WRONGPASS invalid username-password pair"));
        assert!(!refusal("NOPERM no permissions"));
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
            cells(&ran(&connection, "LRANGE queue:emails 1 -1")),
            [some(&["1", "reminder"]), some(&["2", "receipt"])]
        );
        // From the end, where no index is known.
        assert_eq!(
            cells(&ran(&connection, "LRANGE queue:emails -1 -1")),
            [some(&["receipt"])]
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
        assert_eq!(names, ["key", "type", "ttl", "value"]);
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

    fn preview(connection: &Connection, bars: &[FilterBar], limit: Option<usize>) -> QueryResult {
        let filter = derived_filter(Engine::Redis, bars, &[]);
        let text = preview_sql(Engine::Redis, "db0", KEYS, &filter, limit, 0);
        assert!(is_generated_select(Engine::Redis, &text), "{text}");
        connection
            .query(
                &text,
                Fetch {
                    limit,
                    ..Fetch::default()
                },
            )
            .unwrap_or_else(|error| panic!("{text}: {}", error.message))
    }

    fn bar(column: &str, operator: Operator, value: &str) -> FilterBar {
        FilterBar {
            column: Some(column.into()),
            operator,
            value: value.into(),
            ..FilterBar::default()
        }
    }

    fn keys(result: &QueryResult) -> Vec<String> {
        let mut keys: Vec<String> = cells(result)
            .into_iter()
            .filter_map(|row| row[0].clone())
            .collect();
        keys.sort();
        keys
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_catalog_is_one_keyspace_counted_by_dbsize() {
        let connection = live();
        let catalog = connection.catalog().unwrap();
        let [schema] = catalog.schemas.as_slice() else {
            panic!("one schema: {catalog:?}");
        };
        assert_eq!(schema.name, "db0");
        assert_eq!(schema.relations.len(), 1);
        assert_eq!(schema.relations[0].name, KEYS);
        assert_eq!(schema.relations[0].rows, Some(214));
        let structure = connection.structure("db0", KEYS).unwrap();
        let names: Vec<&str> = structure.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["key", "type", "ttl", "value"]);
        assert_eq!(structure.row_key(), ["key"]);

        let count = count_sql(Engine::Redis, "db0", KEYS, "");
        assert!(is_generated_select(Engine::Redis, &count));
        assert_eq!(cells(&ran(&connection, &count)), [some(&["214"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_shows_each_keys_value_or_its_size() {
        let connection = live();
        let all = preview(&connection, &[], Some(1_000));
        assert_eq!(all.rows.len(), 214);
        assert_eq!(all.stopped, None);
        let row = |key: &str| {
            cells(&all)
                .into_iter()
                .find(|row| row[0].as_deref() == Some(key))
                .unwrap_or_else(|| panic!("{key} is listed"))
        };
        assert_eq!(
            row("greeting"),
            [
                Some("greeting".into()),
                Some("string".into()),
                None,
                Some("hello world".into())
            ]
        );
        assert_eq!(
            row("user:1")[3].as_deref(),
            Some(r#"{"name":"Ada Lovelace","email":"ada@example.test","plan":"pro"}"#)
        );
        assert_eq!(
            row("queue:emails")[3].as_deref(),
            Some(r#"["welcome","reminder","receipt"]"#)
        );
        assert_eq!(
            row("tags:redis")[3].as_deref(),
            Some(r#"["fast","in memory","key-value"]"#)
        );
        assert_eq!(
            row("leaderboard")[3].as_deref(),
            Some(r#"{"linus":72,"grace":85.5,"ada":100}"#)
        );
        assert_eq!(row("events:signup")[3].as_deref(), Some("3 entries"));
        assert_eq!(row("doc:1")[1].as_deref(), Some("ReJSON-RL"));
        assert!(
            row("doc:1")[3]
                .as_deref()
                .is_some_and(|json| json.contains("\"Ada\""))
        );
        assert_eq!(
            row("bin:blob")[3].as_deref(),
            Some(r#""\x00\xff\xfebinary\x01""#)
        );
        assert!(row("session:abc")[2].is_some());

        // The tab's row limit is the fetch's: SCAN has none to write.
        let capped = preview(&connection, &[], Some(10));
        assert_eq!(capped.rows.len(), 10);
        // One page held them all, so it says how many there were.
        assert_eq!(capped.capped_from, Some(214));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_reads_only_the_keys_it_keeps() {
        let connection = live();
        // This session's own count, which no other test's commands move.
        let commands = |connection: &Connection| -> u64 {
            let info = ran(connection, "CLIENT INFO");
            let at = info
                .columns
                .iter()
                .position(|column| column.name == "tot-cmds")
                .expect("CLIENT INFO says tot-cmds");
            info.rows[0].get(at).unwrap().parse().unwrap()
        };
        let before = commands(&connection);
        let capped = preview(&connection, &[], Some(10));
        assert_eq!(capped.capped_from, Some(214));
        // The SCAN, a TYPE and a TTL for each of the 10 and at most two value
        // reads each, and the second CLIENT INFO; not the 214 keys' worth.
        let sent = commands(&connection) - before;
        assert!(sent <= 1 + 10 * 4 + 1, "{sent} commands");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_shows_the_start_of_a_long_value_marked_as_cut() {
        // A database of its own, so the seeded one keeps its size.
        let connection = Connection::open(&ServerConfig {
            database: "3".into(),
            ..live_config()
        })
        .unwrap();
        ran(&connection, "FLUSHDB");
        // Two-byte characters after one byte: the cut lands mid-character.
        let long = format!("a{}", "é".repeat(SHOWN));
        ran(&connection, &format!("SET long {long}"));
        ran(&connection, "SET short é");
        let document = format!(r#"{{"s":"{}"}}"#, "x".repeat(SHOWN));
        ran(
            &connection,
            &format!("JSON.SET doc $ {}", redis_cmd::quoted(&document)),
        );
        let text = preview_sql(Engine::Redis, "db3", KEYS, "", None, 0);
        let all = ran(&connection, &text);
        let value = |key: &str| {
            cells(&all)
                .into_iter()
                .find(|row| row[0].as_deref() == Some(key))
                .and_then(|row| row[3].clone())
                .unwrap_or_else(|| panic!("{key} has a value"))
        };
        assert_eq!(value("long"), format!("a{}…", "é".repeat((SHOWN - 1) / 2)));
        assert_eq!(value("short"), "é");
        assert_eq!(value("doc").chars().count(), SHOWN + 1);
        assert!(value("doc").starts_with(r#"{"s":"xxx"#) && value("doc").ends_with("x…"));
        ran(&connection, "FLUSHDB");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_counts_a_big_collection_and_reads_no_more_of_it() {
        let connection = Connection::open(&ServerConfig {
            database: "5".into(),
            ..live_config()
        })
        .unwrap();
        let prefix = "live:preview:";
        clean(&connection, prefix);
        ran(
            &connection,
            "EVAL \"for i = 1, 1000 do \
             redis.call('HSET', KEYS[1], 'field:' .. i, 'value ' .. i) \
             redis.call('RPUSH', KEYS[2], 'item ' .. i) \
             redis.call('SADD', KEYS[3], 'member:' .. i) \
             redis.call('ZADD', KEYS[4], 1001 - i, 'member:' .. i) end\" 4 \
             live:preview:hash live:preview:list live:preview:set live:preview:zset",
        );
        ran(
            &connection,
            "HSET live:preview:small name \"Grace Hopper\" email grace@example.test plan free",
        );
        let net_out = |connection: &Connection| -> u64 {
            let info = ran(connection, "CLIENT INFO");
            let at = info
                .columns
                .iter()
                .position(|column| column.name == "tot-net-out")
                .expect("CLIENT INFO says tot-net-out");
            info.rows[0].get(at).unwrap().parse().unwrap()
        };
        let before = net_out(&connection);
        let text = preview_sql(Engine::Redis, "db5", KEYS, "MATCH live:preview:*", None, 0);
        let all = ran(&connection, &text);
        // Each 1000-entry key read whole would send over 10,000 bytes; its
        // first 100 entries are about a tenth of that.
        let sent = net_out(&connection) - before;
        let value = |key: &str| {
            cells(&all)
                .into_iter()
                .find(|row| row[0].as_deref() == Some(key))
                .and_then(|row| row[3].clone())
                .unwrap_or_else(|| panic!("{key} has a value"))
        };
        assert_eq!(value("live:preview:hash"), "1000 fields");
        assert_eq!(value("live:preview:list"), "1000 items");
        assert_eq!(value("live:preview:set"), "1000 members");
        assert_eq!(value("live:preview:zset"), "1000 members");
        assert_eq!(
            value("live:preview:small"),
            r#"{"name":"Grace Hopper","email":"grace@example.test","plan":"free"}"#
        );
        assert!(sent < 4 * 4_000, "{sent} bytes sent");
        clean(&connection, prefix);
    }

    /// The keys grid as the workspace builds it from the preview, asked
    /// which value cells take an edit: the connection's tags alone do not say
    /// whether the grid lets one through.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_edits_a_value_shown_whole_and_no_other() {
        use crate::result_grid::ResultGrid;
        use crate::sql::Mode;
        let connection = Connection::open(&ServerConfig {
            database: "5".into(),
            ..live_config()
        })
        .unwrap();
        let prefix = "live:editable:";
        clean(&connection, prefix);
        for seed in [
            "HSET live:editable:hash name Ada plan pro",
            "HSET live:editable:hash1 name Ada",
            "RPUSH live:editable:list a b",
            "RPUSH live:editable:list1 a",
            "SADD live:editable:set a b",
            "SADD live:editable:set1 a",
            "ZADD live:editable:zset 1 a 2 b",
            "ZADD live:editable:zset1 1 a",
            "SET live:editable:string hello",
            r#"JSON.SET live:editable:doc $ "{\"a\":1}""#,
        ] {
            ran(&connection, seed);
        }
        ran(
            &connection,
            &format!("SET live:editable:long {}", "x".repeat(SHOWN + 1)),
        );
        ran(
            &connection,
            &format!("HSET live:editable:wide f {}", "v".repeat(GLANCE)),
        );
        let text = preview_sql(Engine::Redis, "db5", KEYS, "MATCH live:editable:*", None, 0);
        let grid =
            ResultGrid::new(ran(&connection, &text), Mode::ReadWrite).with_engine(Engine::Redis);
        let editable = |key: &str| {
            let row = (0..grid.result().rows.len())
                .find(|&row| grid.cell(row, 0) == Some(key))
                .unwrap_or_else(|| panic!("{key} is listed"));
            (grid.cell(row, 3).map(str::to_string), grid.editable(row, 3))
        };
        for key in ["hash", "list", "set", "zset", "string"] {
            for key in [format!("{prefix}{key}"), format!("{prefix}{key}1")] {
                if key.ends_with("string1") {
                    continue;
                }
                let (value, editable) = editable(&key);
                assert!(editable, "{key}: {value:?}");
            }
        }
        assert_eq!(
            editable("live:editable:hash1"),
            (Some(r#"{"name":"Ada"}"#.into()), true)
        );
        for key in ["doc", "long", "wide"] {
            let key = format!("{prefix}{key}");
            let (value, editable) = editable(&key);
            assert!(!editable, "{key}: {value:?}");
        }
        assert_eq!(editable("live:editable:wide").0.as_deref(), Some("1 field"));
        clean(&connection, prefix);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_filter_bar_narrows_by_glob_and_type() {
        let connection = live();
        let raw = FilterBar {
            raw: true,
            value: "user:*".into(),
            ..FilterBar::default()
        };
        assert_eq!(
            keys(&preview(&connection, &[raw], None)),
            ["user:1", "user:2", "user:3"]
        );
        assert_eq!(
            keys(&preview(
                &connection,
                &[bar("type", Operator::Equals, "hash")],
                None
            )),
            ["cache:page", "user:1", "user:2", "user:3"]
        );
        assert_eq!(
            keys(&preview(
                &connection,
                &[
                    bar("key", Operator::StartsWith, "c"),
                    bar("type", Operator::Equals, "string"),
                ],
                None
            )),
            ["counter"]
        );
        assert_eq!(
            keys(&preview(
                &connection,
                &[bar("key", Operator::EndsWith, ":signup")],
                None
            )),
            ["events:signup"]
        );
        assert_eq!(
            keys(&preview(
                &connection,
                &[bar("key", Operator::Contains, "with spaces")],
                None
            )),
            ["key with spaces and \"quotes\""]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_keys_glob_metacharacters_match_only_themselves() {
        // A database of its own, so the seeded one keeps its size.
        let connection = Connection::open(&ServerConfig {
            database: "6".into(),
            ..live_config()
        })
        .unwrap();
        let odd = r"odd*key?[1]\x";
        let set = |key: &str| format!("SET {} 1", redis_cmd::quoted(key));
        ran(&connection, "FLUSHDB");
        ran(&connection, &set(odd));
        // What the same text matches read as a glob.
        ran(&connection, &set("oddZZkeyQ1x"));
        let preview = |bars: &[FilterBar]| {
            let filter = derived_filter(Engine::Redis, bars, &[]);
            let text = preview_sql(Engine::Redis, "db6", KEYS, &filter, None, 0);
            assert!(is_generated_select(Engine::Redis, &text), "{text}");
            keys(&ran(&connection, &text))
        };
        let raw = FilterBar {
            raw: true,
            value: odd.into(),
            ..FilterBar::default()
        };
        assert_eq!(preview(&[raw]), ["oddZZkeyQ1x"]);
        assert_eq!(preview(&[bar("key", Operator::Equals, odd)]), [odd]);
        assert_eq!(preview(&[bar("key", Operator::Contains, "*key?[")]), [odd]);
        assert_eq!(preview(&[bar("key", Operator::EndsWith, r"]\x")]), [odd]);
        ran(&connection, "FLUSHDB");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_key_opens_with_its_types_read() {
        let connection = live();
        for (key, kind, columns, rows) in [
            ("greeting", "string", &["value"][..], 1),
            ("user:1", "hash", &["field", "value"][..], 3),
            ("queue:emails", "list", &["index", "value"][..], 3),
            ("tags:redis", "set", &["member"][..], 3),
            ("leaderboard", "zset", &["member", "score"][..], 3),
            (
                "events:signup",
                "stream",
                &["id", "user", "plan", "referrer"][..],
                3,
            ),
            ("doc:1", "ReJSON-RL", &["value"][..], 1),
            (
                "key with spaces and \"quotes\"",
                "string",
                &["value"][..],
                1,
            ),
            ("bin:blob", "string", &["value"][..], 1),
        ] {
            let read = |limit| {
                open_row_sql(Engine::Redis, limit, |column| match column {
                    "key" => Some(key),
                    "type" => Some(kind),
                    _ => None,
                })
                .unwrap()
            };
            let result = ran(&connection, &read(100));
            let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, columns, "{}", read(100));
            assert_eq!(result.rows.len(), rows, "{}", read(100));
            assert!(cells(&result)[0][0].is_some(), "{}", read(100));
            // Under the limit a collection reads no further, whichever way
            // it stops: by what the command asks for or by the fetch's limit.
            if rows > 1 {
                let fetch = Fetch {
                    limit: Some(2),
                    ..Fetch::default()
                };
                let capped = connection.query(&read(2), fetch).unwrap();
                assert_eq!(capped.rows.len(), 2, "{}", read(2));
            }
        }
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

    fn with_user(admin: &Connection, name: &str, rules: &str) -> ServerConfig {
        ran(admin, &format!("ACL DELUSER {name}"));
        ran(admin, &format!("ACL SETUSER {name} {rules}"));
        ServerConfig {
            user: name.into(),
            password: "pw".into(),
            ..live_config()
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_user_with_no_password_logs_in_as_that_user() {
        let admin = live();
        let server = ServerConfig {
            password: String::new(),
            ..with_user(
                &admin,
                "live_nopass",
                "on nopass ~* +@read +acl|whoami +select",
            )
        };
        let whoami = Connection::open(&server)
            .and_then(|connection| connection.query("ACL WHOAMI", Fetch::default()));
        ran(&admin, "ACL DELUSER live_nopass");
        let whoami = whoami.expect("a nopass user should log in");
        assert_eq!(cells(&whoami), [some(&["live_nopass"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_scan_still_lists_keys_a_user_may_not_describe() {
        let admin = live();
        let server = with_user(&admin, "live_scanonly", "on >pw ~* +scan +select");
        let connection = Connection::open(&server);
        let keys = connection.and_then(|connection| {
            connection.query("SCAN 0 MATCH item:1* COUNT 1000", Fetch::default())
        });
        ran(&admin, "ACL DELUSER live_scanonly");
        let keys = cells(&keys.expect("the scan should succeed"));
        assert_eq!(keys.len(), 111);
        assert!(
            keys.iter()
                .all(|row| row[0].is_some() && row[1..].iter().all(Option::is_none))
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_run_that_fails_inside_a_multi_does_not_leave_it_open() {
        let connection = live();
        let error = connection
            .query("MULTI\nSET onlyone\nEXEC", Fetch::default())
            .unwrap_err();
        assert!(error.message.contains("discarded"), "{}", error.message);
        assert_eq!(cells(&ran(&connection, "GET counter")), [some(&["42"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_command_name_is_sent_as_typed() {
        let connection = live();
        let error = connection
            .query("frobnicate a", Fetch::default())
            .unwrap_err();
        assert!(error.message.contains("'frobnicate'"), "{}", error.message);
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
        let info = ran(&connection, "CLIENT INFO");
        let flags = info
            .columns
            .iter()
            .position(|column| column.name == "flags");
        let flags = info.rows[0].get(flags.unwrap()).unwrap();
        assert!(
            flags.contains('T'),
            "the redialled session touches keys: {flags}"
        );
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

    /// A database of the edit tests' own, so the seeded one keeps its keys.
    fn scratch(prefix: &str) -> Connection {
        let connection = Connection::open(&ServerConfig {
            database: "4".into(),
            ..live_config()
        })
        .unwrap();
        clean(&connection, prefix);
        connection
    }

    fn clean(connection: &Connection, prefix: &str) {
        let found = ran(connection, &format!("SCAN 0 MATCH {prefix}* COUNT 1000"));
        for row in cells(&found) {
            let key = row[0].clone().unwrap();
            ran(connection, &format!("UNLINK {}", redis_cmd::quoted(&key)));
        }
    }

    /// `text` run the way the grid runs what it writes: through the gate,
    /// then as a generated submission.
    fn applied(connection: &Connection, text: &str) -> Result<QueryResult, DbError> {
        assert!(
            crate::sql::is_generated_write_on(Engine::Redis, text),
            "{text}"
        );
        connection.generated(text, Fetch::default())
    }

    fn editing(connection: &Connection, read: &str) -> crate::result_grid::ResultGrid {
        let result = connection.generated(read, Fetch::default()).unwrap();
        assert!(result.edit.is_some(), "{read}");
        crate::result_grid::ResultGrid::new(result, crate::sql::Mode::ReadWrite)
            .with_engine(Engine::Redis)
    }

    fn row_of(grid: &crate::result_grid::ResultGrid, text: &str) -> usize {
        (0..grid.result().rows.len())
            .find(|&row| grid.cell(row, 0) == Some(text))
            .unwrap_or_else(|| panic!("{text} is listed"))
    }

    fn staged(text: &str) -> crate::result_grid::NewValue {
        crate::result_grid::NewValue::Value(text.to_string().into())
    }

    fn edits(grid: &crate::result_grid::ResultGrid) -> String {
        crate::sql::update_batch(Engine::Redis, &grid.pending_updates()).unwrap()
    }

    fn deletion(grid: &crate::result_grid::ResultGrid, row: usize) -> String {
        let (schema, table, keys) = grid.row_key(row).unwrap();
        let keys: Vec<(&str, &str)> = keys
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_str()))
            .collect();
        crate::sql::delete_row(Engine::Redis, &schema, &table, &keys, &grid.row_types(row)).unwrap()
    }

    fn insertion(schema: &str, table: &str, columns: &[(&str, Option<&str>)]) -> String {
        crate::sql::insert_row(Engine::Redis, schema, table, columns, &[]).unwrap()
    }

    fn one(connection: &Connection, text: &str) -> Cell {
        cells(&ran(connection, text))[0][0].clone()
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_the_keys_grid_writes_back_values_ttls_and_names() {
        let connection = scratch("live:keys:");
        ran(&connection, "SET live:keys:a hello");
        ran(&connection, "SET live:keys:b x");
        ran(
            &connection,
            &format!("SET live:keys:long {}", "x".repeat(SHOWN + 1)),
        );
        ran(&connection, r#"SET "live:keys:\xff" "\x00\xfe""#);
        let preview = preview_sql(Engine::Redis, "db4", KEYS, "MATCH live:keys:*", None, 0);
        let mut keys = editing(&connection, &preview);
        let a = row_of(&keys, "live:keys:a");
        let b = row_of(&keys, "live:keys:b");
        let long = row_of(&keys, "live:keys:long");
        let binary = row_of(&keys, r#""live:keys:\xff""#);
        assert!(!keys.editable(long, 3), "a cut value");
        assert!(!keys.editable(binary, 0) && !keys.editable(binary, 3));
        assert!(keys.set_pending(a, 3, staged("bonjour")));
        assert!(keys.set_pending(a, 2, staged("100")));
        assert!(keys.set_pending(b, 0, staged("live:keys:c")));
        assert!(keys.set_pending(binary, 2, staged("100")));
        applied(&connection, &edits(&keys)).unwrap();
        assert_eq!(
            one(&connection, "GET live:keys:a").as_deref(),
            Some("bonjour")
        );
        assert_eq!(one(&connection, "EXISTS live:keys:b").as_deref(), Some("0"));
        assert_eq!(one(&connection, "GET live:keys:c").as_deref(), Some("x"));
        for key in ["live:keys:a", r#""live:keys:\xff""#] {
            let ttl: i64 = one(&connection, &format!("TTL {key}"))
                .unwrap()
                .parse()
                .unwrap();
            assert!((1..=100).contains(&ttl), "{key}: {ttl}");
        }

        // The value is set again with the TTL kept, and a name that is taken
        // is refused rather than written over.
        let mut keys = editing(&connection, &preview);
        let a = row_of(&keys, "live:keys:a");
        assert!(keys.set_pending(a, 3, staged("hola")));
        assert!(keys.set_pending(a, 0, staged("live:keys:c")));
        let error = applied(&connection, &edits(&keys)).unwrap_err();
        assert!(
            error.message.contains("already exists"),
            "{}",
            error.message
        );
        assert_eq!(one(&connection, "GET live:keys:a").as_deref(), Some("hola"));
        assert_eq!(one(&connection, "GET live:keys:c").as_deref(), Some("x"));
        assert_ne!(one(&connection, "TTL live:keys:a").as_deref(), Some("-1"));

        let mut keys = editing(&connection, &preview);
        let a = row_of(&keys, "live:keys:a");
        assert!(keys.set_pending(a, 2, crate::result_grid::NewValue::Null));
        applied(&connection, &edits(&keys)).unwrap();
        assert_eq!(one(&connection, "TTL live:keys:a").as_deref(), Some("-1"));

        let new = insertion(
            "db4",
            KEYS,
            &[
                ("key", Some("live:keys:new")),
                ("value", Some("v")),
                ("ttl", Some("50")),
            ],
        );
        applied(&connection, &new).unwrap();
        assert_eq!(one(&connection, "GET live:keys:new").as_deref(), Some("v"));
        assert_ne!(one(&connection, "TTL live:keys:new").as_deref(), Some("-1"));
        let again = insertion(
            "db4",
            KEYS,
            &[("key", Some("live:keys:a")), ("value", Some("v"))],
        );
        assert!(applied(&connection, &again).is_err());
        assert_eq!(one(&connection, "GET live:keys:a").as_deref(), Some("hola"));

        let keys = editing(&connection, &preview);
        for key in ["live:keys:new", r#""live:keys:\xff""#] {
            let deleted = deletion(&keys, row_of(&keys, key));
            assert!(crate::sql::delete_matches_key_on(
                Engine::Redis,
                &deleted,
                &["key"]
            ));
            applied(&connection, &deleted).unwrap();
            assert_eq!(
                one(&connection, &format!("EXISTS {key}")).as_deref(),
                Some("0")
            );
        }
        clean(&connection, "live:keys:");
    }

    /// db 5, with keys of `prefix`'s own: db 4 is the other edit tests', and
    /// db 6 is flushed by the glob test.
    fn spare(prefix: &str) -> Connection {
        let connection = Connection::open(&ServerConfig {
            database: "5".into(),
            ..live_config()
        })
        .unwrap();
        clean(&connection, prefix);
        connection
    }

    fn keys_grid(connection: &Connection, prefix: &str) -> crate::result_grid::ResultGrid {
        let preview = preview_sql(
            Engine::Redis,
            "db5",
            KEYS,
            &format!("MATCH {prefix}*"),
            None,
            0,
        );
        editing(connection, &preview)
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_small_collections_json_is_edited_from_the_keys_grid() {
        let prefix = "live:json:";
        let connection = spare(prefix);
        ran(&connection, "HSET live:json:h a 1 b 2 c 3");
        ran(&connection, "RPUSH live:json:l a b c");
        ran(&connection, "SADD live:json:s x y");
        ran(&connection, "ZADD live:json:z 1 a 2.5 b");
        ran(&connection, r#"SADD live:json:bin "\xff" a"#);
        ran(
            &connection,
            "EVAL \"for i = 1, 300 do redis.call('RPUSH', KEYS[1], i) end\" 1 live:json:big",
        );
        let mut keys = keys_grid(&connection, prefix);
        let at = |keys: &crate::result_grid::ResultGrid, key| row_of(keys, key);
        assert_eq!(keys.cell(at(&keys, "live:json:big"), 3), Some("300 items"));
        for key in ["live:json:big", "live:json:bin"] {
            assert!(!keys.editable(at(&keys, key), 3), "{key}");
        }
        for (key, edited) in [
            ("live:json:h", r#"{"a":"1","b":"two","d":"4"}"#),
            ("live:json:l", r#"["a","B","c"]"#),
            ("live:json:s", r#"["y","z z"]"#),
            ("live:json:z", r#"{"a":1,"b":7,"c":-1}"#),
        ] {
            let row = at(&keys, key);
            assert!(keys.editable(row, 3), "{key}");
            assert!(keys.set_pending(row, 3, staged(edited)), "{key}");
        }
        applied(&connection, &edits(&keys)).unwrap();
        let keys = keys_grid(&connection, prefix);
        let shown = |key| keys.cell(at(&keys, key), 3).map(str::to_string);
        assert_eq!(
            cells(&ran(&connection, "HGETALL live:json:h")).len(),
            3,
            "c was deleted"
        );
        assert_eq!(
            one(&connection, "HGET live:json:h b").as_deref(),
            Some("two")
        );
        assert_eq!(one(&connection, "HGET live:json:h d").as_deref(), Some("4"));
        assert_eq!(shown("live:json:l").as_deref(), Some(r#"["a","B","c"]"#));
        assert_eq!(
            one(&connection, r#"SISMEMBER live:json:s "z z""#).as_deref(),
            Some("1")
        );
        assert_eq!(one(&connection, "SCARD live:json:s").as_deref(), Some("2"));
        assert_eq!(
            shown("live:json:z").as_deref(),
            Some(r#"{"c":-1,"a":1,"b":7}"#)
        );
        clean(&connection, prefix);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_collection_changed_since_it_was_read_is_not_written() {
        let prefix = "live:raced:";
        let connection = spare(prefix);
        let other = spare(prefix);
        ran(&connection, "HSET live:raced:h a 1 b 2");
        ran(&connection, "SADD live:raced:s x y");
        let mut keys = keys_grid(&connection, prefix);
        let h = row_of(&keys, "live:raced:h");
        assert!(keys.set_pending(h, 3, staged(r#"{"a":"one","b":"2"}"#)));
        let hash = edits(&keys);
        let mut keys = keys_grid(&connection, prefix);
        let s = row_of(&keys, "live:raced:s");
        assert!(keys.set_pending(s, 3, staged(r#"["x"]"#)));
        let set = edits(&keys);

        ran(&other, "HSET live:raced:h b 3");
        ran(&other, "SREM live:raced:s y");
        ran(&other, "SADD live:raced:s y");
        ran(&other, "SADD live:raced:s w");
        for text in [&hash, &set] {
            let error = applied(&connection, text).unwrap_err();
            assert!(error.message.contains("changed"), "{}", error.message);
        }
        assert_eq!(
            one(&connection, "HGET live:raced:h a").as_deref(),
            Some("1")
        );
        assert_eq!(
            one(&connection, "HGET live:raced:h b").as_deref(),
            Some("3")
        );
        assert_eq!(one(&connection, "SCARD live:raced:s").as_deref(), Some("3"));
        // Retyped and gone are refused too.
        ran(&other, "DEL live:raced:s");
        ran(&other, "SET live:raced:h now-a-string");
        for text in [&hash, &set] {
            assert!(applied(&connection, text).is_err(), "{text}");
        }
        assert_eq!(
            one(&connection, "EXISTS live:raced:s").as_deref(),
            Some("0")
        );
        // The failed runs left no WATCH behind to void the next edit.
        ran(&other, "DEL live:raced:h");
        ran(&other, "HSET live:raced:h a 1");
        let mut keys = keys_grid(&connection, prefix);
        let h = row_of(&keys, "live:raced:h");
        assert!(keys.set_pending(h, 3, staged(r#"{"a":"2"}"#)));
        applied(&connection, &edits(&keys)).unwrap();
        assert_eq!(
            one(&connection, "HGET live:raced:h a").as_deref(),
            Some("2")
        );
        clean(&connection, prefix);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_collection_grown_past_its_read_is_not_written() {
        let prefix = "live:grown:";
        let connection = spare(prefix);
        let other = spare(prefix);
        ran(&connection, "HSET live:grown:h a 1");
        ran(&connection, "RPUSH live:grown:l a");
        let mut keys = keys_grid(&connection, prefix);
        let h = row_of(&keys, "live:grown:h");
        assert!(keys.set_pending(h, 3, staged(r#"{"a":"2"}"#)));
        let l = row_of(&keys, "live:grown:l");
        assert!(keys.set_pending(l, 3, staged(r#"["b"]"#)));
        let both = edits(&keys);
        let (hash, list) = both.split_at(both.find("WATCH live:grown:l").unwrap());
        let hash = hash.trim_end();

        ran(
            &other,
            "EVAL \"for i = 1, 1000 do redis.call('HSET', KEYS[1], 'f' .. i, i) end\" 1 live:grown:h",
        );
        ran(&other, "RPUSH live:grown:l z");
        for text in [hash, list] {
            let error = applied(&connection, text).unwrap_err();
            assert!(
                error.message.contains("changed") || error.message.contains("one read"),
                "{}",
                error.message
            );
        }
        assert_eq!(
            one(&connection, "HGET live:grown:h a").as_deref(),
            Some("1")
        );
        assert_eq!(
            one(&connection, "LINDEX live:grown:l 0").as_deref(),
            Some("a")
        );
        clean(&connection, prefix);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_an_opened_key_writes_back_its_own_type() {
        let connection = scratch("live:open:");
        let read = |key: &str, kind: &str| redis_cmd::browse::read(key, kind, 100).unwrap();

        ran(&connection, "HSET live:open:h f 1 g 2");
        let mut hash = editing(&connection, &read("live:open:h", "hash"));
        let f = row_of(&hash, "f");
        assert!(!hash.editable(f, 0) && hash.editable(f, 1));
        assert!(hash.set_pending(f, 1, staged("one")));
        applied(&connection, &edits(&hash)).unwrap();
        assert_eq!(
            one(&connection, "HGET live:open:h f").as_deref(),
            Some("one")
        );
        // What its query tab's New row form is built from.
        let (schema, table, columns) = hash.insert_target().unwrap();
        assert_eq!(
            (schema.as_str(), table.as_str()),
            (edit::HASH, "live:open:h")
        );
        let names: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["field", "value"]);
        let field = |name| {
            insertion(
                edit::HASH,
                "live:open:h",
                &[("field", Some(name)), ("value", Some("new"))],
            )
        };
        applied(&connection, &field("n")).unwrap();
        assert!(applied(&connection, &field("g")).is_err());
        assert_eq!(one(&connection, "HGET live:open:h g").as_deref(), Some("2"));
        let hash = editing(&connection, &read("live:open:h", "hash"));
        applied(&connection, &deletion(&hash, row_of(&hash, "g"))).unwrap();
        assert_eq!(
            one(&connection, "HEXISTS live:open:h g").as_deref(),
            Some("0")
        );

        ran(&connection, "RPUSH live:open:l a b c");
        let mut list = editing(&connection, &read("live:open:l", "list"));
        assert!(list.set_pending(1, 1, staged("B")));
        applied(&connection, &edits(&list)).unwrap();
        let list = editing(&connection, &read("live:open:l", "list"));
        applied(&connection, &deletion(&list, 0)).unwrap();
        applied(
            &connection,
            &insertion(edit::LIST, "live:open:l", &[("value", Some("d"))]),
        )
        .unwrap();
        assert_eq!(
            cells(&ran(&connection, "LRANGE live:open:l 0 -1")),
            [some(&["0", "B"]), some(&["1", "c"]), some(&["2", "d"])]
        );

        ran(&connection, "SADD live:open:s a b");
        let mut set = editing(&connection, &read("live:open:s", "set"));
        assert!(set.set_pending(row_of(&set, "a"), 0, staged("z z")));
        applied(&connection, &edits(&set)).unwrap();
        assert_eq!(
            one(&connection, "SISMEMBER live:open:s a").as_deref(),
            Some("0")
        );
        assert_eq!(
            one(&connection, r#"SISMEMBER live:open:s "z z""#).as_deref(),
            Some("1")
        );
        let member = insertion(edit::SET, "live:open:s", &[("member", Some("b"))]);
        assert!(applied(&connection, &member).is_err());
        let set = editing(&connection, &read("live:open:s", "set"));
        applied(&connection, &deletion(&set, row_of(&set, "b"))).unwrap();
        assert_eq!(one(&connection, "SCARD live:open:s").as_deref(), Some("1"));

        ran(&connection, "ZADD live:open:z 1 a 2 b");
        let mut zset = editing(&connection, &read("live:open:z", "zset"));
        assert!(zset.set_pending(row_of(&zset, "a"), 1, staged("5.5")));
        assert!(zset.set_pending(row_of(&zset, "b"), 0, staged("c")));
        applied(&connection, &edits(&zset)).unwrap();
        assert_eq!(
            cells(&ran(&connection, "ZRANGE live:open:z 0 -1 WITHSCORES")),
            [some(&["c", "2"]), some(&["a", "5.5"])]
        );
        let zset = editing(&connection, &read("live:open:z", "zset"));
        applied(&connection, &deletion(&zset, row_of(&zset, "a"))).unwrap();
        let member = |score| {
            insertion(
                edit::ZSET,
                "live:open:z",
                &[("member", Some("c")), ("score", Some(score))],
            )
        };
        assert!(applied(&connection, &member("9")).is_err());
        assert_eq!(
            one(&connection, "ZSCORE live:open:z c").as_deref(),
            Some("2")
        );

        ran(&connection, "SET live:open:str v EX 100");
        let mut string = editing(&connection, &read("live:open:str", "string"));
        assert!(string.set_pending(0, 0, staged("w")));
        applied(&connection, &edits(&string)).unwrap();
        assert_eq!(one(&connection, "GET live:open:str").as_deref(), Some("w"));
        assert_ne!(one(&connection, "TTL live:open:str").as_deref(), Some("-1"));

        ran(&connection, r#"SET live:open:bin "\x00\xfe""#);
        let binary = editing(&connection, &read("live:open:bin", "string"));
        assert!(!binary.editable(0, 0));
        clean(&connection, "live:open:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_list_item_changed_since_it_was_read_is_not_written() {
        let connection = scratch("live:shift:");
        let other = scratch("live:shift:");
        let read = redis_cmd::browse::read("live:shift:l", "list", 100).unwrap();
        let listed = || cells(&ran(&connection, "LRANGE live:shift:l 0 -1"));
        ran(&connection, "RPUSH live:shift:l a b c");
        let list = editing(&connection, &read);
        let deleted = deletion(&list, 1);
        let mut list = editing(&connection, &read);
        assert!(list.set_pending(1, 1, staged("B")));
        let edited = edits(&list);

        ran(&other, "LPUSH live:shift:l z");
        let before = listed();
        for text in [&deleted, &edited] {
            let error = applied(&connection, text).unwrap_err();
            assert!(error.message.contains("changed"), "{}", error.message);
        }
        assert_eq!(listed(), before);

        // The failed runs left no WATCH behind to fail the next one.
        ran(&other, "LPUSH live:shift:l y");
        let list = editing(&connection, &read);
        applied(&connection, &deletion(&list, 0)).unwrap();
        let mut list = editing(&connection, &read);
        assert!(list.set_pending(2, 1, staged("B")));
        applied(&connection, &edits(&list)).unwrap();
        assert_eq!(
            listed(),
            [
                some(&["0", "z"]),
                some(&["1", "a"]),
                some(&["2", "B"]),
                some(&["3", "c"])
            ]
        );
        clean(&connection, "live:shift:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_value_edit_never_brings_back_a_deleted_key_or_replaces_another_type() {
        let connection = scratch("live:stale:");
        let other = scratch("live:stale:");
        ran(&connection, "SET live:stale:gone v");
        ran(&connection, "SET live:stale:hash v");
        ran(&connection, "SET live:stale:str v");
        let preview = preview_sql(Engine::Redis, "db4", KEYS, "MATCH live:stale:*", None, 0);
        let mut keys = editing(&connection, &preview);
        assert!(keys.set_pending(row_of(&keys, "live:stale:gone"), 3, staged("w")));
        let gone = edits(&keys);
        let mut keys = editing(&connection, &preview);
        assert!(keys.set_pending(row_of(&keys, "live:stale:hash"), 3, staged("w")));
        let replaced = edits(&keys);
        let read = redis_cmd::browse::read("live:stale:str", "string", 100).unwrap();
        let mut string = editing(&connection, &read);
        assert!(string.set_pending(0, 0, staged("w")));
        let opened = edits(&string);

        ran(&other, "UNLINK live:stale:gone live:stale:str");
        ran(&other, "UNLINK live:stale:hash");
        ran(&other, "HSET live:stale:hash f 1");
        for text in [&gone, &opened] {
            let error = applied(&connection, text).unwrap_err();
            assert!(
                error.message.contains("no longer exists"),
                "{}",
                error.message
            );
        }
        let error = applied(&connection, &replaced).unwrap_err();
        assert!(error.message.contains("WRONGTYPE"), "{}", error.message);
        assert_eq!(
            one(&connection, "EXISTS live:stale:gone live:stale:str").as_deref(),
            Some("0")
        );
        assert_eq!(
            one(&connection, "HGET live:stale:hash f").as_deref(),
            Some("1")
        );
        clean(&connection, "live:stale:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_renaming_a_member_onto_one_already_there_is_refused() {
        let connection = scratch("live:rename:");
        let read = |key: &str, kind: &str| redis_cmd::browse::read(key, kind, 100).unwrap();
        let refused = |text: &str| {
            let error = applied(&connection, text).unwrap_err();
            assert!(error.message.contains("already"), "{}", error.message);
        };

        ran(&connection, "SADD live:rename:s a b");
        let mut set = editing(&connection, &read("live:rename:s", "set"));
        assert!(set.set_pending(row_of(&set, "a"), 0, staged("b")));
        refused(&edits(&set));
        assert_eq!(
            one(&connection, "SCARD live:rename:s").as_deref(),
            Some("2")
        );

        ran(&connection, "ZADD live:rename:z 1 a 2 b");
        let mut zset = editing(&connection, &read("live:rename:z", "zset"));
        assert!(zset.set_pending(row_of(&zset, "a"), 0, staged("b")));
        refused(&edits(&zset));
        assert_eq!(
            cells(&ran(&connection, "ZRANGE live:rename:z 0 -1 WITHSCORES")),
            [some(&["a", "1"]), some(&["b", "2"])]
        );
        clean(&connection, "live:rename:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_an_edit_is_refused_while_a_query_tab_holds_the_session_in_a_multi() {
        let connection = scratch("live:multi:");
        ran(&connection, "HSET live:multi:h f 1");
        let edit = "HSET live:multi:h f 2";
        let refused = |connection: &Connection| {
            let error = applied(connection, edit).unwrap_err();
            assert!(
                error.message.contains("EXEC or DISCARD"),
                "{}",
                error.message
            );
        };
        ran(&connection, "MULTI");
        refused(&connection);
        ran(&connection, "DISCARD");
        assert_eq!(
            one(&connection, "HGET live:multi:h f").as_deref(),
            Some("1")
        );

        // Across runs, until the EXEC.
        ran(&connection, "MULTI\nHGET live:multi:h f");
        refused(&connection);
        ran(&connection, "EXEC");
        applied(&connection, edit).unwrap();
        assert_eq!(
            one(&connection, "HGET live:multi:h f").as_deref(),
            Some("2")
        );

        // A run that fails discards the MULTI it opened.
        assert!(
            connection
                .query("MULTI\nSET onlyone", Fetch::default())
                .is_err()
        );
        applied(&connection, "HSET live:multi:h f 3").unwrap();

        // A redial starts outside any MULTI.
        let id = one(&connection, "CLIENT ID").unwrap();
        ran(&connection, "MULTI");
        refused(&connection);
        ran(&live(), &format!("CLIENT KILL ID {id}"));
        assert!(connection.query("PING", Fetch::default()).is_err());
        applied(&connection, "HSET live:multi:h f 4").unwrap();
        assert_eq!(
            one(&connection, "HGET live:multi:h f").as_deref(),
            Some("4")
        );
        clean(&connection, "live:multi:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_cancel_that_skips_an_exec_closes_the_session_and_its_multi() {
        let connection = scratch("live:skipped:");
        ran(&connection, "MULTI");
        ran(&connection, "SET live:skipped:a 1");
        // A Cancel landing between a run's commands, as its next send sees it.
        lock(&connection.running).asked = true;
        let exec = redis_cmd::parse("EXEC").unwrap().remove(0);
        let error = connection
            .command(&mut lock(&connection.session), &exec, None, false, None)
            .unwrap_err();
        assert_eq!(error.message, CANCELLED);
        *lock(&connection.running) = Running::default();

        let edited = applied(&connection, "HSET live:skipped:h f 1").unwrap();
        assert_ne!(cells(&edited), [some(&["QUEUED"])]);
        assert_eq!(
            one(&connection, "EXISTS live:skipped:a live:skipped:h").as_deref(),
            Some("1")
        );
        clean(&connection, "live:skipped:");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_listing_databases_and_the_catalog_queue_nothing_into_a_users_multi() {
        let admin = live();
        let server = ServerConfig {
            database: "4".into(),
            ..with_user(&admin, "live_noconfig", "on >pw ~* +@all -config -dbsize")
        };
        let connection = Connection::open(&server).unwrap();
        ran(&connection, "MULTI");
        ran(&connection, "SET live:queued:a 1");
        let databases = connection.databases().unwrap();
        let catalog = connection.catalog().unwrap();
        let exec = connection.query("EXEC", Fetch::default());
        clean(&connection, "live:queued:");
        ran(&admin, "ACL DELUSER live_noconfig");
        assert_eq!(databases.names.len(), DEFAULT_DATABASES as usize);
        assert_eq!(catalog.schemas[0].relations[0].rows, None);
        assert_eq!(cells(&exec.unwrap()), [some(&["OK"])]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_REDIS_URL"]
    fn live_a_list_edit_is_refused_while_a_query_tab_holds_a_watch() {
        let connection = scratch("live:watched:");
        let other = scratch("live:watched:");
        ran(&connection, "RPUSH live:watched:l a");
        let read = redis_cmd::browse::read("live:watched:l", "list", 100).unwrap();
        let edit = |value: &str| {
            let mut list = editing(&connection, &read);
            assert!(list.set_pending(0, 1, staged(value)));
            edits(&list)
        };
        let refused = || {
            let error = applied(&connection, &edit("refused")).unwrap_err();
            assert!(error.message.contains("UNWATCH"), "{}", error.message);
        };

        ran(&connection, "WATCH live:watched:k");
        refused();
        // An edit with no WATCH of its own leaves the user's be.
        applied(&connection, "HSET live:watched:h f 1").unwrap();
        ran(&other, "SET live:watched:k 1");
        ran(&connection, "MULTI\nSET live:watched:x 1");
        assert_eq!(cells(&ran(&connection, "EXEC")), [vec![None]]);
        applied(&connection, &edit("b")).unwrap();

        // An EXEC outside a MULTI is an error that keeps the WATCH.
        ran(&connection, "WATCH live:watched:k");
        assert!(connection.query("EXEC", Fetch::default()).is_err());
        refused();
        ran(&connection, "UNWATCH");
        applied(&connection, &edit("c")).unwrap();

        // A redial starts with nothing watched.
        ran(&connection, "WATCH live:watched:k");
        let id = one(&connection, "CLIENT ID").unwrap();
        ran(&live(), &format!("CLIENT KILL ID {id}"));
        assert!(connection.query("PING", Fetch::default()).is_err());
        applied(&connection, &edit("d")).unwrap();
        assert_eq!(
            cells(&ran(&connection, "LRANGE live:watched:l 0 -1")),
            [some(&["0", "d"])]
        );
        assert_eq!(
            one(&connection, "EXISTS live:watched:x").as_deref(),
            Some("0")
        );
        clean(&connection, "live:watched:");
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

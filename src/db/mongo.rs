//! The MongoDB boundary.
//!
//! The driver is async on tokio, so each connection owns a runtime and blocks
//! on it from the background thread every call already runs on, as `mssql.rs`
//! does. Unlike that one the runtime is multi-threaded, with one worker: the
//! driver spawns tasks of its own -- a monitor per server, pool maintenance --
//! that have to keep running between calls, and a current-thread runtime only
//! drives them inside a `block_on`. Nothing tokio-shaped leaves this module.
//!
//! There is no connection mutex. The client is a pool, safe to share, so a
//! catalog load does not queue behind a slow statement the way it does on the
//! engines with one socket. Several statements can be in flight at once, so a
//! run goes out under the tab's [`CancelToken`], as on Snowflake, and Cancel
//! stops only that run's.
//!
//! A statement is never sent as typed: there is no JavaScript to send it to.
//! `mql` reads it into a method and literals, and each method is run as the
//! server command mongosh would send for it.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::TryStreamExt;
use mongodb::bson::oid::ObjectId;
use mongodb::bson::{Binary, Bson, DateTime, Document, Regex, Timestamp, Uuid, doc};
use mongodb::error::{Error, ErrorKind};
use mongodb::event::{EventHandler, sdam::SdamEvent};
use mongodb::options::{ClientOptions, ConnectionString, HostInfo, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, Database};
use percent_encoding::{AsciiSet, CONTROLS, NON_ALPHANUMERIC, utf8_percent_encode};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use super::ssh::{Tunnel, tunnelled};
use super::{
    CancelToken, Catalog, Cell, Column, ColumnDefinition, DbError, MISSING, NamedDefinition,
    QueryResult, Relation, RelationKind, Schema, ServerConfig, Sizes, SslMode, Statistics,
    Structure, plain_error,
};
use crate::mql::{self, Arg, Call, CursorMethod, DbMethod, Method, Show, Target, Value};

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 27017;

/// How long a connect waits for a server to answer, TLS and login included.
/// The driver's own default is thirty seconds of "Connecting…".
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// What ends or changes an option's value in a query string, escaped so a
/// database name written into the options reads back as itself.
const OPTION_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'=');

/// How many documents a structure is inferred from. A collection has no
/// declared columns, so its shape is whatever this many of them say.
const SAMPLE_SIZE: i32 = 1000;

/// The order a field's types are joined in. Null last, so a field that is
/// sometimes null reads as `string | null`.
const TYPE_ORDER: [&str; 21] = [
    "objectId",
    "string",
    "int",
    "long",
    "double",
    "decimal",
    "bool",
    "date",
    "object",
    "array",
    "binData",
    "regex",
    "javascript",
    "timestamp",
    "minKey",
    "maxKey",
    "javascriptWithScope",
    "symbol",
    "dbPointer",
    "undefined",
    "null",
];

/// What a MongoDB profile connects with: a server engine's fields plus what
/// a connection string carries that they do not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MongoConfig {
    /// `host` may be a comma-separated seed list (`h1:27017,h2:27018`), and
    /// `user` may be blank: an unauthenticated server is an ordinary one.
    pub server: ServerConfig,
    /// `mongodb+srv`: `server.host` is a DNS name whose SRV records list the
    /// servers.
    pub srv: bool,
    /// The connection string's options (`authSource=admin&replicaSet=rs0`),
    /// passed to the driver as typed. The keys the profile's own fields decide
    /// (`tls*`, and `directConnection` through a tunnel) are refused here
    /// rather than quietly overridden.
    pub options: String,
}

impl MongoConfig {
    pub fn endpoint(&self) -> String {
        match self.srv {
            true => self.server.host.clone(),
            false => self.server.endpoint(),
        }
    }

    fn seed_list(&self) -> bool {
        self.server.host.contains(',')
    }

    /// Move onto `database`, keeping the login where it was.
    ///
    /// With no `authSource` the driver authenticates against the database the
    /// connection string names, so a user defined in one database could not
    /// log in once moved to another. The source it had is written into the
    /// options, where the form shows it, before the database changes. Only for
    /// a password mechanism: X.509, AWS, Kerberos and LDAP authenticate against
    /// `$external` whatever the database.
    pub(super) fn set_database(&mut self, database: String) {
        let options = self.options.trim().trim_start_matches('?');
        let option = |name: &str| {
            options.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                key.eq_ignore_ascii_case(name).then_some(value)
            })
        };
        let password_login = option("authMechanism")
            .is_none_or(|mechanism| mechanism.to_ascii_uppercase().starts_with("SCRAM-"));
        if !self.server.user.is_empty() && password_login && option("authSource").is_none() {
            let source = match self.server.database.as_str() {
                "" => "admin".to_string(),
                current => utf8_percent_encode(current, OPTION_VALUE).to_string(),
            };
            self.options = match options {
                "" => format!("authSource={source}"),
                options => format!("{options}&authSource={source}"),
            };
        }
        self.server.database = database;
    }
}

/// A `mongodb://` or `mongodb+srv://` URL. The driver reads the address and
/// checks the options; what is left to do here is take the TLS keys out into
/// the profile's mode and keep the rest as typed.
pub fn config_from_url(url: &str) -> Result<MongoConfig, String> {
    let (address, query) = url.split_once('?').unwrap_or((url, ""));
    // A username holding an `@` (hard rule 5), typed rather than escaped. The
    // last `@` ends the credentials, which is where the driver splits too; it
    // only refuses the others.
    let address = match address.rsplit_once('@') {
        Some((credentials, rest)) => format!("{}@{rest}", credentials.replace('@', "%40")),
        None => address.to_string(),
    };

    let mut tls = None;
    let mut unchecked = false;
    let mut any_name = false;
    let mut root_certificate = None;
    let mut passed = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = super::percent_decoded(value)?;
        let flag = || match value.to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!(
                "Connection URL parameter {key}={value} is not true or false."
            )),
        };
        match key.to_ascii_lowercase().as_str() {
            "tls" | "ssl" => tls = Some(flag()?),
            "tlsinsecure" | "tlsallowinvalidcertificates" => unchecked |= flag()?,
            "tlsallowinvalidhostnames" => any_name |= flag()?,
            "tlscafile" => root_certificate = Some(value).filter(|path| !path.is_empty()),
            lowered if lowered.starts_with("tls") => {
                return Err(format!(
                    "Connection URL parameter {key} is not one dbdelve can pass to MongoDB."
                ));
            }
            _ => passed.push(pair),
        }
    }
    let options = passed.join("&");

    let parsed = ConnectionString::parse(match options.is_empty() {
        true => address.clone(),
        false => format!("{address}?{options}"),
    })
    .map_err(|error| format!("Connection URL is invalid: {}", error.kind))?;

    let (host, port, srv) = match parsed.host_info {
        HostInfo::DnsRecord(name) => (name, None, true),
        HostInfo::HostIdentifiers(hosts) => match hosts.as_slice() {
            [ServerAddress::Tcp { host, port }] => (host.clone(), *port, false),
            // As written: the driver unbrackets an IPv6 seed, and a list needs
            // the brackets back to read its ports.
            [_, _, ..] => (seeds_as_written(&address), None, false),
            _ => return Err("Connection URL does not name a host and port.".into()),
        },
        _ => return Err("Connection URL does not name a host.".into()),
    };

    let tls_keys = unchecked || any_name || root_certificate.is_some();
    let sslmode = match tls {
        Some(false) if tls_keys => {
            return Err("Connection URL sets tls=false beside options for TLS.".into());
        }
        Some(false) => SslMode::Disable,
        // A seed list's TLS is off unless asked for, and dbdelve's default
        // tries it anyway; an SRV name's is on, so its URL means verified.
        None if !srv && !tls_keys => SslMode::default(),
        Some(true) | None if unchecked => SslMode::Require,
        Some(true) | None if any_name => SslMode::VerifyCa,
        Some(true) | None => SslMode::VerifyFull,
    };
    let credential = parsed.credential.unwrap_or_default();

    Ok(MongoConfig {
        server: ServerConfig {
            host,
            port,
            database: parsed.default_database.unwrap_or_default(),
            user: credential.username.unwrap_or_default(),
            password: credential.password.unwrap_or_default(),
            sslmode,
            root_certificate: root_certificate.filter(|_| sslmode.checks_certificate()),
            // A URL has nowhere to say either; the form is where they are set.
            statement_timeout: 0,
            ssh: None,
        },
        srv,
        options,
    })
}

/// The hosts between the credentials and the path, exactly as the URL spelled
/// them.
fn seeds_as_written(address: &str) -> String {
    let after_scheme = address.split_once("://").map_or(address, |(_, rest)| rest);
    let after_credentials = after_scheme
        .rsplit_once('@')
        .map_or(after_scheme, |(_, rest)| rest);
    after_credentials
        .split_once('/')
        .map_or(after_credentials, |(hosts, _)| hosts)
        .to_string()
}

/// The connection string the driver is handed: the profile's fields
/// percent-encoded into it, and its options as typed, so the driver reads
/// `authSource` and the rest exactly as it would from a URL.
fn connection_string(config: &MongoConfig) -> Result<String, DbError> {
    let server = &config.server;
    let host = server.host.trim();
    if host.is_empty() || host.contains(['@', '/', '?', '#']) || host.contains(char::is_whitespace)
    {
        return Err(plain_error(format!(
            "Host {host} is not a host name or a list of them."
        )));
    }
    let hosts = match (config.srv || config.seed_list(), server.port) {
        (true, Some(_)) => {
            return Err(plain_error(
                "A port belongs in the host list for a seed list, and an SRV name takes none."
                    .into(),
            ));
        }
        (true, None) => host.to_string(),
        (false, port) => {
            // An IPv6 address, which a URI brackets; one colon is a port.
            let host = match host.matches(':').count() > 1 && !host.starts_with('[') {
                true => format!("[{host}]"),
                false => host.to_string(),
            };
            match port {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        }
    };
    let encoded = |text: &str| utf8_percent_encode(text, NON_ALPHANUMERIC).to_string();
    // A password only beside a user. Blank is not sent at all: the server
    // refuses an empty one, and the mechanisms that take none (X.509, AWS from
    // the environment) are told apart from SCRAM by its absence.
    let credentials = match (server.user.as_str(), server.password.as_str()) {
        ("", _) => String::new(),
        (user, "") => format!("{}@", encoded(user)),
        (user, password) => format!("{}:{}@", encoded(user), encoded(password)),
    };
    let scheme = match config.srv {
        true => "mongodb+srv",
        false => "mongodb",
    };
    let options = config.options.trim().trim_start_matches('?');
    Ok(format!(
        "{scheme}://{credentials}{hosts}/{}?{options}",
        encoded(&server.database)
    ))
}

/// A key in `options` that the profile's own fields decide, named in the
/// error rather than silently overridden by them.
fn owned_option(options: &str, tunnelled: bool) -> Option<String> {
    options
        .trim()
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let lowered = key.to_ascii_lowercase();
            if lowered.starts_with("tls") || lowered == "ssl" {
                Some(format!(
                    "Options sets {key}, which the Encryption setting decides on MongoDB."
                ))
            } else if tunnelled
                && lowered == "directconnection"
                && !value.eq_ignore_ascii_case("true")
            {
                Some(format!(
                    "Options sets {key}={value}, but an SSH tunnel reaches one server, so the \
                     connection through it is direct."
                ))
            } else {
                None
            }
        })
}

/// dbdelve's five rungs as the driver's TLS settings.
///
/// The driver's rustls offers no way to skip the hostname check alone, so
/// `verify-ca` checks the name too: stricter than asked, never weaker. Without
/// a named root it verifies against webpki-roots rather than the platform
/// store, as MySQL does, and fails loudly where the two disagree.
fn tls(server: &ServerConfig) -> Tls {
    match server.sslmode {
        SslMode::Disable => Tls::Disabled,
        SslMode::Prefer | SslMode::Require => Tls::Enabled(
            TlsOptions::builder()
                .allow_invalid_certificates(true)
                .build(),
        ),
        SslMode::VerifyCa | SslMode::VerifyFull => Tls::Enabled(
            TlsOptions::builder()
                .ca_file_path(server.root_certificate.as_deref().map(PathBuf::from))
                .build(),
        ),
    }
}

/// Whether a failed TLS handshake means the server speaks no TLS at all, which
/// is the one failure `prefer` answers by trying plaintext. A refused or
/// unanswered connect is not: it would fail the same way again.
fn tls_not_offered(error: &Error) -> bool {
    matches!(
        &*error.kind,
        ErrorKind::Io(io) if matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::InvalidData
                | std::io::ErrorKind::ConnectionReset
        )
    )
}

/// The one forwarded port reaches one server, so neither a list of them nor
/// the ones an SRV record names can be reached through it; and the driver
/// checks a certificate's name against the address it dials, which through a
/// tunnel is the loopback, as on MySQL. `verify-ca` checks the name here too.
fn unreachable_through_a_tunnel(config: &MongoConfig) -> Option<DbError> {
    let server = &config.server;
    server.ssh.as_ref()?;
    if config.srv || config.seed_list() {
        return Some(plain_error(
            "A seed list or an SRV name cannot be reached through an SSH tunnel: the tunnel \
             forwards one port to one server."
                .into(),
        ));
    }
    server.sslmode.checks_certificate().then(|| {
        plain_error(format!(
            "sslmode={} cannot be honoured through an SSH tunnel on MongoDB: the driver checks \
             the certificate against the address it dials, which is the tunnel's loopback \
             address rather than {}.",
            server.sslmode.as_str(),
            server.host
        ))
    })
}

/// The runtime and the client it drives, and the tunnel they dial through.
struct Driver {
    /// `None` only inside `drop`.
    client: Option<Client>,
    runtime: Runtime,
    _tunnel: Option<Arc<Tunnel>>,
}

impl Drop for Driver {
    /// Dropping the last client spawns the task that ends its server sessions,
    /// which panics outside a runtime.
    fn drop(&mut self) {
        let _entered = self.runtime.enter();
        self.client.take();
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
#[derive(Clone)]
pub struct Connection {
    driver: Arc<Driver>,
    /// The database the profile is in, and the explorer's one schema. Blank
    /// is none, as on MySQL: there is no login default to land in.
    database: String,
    /// The profile's statement timeout in seconds, or 0: the client-side bound
    /// [`Connection::call`] puts on every round trip.
    timeout: u32,
    /// Each user statement in flight, by the comment it carries, with the
    /// sender that drops it here once Cancel has asked the server to stop it.
    stops: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
}

impl Connection {
    pub fn open(config: &MongoConfig) -> Result<Self, DbError> {
        let server = &config.server;
        if let Some(error) = owned_option(&config.options, server.ssh.is_some()) {
            return Err(plain_error(error));
        }
        // Before the tunnel, which may have cost a hardware-key touch.
        if let Some(error) = unreachable_through_a_tunnel(config) {
            return Err(error);
        }
        let connection_string = connection_string(config)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("dbdelve-mongodb")
            .enable_all()
            .build()
            .map_err(|error| plain_error(format!("Could not start the connection: {error}")))?;

        tunnelled(server, DEFAULT_PORT, |tunnel| {
            let dial = tunnel.as_deref().map(Tunnel::dial).transpose()?;
            let client = guarded(|| {
                runtime.block_on(async {
                    let mut options = ClientOptions::parse(connection_string)
                        .await
                        .map_err(|error| plain_error(error.kind.to_string()))?;
                    let bound = Duration::from_secs(CONNECT_TIMEOUT_SECONDS);
                    options.app_name.get_or_insert_with(|| "DBDelve".into());
                    options.server_selection_timeout.get_or_insert(bound);
                    options.connect_timeout.get_or_insert(bound);
                    if let Some(dial) = dial {
                        options.hosts = vec![ServerAddress::Tcp {
                            host: dial.ip().to_string(),
                            port: Some(dial.port()),
                        }];
                        options.direct_connection = Some(true);
                    }
                    let tls = tls(server);
                    match attempt(options.clone(), tls.clone()).await {
                        // `prefer` is the one rung where plaintext is
                        // reachable, and only from a server that offers no
                        // TLS: a TLS hello sent to one is closed unanswered.
                        Err(error)
                            if server.sslmode == SslMode::Prefer && tls_not_offered(&error) =>
                        {
                            attempt(options, Tls::Disabled)
                                .await
                                .map_err(|error| connect_error(&error, config, false))
                        }
                        other => other.map_err(|error| {
                            connect_error(&error, config, matches!(tls, Tls::Enabled(_)))
                        }),
                    }
                })
            })??;
            Ok(Self {
                driver: Arc::new(Driver {
                    client: Some(client),
                    runtime,
                    _tunnel: tunnel,
                }),
                database: server.database.clone(),
                timeout: server.statement_timeout,
                stops: Arc::default(),
            })
        })
    }

    fn client(&self) -> &Client {
        self.driver
            .client
            .as_ref()
            .expect("taken only when the driver drops")
    }

    fn database(&self) -> Database {
        self.client().database(&self.database)
    }

    /// One driver call, blocked on, under the statement timeout when there is
    /// one, and with a driver panic reported rather than unwinding through the
    /// background thread.
    fn call<T>(
        &self,
        operation: impl IntoFuture<Output = mongodb::error::Result<T>>,
    ) -> Result<T, DbError> {
        let timeout = self.timeout;
        guarded(|| {
            self.driver.runtime.block_on(async {
                let outcome = match timeout {
                    0 => operation.await,
                    seconds => tokio::time::timeout(
                        Duration::from_secs(u64::from(seconds)),
                        operation.into_future(),
                    )
                    .await
                    .map_err(|_| {
                        plain_error(format!(
                            "Stopped after the statement timeout of {seconds} seconds."
                        ))
                    })?,
                };
                outcome.map_err(|error| plain_error(error.kind.to_string()))
            })
        })?
    }

    /// Run the statements in `text` in turn and keep the last one's result,
    /// as a Postgres submission of several keeps its last set. The run path
    /// sends one at a time; this is for a selection `mql` reads as more.
    pub fn query(&self, text: &str, cancel: &CancelToken) -> Result<QueryResult, DbError> {
        let statements = mql::parse(text).map_err(|error| DbError {
            message: error.message,
            position: Some(error.at),
        })?;
        let started = Instant::now();
        let mut result = QueryResult::default();
        for statement in &statements {
            result = self.statement(&statement.target, Run::start(self, cancel)?)?;
        }
        result.elapsed = started.elapsed();
        Ok(result)
    }

    /// Ask the server to stop every statement running under `cancel`, then
    /// drop each one here whether or not the server found it: one still on its
    /// way there has nothing to kill yet. The client's pool is untouched, so
    /// the session carries on.
    pub fn cancel(&self, cancel: &CancelToken) -> Result<(), DbError> {
        let comments = cancel
            .0
            .lock()
            .map(|mut running| {
                running.asked = true;
                running.handles.clone()
            })
            .unwrap_or_default();
        comments
            .iter()
            .map(|comment| {
                let killed = self.kill(comment);
                if let Some(stop) = lock(&self.stops).remove(comment) {
                    let _ = stop.send(());
                }
                killed
            })
            .fold(Ok(()), Result::and)
    }

    /// End the server's operations carrying `comment`. A user may list and
    /// kill their own operations without the privilege to see anyone else's.
    fn kill(&self, comment: &str) -> Result<(), DbError> {
        let admin = self.client().database("admin");
        let carrying = doc! {
            "$or": [
                { "command.comment": comment },
                { "cursor.originatingCommand.comment": comment },
            ]
        };
        self.call(async {
            let operations: Vec<Document> = admin
                .aggregate([doc! { "$currentOp": {} }, doc! { "$match": carrying }])
                .await?
                .try_collect()
                .await?;
            for operation in operations {
                if let Some(id) = operation.get("opid") {
                    admin
                        .run_command(doc! { "killOp": 1, "op": id.clone() })
                        .await?;
                }
            }
            Ok(())
        })
        .map_err(|error| {
            plain_error(format!(
                "Could not ask the server to cancel: {}",
                error.message
            ))
        })
    }

    /// `database`, or the profile's own when the statement names none.
    fn named(&self, database: Option<&str>) -> Result<Database, DbError> {
        match database.unwrap_or(&self.database) {
            "" => Err(plain_error(
                "No database is selected, so `db` names none.".into(),
            )),
            name => Ok(self.client().database(name)),
        }
    }

    fn statement(&self, target: &Target, run: Run) -> Result<QueryResult, DbError> {
        match target {
            Target::Show(Show::Databases) => self.database_list(run),
            Target::Show(Show::Collections) => collection_names(run, &self.named(None)?),
            Target::Database { database, call } => {
                self.database_call(run, database.as_deref(), call)
            }
            Target::Collection {
                database,
                collection,
                call,
                cursor,
            } => self.collection_call(run, database.as_deref(), collection, call, cursor),
        }
    }

    /// `show dbs`: the databases the user holds a role on, with their size.
    fn database_list(&self, run: Run) -> Result<QueryResult, DbError> {
        let admin = self.client().database("admin");
        let reply = command(
            run,
            &admin,
            doc! { "listDatabases": 1, "authorizedDatabases": true },
        )?;
        let mut databases = reply
            .get_array("databases")
            .map(|listed| {
                listed
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|listed| {
                        let field = |name| listed.get(name).cloned().unwrap_or(Bson::Null);
                        doc! { "name": field("name"), "sizeOnDisk": field("sizeOnDisk") }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        databases.sort_by(|a, b| a.get_str("name").ok().cmp(&b.get_str("name").ok()));
        Ok(documents(databases))
    }

    fn database_call(
        &self,
        run: Run,
        database: Option<&str>,
        call: &Call<DbMethod>,
    ) -> Result<QueryResult, DbError> {
        let args = Args::of(call.method.name(), &call.args);
        match call.method {
            DbMethod::RunCommand | DbMethod::AdminCommand => {
                let mut sent = match args.bson(0)? {
                    Some(Bson::String(name)) => doc! { name: 1 },
                    Some(Bson::Document(sent)) => sent,
                    _ => return Err(args.not(0, "a command document")),
                };
                if args.value(1).is_some() {
                    return Err(plain_error(format!(
                        "{} takes the command document alone here.",
                        args.method
                    )));
                }
                let target = match call.method {
                    DbMethod::AdminCommand => self.client().database("admin"),
                    _ => self.named(database)?,
                };
                // The user's command goes as written but for the comment Cancel
                // finds it by: no `maxTimeMS` is added, since the command is
                // theirs to bound, and the client-side bound still holds.
                if !sent.contains_key("comment") {
                    sent.insert("comment", run.comment.as_str());
                }
                Ok(reply(run.execute(target.run_command(sent))?))
            }
            DbMethod::GetCollectionNames => collection_names(run, &self.named(database)?),
            DbMethod::Stats => {
                let mut sent = doc! { "dbStats": 1 };
                match args.bson(0)? {
                    None => {}
                    Some(Bson::Document(options)) => sent.extend(options),
                    Some(scale) => {
                        sent.insert("scale", scale);
                    }
                }
                Ok(reply(command(run, &self.named(database)?, sent)?))
            }
            DbMethod::CreateCollection => {
                let mut sent = doc! { "create": args.string(0)? };
                sent.extend(args.document(1)?.unwrap_or_default());
                Ok(reply(command(run, &self.named(database)?, sent)?))
            }
            DbMethod::CreateView => {
                let mut sent = doc! {
                    "create": args.string(0)?,
                    "viewOn": args.string(1)?,
                    "pipeline": args.pipeline(2)?,
                };
                sent.extend(args.document(3)?.unwrap_or_default());
                Ok(reply(command(run, &self.named(database)?, sent)?))
            }
            DbMethod::DropDatabase => {
                let mut sent = doc! { "dropDatabase": 1 };
                if let Some(concern) = args.bson(0)? {
                    sent.insert("writeConcern", concern);
                }
                Ok(reply(command(run, &self.named(database)?, sent)?))
            }
        }
    }

    fn collection_call(
        &self,
        run: Run,
        database: Option<&str>,
        collection: &str,
        call: &Call<Method>,
        cursor: &[Call<CursorMethod>],
    ) -> Result<QueryResult, DbError> {
        let database = self.named(database)?;
        let args = Args::of(call.method.name(), &call.args);
        match call.method {
            Method::Find | Method::FindOne => {
                let mut sent = doc! { "find": collection };
                if let Some(filter) = args.document(0)? {
                    sent.insert("filter", filter);
                }
                if let Some(projection) = args.document(1)? {
                    sent.insert("projection", projection);
                }
                sent.extend(args.document(2)?.unwrap_or_default());
                if call.method == Method::FindOne {
                    sent.insert("limit", 1);
                    sent.insert("singleBatch", true);
                }
                match chained(&mut sent, cursor, true)? {
                    Some(verbosity) => explained(run, &database, sent, verbosity),
                    None => Ok(documents(fetch(run, &database, sent)?)),
                }
            }
            Method::Aggregate => {
                let mut sent = doc! {
                    "aggregate": collection,
                    "pipeline": args.pipeline(0)?,
                    "cursor": {},
                };
                let mut options = args.document(1)?.unwrap_or_default();
                if let Some(size) = options.remove("batchSize") {
                    sent.insert("cursor", doc! { "batchSize": size });
                }
                sent.extend(options);
                match chained(&mut sent, cursor, false)? {
                    Some(verbosity) => explained(run, &database, sent, verbosity),
                    None => Ok(documents(fetch(run, &database, sent)?)),
                }
            }
            // As the drivers count: a `$group` over the matching documents,
            // since the `count` command reads a stale total off the metadata.
            Method::CountDocuments => {
                let mut options = args.document(1)?.unwrap_or_default();
                let mut pipeline = vec![doc! { "$match": args.document(0)?.unwrap_or_default() }];
                for stage in ["skip", "limit"] {
                    if let Some(n) = options.remove(stage) {
                        pipeline.push(doc! { format!("${stage}"): n });
                    }
                }
                pipeline.push(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } });
                let mut sent = doc! { "aggregate": collection, "pipeline": pipeline, "cursor": {} };
                sent.extend(options);
                let counted = fetch(run, &database, sent)?;
                let n = counted
                    .first()
                    .and_then(|counted| counted.get("n"))
                    .cloned()
                    .unwrap_or(Bson::Int32(0));
                Ok(documents(vec![doc! { "count": n }]))
            }
            Method::EstimatedDocumentCount => {
                let mut sent = doc! { "count": collection };
                sent.extend(args.document(0)?.unwrap_or_default());
                let counted = command(run, &database, sent)?;
                let n = counted.get("n").cloned().unwrap_or(Bson::Null);
                Ok(documents(vec![doc! { "count": n }]))
            }
            Method::Distinct => {
                let key = args.string(0)?;
                let mut sent = doc! { "distinct": collection, "key": key.as_str() };
                if let Some(query) = args.document(1)? {
                    sent.insert("query", query);
                }
                sent.extend(args.document(2)?.unwrap_or_default());
                let values = command(run, &database, sent)?
                    .get_array("values")
                    .cloned()
                    .unwrap_or_default();
                Ok(documents(
                    values
                        .into_iter()
                        .map(|value| doc! { key.as_str(): value })
                        .collect(),
                ))
            }
            Method::GetIndexes => Ok(documents(fetch(
                run,
                &database,
                doc! { "listIndexes": collection },
            )?)),
            Method::InsertOne | Method::InsertMany => {
                let given = match call.method {
                    Method::InsertOne => vec![args.required_document(0)?],
                    _ => args.documents(0)?,
                };
                let (ids, inserted): (Vec<Bson>, Vec<Document>) =
                    given.into_iter().map(with_id).unzip();
                let mut sent = doc! { "insert": collection, "documents": inserted };
                sent.extend(args.document(1)?.unwrap_or_default());
                let written = write(run, &database, sent)?;
                let shown = match call.method {
                    Method::InsertOne => {
                        doc! { "acknowledged": true, "insertedId": ids[0].clone() }
                    }
                    _ => doc! { "acknowledged": true, "insertedIds": ids },
                };
                Ok(affected(documents(vec![shown]), &written))
            }
            Method::UpdateOne | Method::UpdateMany | Method::ReplaceOne => {
                let update = args.required(1)?;
                operators(call.method, &update)?;
                let mut statement = doc! {
                    "q": args.required_document(0)?,
                    "u": update,
                    "multi": call.method == Method::UpdateMany,
                };
                let mut sent = doc! { "update": collection };
                for (key, value) in args.document(2)?.unwrap_or_default() {
                    match key.as_str() {
                        "upsert" | "arrayFilters" | "hint" | "collation" | "sort" => {
                            statement.insert(key, value)
                        }
                        _ => sent.insert(key, value),
                    };
                }
                sent.insert("updates", vec![statement]);
                let written = write(run, &database, sent)?;
                let upserted = written.get_array("upserted").map_or(&[][..], Vec::as_slice);
                let upserted_id = upserted
                    .first()
                    .and_then(Bson::as_document)
                    .and_then(|upserted| upserted.get("_id"))
                    .cloned()
                    .unwrap_or(Bson::Null);
                let shown = doc! {
                    "acknowledged": true,
                    "matchedCount": number(written.get("n")) - upserted.len() as i64,
                    "modifiedCount": number(written.get("nModified")),
                    "upsertedId": upserted_id,
                };
                Ok(affected(documents(vec![shown]), &written))
            }
            Method::DeleteOne | Method::DeleteMany => {
                let mut statement = doc! {
                    "q": args.required_document(0)?,
                    "limit": i32::from(call.method == Method::DeleteOne),
                };
                let mut sent = doc! { "delete": collection };
                for (key, value) in args.document(1)?.unwrap_or_default() {
                    match key.as_str() {
                        "hint" | "collation" => statement.insert(key, value),
                        _ => sent.insert(key, value),
                    };
                }
                sent.insert("deletes", vec![statement]);
                let written = write(run, &database, sent)?;
                let shown = doc! { "acknowledged": true, "deletedCount": number(written.get("n")) };
                Ok(affected(documents(vec![shown]), &written))
            }
            Method::FindOneAndUpdate | Method::FindOneAndReplace | Method::FindOneAndDelete => {
                let mut sent = doc! {
                    "findAndModify": collection,
                    "query": args.required_document(0)?,
                };
                let options = match call.method {
                    Method::FindOneAndDelete => {
                        sent.insert("remove", true);
                        args.document(1)?
                    }
                    _ => {
                        let update = args.required(1)?;
                        operators(call.method, &update)?;
                        sent.insert("update", update);
                        args.document(2)?
                    }
                };
                for (key, value) in options.unwrap_or_default() {
                    match key.as_str() {
                        "projection" => sent.insert("fields", value),
                        "returnNewDocument" => sent.insert("new", value),
                        "returnDocument" => match value.as_str() {
                            Some("after") => sent.insert("new", true),
                            Some("before") => sent.insert("new", false),
                            _ => {
                                return Err(plain_error(format!(
                                    "{}'s returnDocument is \"before\" or \"after\".",
                                    args.method
                                )));
                            }
                        },
                        _ => sent.insert(key, value),
                    };
                }
                let written = write(run, &database, sent)?;
                let found = match written.get("value") {
                    Some(Bson::Document(found)) => vec![found.clone()],
                    _ => Vec::new(),
                };
                let mut result = documents(found);
                result.rows_affected = written
                    .get_document("lastErrorObject")
                    .ok()
                    .and_then(|outcome| u64::try_from(number(outcome.get("n"))).ok());
                Ok(result)
            }
            Method::CreateIndex | Method::CreateIndexes => {
                let keys = match call.method {
                    Method::CreateIndex => vec![args.required_document(0)?],
                    _ => args.documents(0)?,
                };
                let options = args.document(1)?.unwrap_or_default();
                let indexes: Vec<Document> = keys
                    .into_iter()
                    .map(|key| {
                        let name = index_name(&key);
                        let mut index = doc! { "key": key };
                        index.extend(options.clone());
                        if !index.contains_key("name") {
                            index.insert("name", name);
                        }
                        index
                    })
                    .collect();
                let names = indexes
                    .iter()
                    .map(|index| doc! { "name": index.get("name").cloned().unwrap_or(Bson::Null) })
                    .collect();
                let mut sent = doc! { "createIndexes": collection, "indexes": indexes };
                if let Some(quorum) = args.bson(2)? {
                    sent.insert("commitQuorum", quorum);
                }
                write(run, &database, sent)?;
                Ok(documents(names))
            }
            Method::DropIndex => {
                let index = args.required(0)?;
                Ok(reply(command(
                    run,
                    &database,
                    doc! { "dropIndexes": collection, "index": index },
                )?))
            }
            Method::DropIndexes => {
                let index = args.bson(0)?.unwrap_or_else(|| "*".into());
                Ok(reply(command(
                    run,
                    &database,
                    doc! { "dropIndexes": collection, "index": index },
                )?))
            }
            Method::Drop => {
                let mut sent = doc! { "drop": collection };
                sent.extend(args.document(0)?.unwrap_or_default());
                Ok(reply(command(run, &database, sent)?))
            }
            Method::RenameCollection => {
                let name = database.name();
                let mut sent = doc! {
                    "renameCollection": format!("{name}.{collection}"),
                    "to": format!("{name}.{}", args.string(0)?),
                };
                if let Some(drop_target) = args.bson(1)? {
                    sent.insert("dropTarget", drop_target);
                }
                let admin = self.client().database("admin");
                Ok(reply(command(run, &admin, sent)?))
            }
        }
    }

    pub fn databases(&self) -> Result<super::Databases, DbError> {
        let mut names = self.call(
            self.client()
                .list_database_names()
                .authorized_databases(true),
        )?;
        names.sort();
        Ok(super::Databases {
            names,
            current: Some(self.database.clone()).filter(|name| !name.is_empty()),
        })
    }

    /// One schema, the connected database. `system.*` is the server's own
    /// bookkeeping (`system.views`, a time-series collection's
    /// `system.buckets.…`), which an ordinary user cannot read.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        if self.database.is_empty() {
            return Ok(Catalog::default());
        }
        let mut relations = self
            .collections()?
            .iter()
            .filter_map(relation)
            .collect::<Vec<_>>();
        relations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Catalog {
            schemas: vec![Schema {
                name: self.database.clone(),
                relations,
                routines: Vec::new(),
            }],
        })
    }

    /// MongoDB stores no routines a catalog could list.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    /// Each collection's bytes on disk, indexes included as the other engines
    /// count them, and its document count, from `$collStats`. A view has
    /// neither, and a time-series collection no count.
    ///
    /// ponytail: one `$collStats` per collection, one after another. Fine for
    /// hundreds; run them concurrently if a database of thousands is slow.
    pub fn sizes(&self) -> Result<Sizes, DbError> {
        if self.database.is_empty() {
            return Ok(Sizes::new());
        }
        let database = self.database();
        let mut sizes = std::collections::HashMap::new();
        for listed in self.collections()? {
            let Some(relation) =
                relation(&listed).filter(|relation| relation.kind == RelationKind::Table)
            else {
                continue;
            };
            let collection = database.collection::<Document>(&relation.name);
            let reports: Vec<Document> = self.call(async {
                collection
                    .aggregate([doc! { "$collStats": { "storageStats": {} } }])
                    .await?
                    .try_collect()
                    .await
            })?;
            let statistics = statistics(&reports);
            if statistics != Statistics::default() {
                sizes.insert(relation.name, statistics);
            }
        }
        Ok(Sizes::from([(self.database.clone(), sizes)]))
    }

    /// Columns sampled from the documents, since nothing declares them. A
    /// collection is keyed by `_id`, which every document has and an index
    /// keeps unique, so it is the primary key `Structure::row_key` reads; a
    /// view's `_id` is whatever its pipeline made it, and keys nothing.
    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let database = self.client().database(schema);
        let listed: Vec<Document> = self.call(async {
            database
                .run_cursor_command(doc! {
                    "listCollections": 1,
                    "filter": { "name": relation },
                })
                .await?
                .try_collect()
                .await
        })?;
        let listed = listed
            .into_iter()
            .next()
            .ok_or_else(|| plain_error(format!("{schema} has no collection {relation}.")))?;
        let collection = database.collection::<Document>(relation);
        let sample: Vec<Document> = self.call(async {
            collection
                .aggregate([doc! { "$sample": { "size": SAMPLE_SIZE } }])
                .await?
                .try_collect()
                .await
        })?;
        let mut structure = Structure {
            columns: sampled_columns(&sample),
            ..Structure::default()
        };

        if listed.get_str("type") != Ok("view") {
            let indexes: Vec<Document> = self.call(async {
                database
                    .run_cursor_command(doc! { "listIndexes": relation })
                    .await?
                    .try_collect()
                    .await
            })?;
            structure.indexes = indexes
                .iter()
                .map(|index| NamedDefinition {
                    name: index.get_str("name").unwrap_or_default().to_string(),
                    definition: index_definition(index),
                })
                .collect();
            structure.constraints.push(NamedDefinition {
                name: "_id_".into(),
                definition: "PRIMARY KEY (_id)".into(),
            });
        }
        if let Ok(validator) = listed
            .get_document("options")
            .and_then(|options| options.get_document("validator"))
        {
            structure.constraints.push(NamedDefinition {
                name: "validator".into(),
                definition: relaxed_json(validator),
            });
        }
        Ok(structure)
    }

    /// Names and types only, which is what lets a user without the
    /// `listCollections` privilege still see the collections it may read.
    fn collections(&self) -> Result<Vec<Document>, DbError> {
        let database = self.database();
        self.call(async {
            database
                .run_cursor_command(doc! {
                    "listCollections": 1,
                    "nameOnly": true,
                    "authorizedCollections": true,
                })
                .await?
                .try_collect()
                .await
        })
    }
}

/// A `listCollections` entry as a relation, or `None` for the server's own.
fn relation(listed: &Document) -> Option<Relation> {
    let name = listed.get_str("name").ok()?;
    if name.starts_with("system.") {
        return None;
    }
    Some(Relation {
        name: name.to_string(),
        // A time-series collection reads and writes as a collection does.
        kind: match listed.get_str("type") {
            Ok("view") => RelationKind::View,
            _ => RelationKind::Table,
        },
        partition_of: None,
        size: None,
        rows: None,
    })
}

/// Every top-level field the sample holds, `_id` first and the rest in the
/// order they were first seen, each with every type it was seen holding.
/// Nullable when a document lacks the field or holds null in it: either way a
/// row may show nothing there.
fn sampled_columns(documents: &[Document]) -> Vec<ColumnDefinition> {
    let mut fields: Vec<(&str, Vec<&'static str>, usize)> = Vec::new();
    let mut positions = std::collections::HashMap::new();
    for document in documents {
        for (name, value) in document {
            let position = *positions.entry(name.as_str()).or_insert_with(|| {
                fields.push((name.as_str(), Vec::new(), 0));
                fields.len() - 1
            });
            let (_, types, present) = &mut fields[position];
            *present += 1;
            let alias = type_alias(value);
            if !types.contains(&alias) {
                types.push(alias);
            }
        }
    }
    fields.sort_by_key(|(name, ..)| *name != "_id");
    fields
        .into_iter()
        .map(|(name, mut types, present)| {
            types.sort_by_key(|alias| TYPE_ORDER.iter().position(|order| order == alias));
            ColumnDefinition {
                name: name.to_string(),
                nullable: present < documents.len() || types.contains(&"null"),
                data_type: types.join(" | "),
                default: None,
            }
        })
        .collect()
}

/// The `&'static` spelling of a cell type read back as text.
pub(super) fn cell_type(name: &str) -> Option<&'static str> {
    TYPE_ORDER
        .into_iter()
        .chain([MISSING])
        .find(|alias| *alias == name)
}

/// The server's own name for a value's type, as `$type` spells it.
pub(super) fn type_alias(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// A `listIndexes` entry as its key and the options that change what it
/// does: `{"external_id":1} unique`.
fn index_definition(index: &Document) -> String {
    let mut parts = vec![
        index
            .get_document("key")
            .map(relaxed_json)
            .unwrap_or_default(),
    ];
    for flag in ["unique", "sparse"] {
        if index.get_bool(flag) == Ok(true) {
            parts.push(flag.to_string());
        }
    }
    if let Some(seconds) = index.get("expireAfterSeconds") {
        parts.push(format!("TTL {}s", seconds.clone().into_relaxed_extjson()));
    }
    for (option, label) in [
        ("partialFilterExpression", "partial"),
        ("collation", "collation"),
    ] {
        if let Ok(document) = index.get_document(option) {
            parts.push(format!("{label} {}", relaxed_json(document)));
        }
    }
    parts.join(" ")
}

/// A document as Relaxed Extended JSON on one line, the shell's own reading
/// of it.
pub(super) fn relaxed_json(document: &Document) -> String {
    Bson::Document(document.clone())
        .into_relaxed_extjson()
        .to_string()
}

/// `$collStats` answers once per shard, so the numbers are summed; one shard
/// without a number leaves the total unknown rather than short.
fn statistics(reports: &[Document]) -> Statistics {
    if reports.is_empty() {
        return Statistics::default();
    }
    let total = |field: &str| {
        reports
            .iter()
            .map(|report| {
                let value = report.get_document("storageStats").ok()?.get(field)?;
                match value {
                    Bson::Int32(number) => u64::try_from(*number).ok(),
                    Bson::Int64(number) => u64::try_from(*number).ok(),
                    Bson::Double(number) if *number >= 0.0 => Some(*number as u64),
                    _ => None,
                }
            })
            .sum::<Option<u64>>()
    };
    Statistics {
        size: total("totalSize"),
        rows: total("count"),
    }
}

/// One client, connected and logged in: a `ping` is the first thing that
/// selects a server and authenticates. A connect to one server fails on the
/// first failed check of it rather than once the selection timeout runs out,
/// since a server that refused or closed on one check does the same on the
/// next; a list of them gets the timeout, as one failing does not mean the
/// others will.
async fn attempt(mut options: ClientOptions, tls: Tls) -> Result<Client, Error> {
    let (failed, mut heartbeat) = oneshot::channel();
    let failed = Mutex::new(Some(failed));
    options.tls = Some(tls);
    options.sdam_event_handler = Some(EventHandler::callback(move |event| {
        if let SdamEvent::ServerHeartbeatFailed(event) = event
            && let Some(failed) = failed.lock().unwrap_or_else(PoisonError::into_inner).take()
        {
            let _ = failed.send(event.failure);
        }
    }));
    let one_server = options.hosts.len() == 1;
    let client = Client::with_options(options)?;
    let admin = client.database("admin");
    let ping = admin.run_command(doc! { "ping": 1 });
    let outcome = match one_server {
        true => tokio::select! {
            outcome = ping => outcome,
            Ok(failure) = &mut heartbeat => Err(failure),
        },
        false => ping.await,
    };
    match outcome {
        Ok(_) => Ok(client),
        // What the server check said is the cause; the selection timeout
        // only reports that there was one.
        Err(error) => Err(heartbeat.try_recv().unwrap_or(error)),
    }
}

/// What happened, in the profile's words: its host rather than the tunnel's
/// loopback, and the server's own message for anything past the socket.
fn connect_error(error: &Error, config: &MongoConfig, tls: bool) -> DbError {
    let endpoint = config.endpoint();
    plain_error(match &*error.kind {
        ErrorKind::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                format!("Connection refused: nothing is listening on {endpoint}")
            }
            std::io::ErrorKind::TimedOut => {
                format!("No answer from {endpoint} within {CONNECT_TIMEOUT_SECONDS} seconds.")
            }
            std::io::ErrorKind::UnexpectedEof if tls => {
                format!("{endpoint} closed the connection during the TLS handshake.")
            }
            _ if tls && tls_not_offered(error) => {
                format!("The TLS handshake with {endpoint} failed: {io}")
            }
            _ => format!("{endpoint}: {io}"),
        },
        ErrorKind::ServerSelection { .. } => {
            format!("No server at {endpoint} answered within {CONNECT_TIMEOUT_SECONDS} seconds.")
        }
        kind => kind.to_string(),
    })
}

/// A panic inside the driver would take the background thread with it, and
/// is a failure to report rather than a crash.
fn guarded<T>(call: impl FnOnce() -> T) -> Result<T, DbError> {
    std::panic::catch_unwind(AssertUnwindSafe(call)).map_err(|panic| {
        let detail = panic
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        plain_error(format!("The MongoDB driver failed: {detail}"))
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One user statement's claim on Cancel, given back however the statement
/// ends. Its comment is what `$currentOp` lists the server's operation under,
/// which is how a cancel from another thread finds it there.
struct Run<'a> {
    connection: &'a Connection,
    token: &'a CancelToken,
    comment: String,
    stopped: oneshot::Receiver<()>,
}

impl<'a> Run<'a> {
    fn start(connection: &'a Connection, token: &'a CancelToken) -> Result<Self, DbError> {
        let comment = format!("dbdelve:{}", Uuid::new());
        let (stop, stopped) = oneshot::channel();
        lock(&connection.stops).insert(comment.clone(), stop);
        let asked = token.0.lock().is_ok_and(|mut running| {
            running.handles.push(comment.clone());
            running.asked
        });
        let run = Self {
            connection,
            token,
            comment,
            stopped,
        };
        match asked {
            // Cancel landed between two statements of one submission.
            true => Err(plain_error("Cancelled before it was sent.".into())),
            false => Ok(run),
        }
    }

    /// `sent` labelled with this run's comment and bounded on the server by
    /// the statement timeout, unless the statement set either itself.
    fn tagged(&self, mut sent: Document) -> Document {
        if !sent.contains_key("comment") {
            sent.insert("comment", self.comment.as_str());
        }
        if self.connection.timeout > 0 && !sent.contains_key("maxTimeMS") {
            sent.insert("maxTimeMS", i64::from(self.connection.timeout) * 1000);
        }
        sent
    }

    /// The statement's one operation, its cursor's `getMore`s included: bounded
    /// here by the statement timeout too, and dropped when Cancel says so.
    fn execute<T>(
        mut self,
        operation: impl IntoFuture<Output = mongodb::error::Result<T>>,
    ) -> Result<T, DbError> {
        let connection = self.connection;
        let timeout = connection.timeout;
        let stopped = &mut self.stopped;
        let outcome = guarded(|| {
            connection.driver.runtime.block_on(async {
                let bound = async {
                    match timeout {
                        0 => std::future::pending::<()>().await,
                        seconds => {
                            tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await
                        }
                    }
                };
                tokio::select! {
                    outcome = operation.into_future() => {
                        Some(outcome.map_err(|error| plain_error(error.kind.to_string())))
                    }
                    Ok(()) = stopped => Some(Err(plain_error("Cancelled.".into()))),
                    () = bound => None,
                }
            })
        })?;
        outcome.unwrap_or_else(|| {
            // What the server's own bound did not stop, or a command that
            // carries none, is stopped the way Cancel stops it.
            let _ = connection.kill(&self.comment);
            Err(plain_error(format!(
                "Stopped after the statement timeout of {timeout} seconds."
            )))
        })
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        lock(&self.connection.stops).remove(&self.comment);
        if let Ok(mut running) = self.token.0.lock() {
            running.handles.retain(|handle| *handle != self.comment);
        }
    }
}

/// One command and its reply.
fn command(run: Run, database: &Database, sent: Document) -> Result<Document, DbError> {
    let sent = run.tagged(sent);
    run.execute(database.run_command(sent))
}

/// Every document a cursor command returns, through as many `getMore`s as it
/// takes. Nothing is limited here: the statement's own `limit` is the only one
/// (hard rule 1), and the whole result is held, as the SQL engines hold theirs.
fn fetch(run: Run, database: &Database, sent: Document) -> Result<Vec<Document>, DbError> {
    let sent = run.tagged(sent);
    let comment = sent.get("comment").cloned();
    run.execute(async {
        let mut action = database.run_cursor_command(sent);
        if let Some(comment) = comment {
            action = action.comment(comment);
        }
        action.await?.try_collect().await
    })
}

/// A write command's reply, or its first write error as the failure. The
/// server answers `ok: 1` to a write command whose every write failed, so the
/// reply alone says nothing.
fn write(run: Run, database: &Database, sent: Document) -> Result<Document, DbError> {
    let written = command(run, database, sent)?;
    if let Some(error) = written
        .get_array("writeErrors")
        .ok()
        .and_then(|errors| errors.first())
        .and_then(Bson::as_document)
    {
        let message = error.get_str("errmsg").unwrap_or("A write failed.");
        return Err(plain_error(match number(written.get("n")) {
            0 => message.to_string(),
            n => format!("{message} The statement's other writes applied: {n}."),
        }));
    }
    if let Ok(error) = written.get_document("writeConcernError") {
        return Err(plain_error(format!(
            "The writes applied, but their write concern failed: {}",
            error.get_str("errmsg").unwrap_or_default()
        )));
    }
    Ok(written)
}

/// `show collections` and `getCollectionNames()`: every name, sorted, the
/// server's own `system.*` included, since the statement asked the server.
fn collection_names(run: Run, database: &Database) -> Result<QueryResult, DbError> {
    let listed = fetch(
        run,
        database,
        doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
    )?;
    let mut names: Vec<&str> = listed
        .iter()
        .filter_map(|listed| listed.get_str("name").ok())
        .collect();
    names.sort_unstable();
    Ok(documents(
        names
            .into_iter()
            .map(|name| doc! { "name": name })
            .collect(),
    ))
}

/// The cursor methods chained after `find` or `aggregate`, as the fields of
/// the command they set, and the verbosity of an `explain` among them.
fn chained(
    sent: &mut Document,
    cursor: &[Call<CursorMethod>],
    find: bool,
) -> Result<Option<Bson>, DbError> {
    let mut explain = None;
    for call in cursor {
        let args = Args::of(call.method.name(), &call.args);
        let field = match call.method {
            CursorMethod::Sort if find => "sort",
            CursorMethod::Limit if find => "limit",
            CursorMethod::Skip if find => "skip",
            CursorMethod::Projection if find => "projection",
            CursorMethod::Sort
            | CursorMethod::Limit
            | CursorMethod::Skip
            | CursorMethod::Projection => {
                return Err(plain_error(format!(
                    "aggregate's cursor has no {}(); a pipeline stage does that.",
                    args.method
                )));
            }
            CursorMethod::Hint => "hint",
            CursorMethod::Collation => "collation",
            CursorMethod::Comment => "comment",
            CursorMethod::MaxTimeMs => "maxTimeMS",
            CursorMethod::Explain => {
                explain = Some(match args.bson(0)? {
                    None | Some(Bson::Boolean(false)) => "queryPlanner".into(),
                    Some(Bson::Boolean(true)) => "allPlansExecution".into(),
                    Some(verbosity @ Bson::String(_)) => verbosity,
                    Some(_) => return Err(args.not(0, "a verbosity")),
                });
                continue;
            }
            CursorMethod::ToArray | CursorMethod::Pretty => continue,
        };
        sent.insert(field, args.required(0)?);
    }
    Ok(explain)
}

/// `.explain(verbosity)`: the command wrapped in an `explain`, which takes the
/// comment and the time bound itself rather than inside what it explains.
fn explained(
    run: Run,
    database: &Database,
    sent: Document,
    verbosity: Bson,
) -> Result<QueryResult, DbError> {
    let mut sent = run.tagged(sent);
    let generic: Vec<(&str, Bson)> = ["comment", "maxTimeMS"]
        .into_iter()
        .filter_map(|field| sent.remove(field).map(|value| (field, value)))
        .collect();
    let mut explain = doc! { "explain": sent, "verbosity": verbosity };
    explain.extend(
        generic
            .into_iter()
            .map(|(field, value)| (field.to_string(), value)),
    );
    Ok(reply(run.execute(database.run_command(explain))?))
}

/// mongosh refuses an update that would replace the document, and a
/// replacement that would update it, before anything is sent: the server
/// takes either for the other.
fn operators(method: Method, update: &Bson) -> Result<(), DbError> {
    let operators = match update {
        Bson::Document(fields) if !fields.is_empty() => {
            fields.keys().filter(|key| key.starts_with('$')).count() == fields.len()
        }
        // An aggregation pipeline is an update.
        Bson::Array(_) => true,
        _ => false,
    };
    let replacement =
        matches!(update, Bson::Document(fields) if !fields.keys().any(|key| key.starts_with('$')));
    match method {
        Method::ReplaceOne | Method::FindOneAndReplace if !replacement => {
            Err(plain_error(format!(
                "{}'s replacement holds update operators; updateOne applies them.",
                method.name()
            )))
        }
        Method::UpdateOne | Method::UpdateMany | Method::FindOneAndUpdate if !operators => {
            Err(plain_error(format!(
                "{}'s update is not all update operators ($set, $inc, …), so it would replace \
                 the document; replaceOne does that.",
                method.name()
            )))
        }
        _ => Ok(()),
    }
}

/// The document with an `_id`, minted here when it has none, as every driver
/// does: the server would mint one too, and not say which.
fn with_id(document: Document) -> (Bson, Document) {
    match document.get("_id").cloned() {
        Some(id) => (id, document),
        None => {
            let id = Bson::ObjectId(ObjectId::new());
            let mut first = doc! { "_id": id.clone() };
            first.extend(document);
            (id, first)
        }
    }
}

/// The name the server's helpers give an index nobody named: `a_1_b_-1`.
fn index_name(key: &Document) -> String {
    key.iter()
        .map(|(field, kind)| match kind {
            Bson::String(kind) => format!("{field}_{kind}"),
            other => format!("{field}_{}", cell(other).unwrap_or_default()),
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn number(value: Option<&Bson>) -> i64 {
    match value {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        Some(Bson::Double(n)) => *n as i64,
        _ => 0,
    }
}

/// The server's count of the documents a write touched, which on an update is
/// the ones it matched, as an unchanged row still counts on Postgres.
fn affected(mut result: QueryResult, written: &Document) -> QueryResult {
    result.rows_affected = u64::try_from(number(written.get("n"))).ok();
    result
}

/// A reply as one row of its fields.
fn reply(reply: Document) -> QueryResult {
    documents(vec![reply])
}

/// A method's arguments, read as the types its command needs.
struct Args<'a> {
    method: &'static str,
    args: &'a [Arg],
}

impl<'a> Args<'a> {
    fn of(method: &'static str, args: &'a [Arg]) -> Self {
        Self { method, args }
    }

    fn value(&self, index: usize) -> Option<&Value> {
        self.args.get(index).map(|arg| &arg.value)
    }

    fn bson(&self, index: usize) -> Result<Option<Bson>, DbError> {
        self.value(index).map(bson).transpose()
    }

    fn required(&self, index: usize) -> Result<Bson, DbError> {
        self.bson(index)?.ok_or_else(|| self.not(index, "given"))
    }

    /// An optional document, where `null` is as good as leaving it out.
    fn document(&self, index: usize) -> Result<Option<Document>, DbError> {
        match self.bson(index)? {
            None | Some(Bson::Null) => Ok(None),
            Some(Bson::Document(document)) => Ok(Some(document)),
            Some(_) => Err(self.not(index, "a document")),
        }
    }

    fn required_document(&self, index: usize) -> Result<Document, DbError> {
        self.document(index)?
            .ok_or_else(|| self.not(index, "a document"))
    }

    fn documents(&self, index: usize) -> Result<Vec<Document>, DbError> {
        match self.bson(index)? {
            Some(Bson::Array(values)) => values
                .into_iter()
                .map(|value| match value {
                    Bson::Document(document) => Ok(document),
                    _ => Err(self.not(index, "an array of documents")),
                })
                .collect(),
            _ => Err(self.not(index, "an array of documents")),
        }
    }

    /// A pipeline, or none.
    fn pipeline(&self, index: usize) -> Result<Vec<Document>, DbError> {
        match self.value(index) {
            None => Ok(Vec::new()),
            Some(_) => self.documents(index),
        }
    }

    fn string(&self, index: usize) -> Result<String, DbError> {
        match self.bson(index)? {
            Some(Bson::String(text)) => Ok(text),
            _ => Err(self.not(index, "a string")),
        }
    }

    fn not(&self, index: usize, what: &str) -> DbError {
        plain_error(format!(
            "{}'s argument {} is not {what}.",
            self.method,
            index + 1
        ))
    }
}

/// A literal as the BSON it spells.
fn bson(value: &Value) -> Result<Bson, DbError> {
    Ok(match value {
        Value::Null => Bson::Null,
        Value::Bool(value) => Bson::Boolean(*value),
        Value::Int32(n) => Bson::Int32(*n),
        Value::Int64(n) => Bson::Int64(*n),
        Value::Double(n) => Bson::Double(*n),
        Value::Decimal128(text) => Bson::Decimal128(
            text.parse()
                .map_err(|_| plain_error(format!("{text} does not fit in a Decimal128.")))?,
        ),
        Value::String(text) => Bson::String(text.clone()),
        Value::ObjectId(bytes) => {
            Bson::ObjectId(bytes.map_or_else(ObjectId::new, ObjectId::from_bytes))
        }
        Value::Date(millis) => Bson::DateTime(DateTime::from_millis(*millis)),
        Value::Binary { subtype, bytes } => Bson::Binary(Binary {
            subtype: (*subtype).into(),
            bytes: bytes.clone(),
        }),
        Value::Timestamp { t, i } => Bson::Timestamp(Timestamp {
            time: *t,
            increment: *i,
        }),
        // BSON stores a regex's options sorted.
        Value::Regex { pattern, flags } => Bson::RegularExpression(Regex {
            pattern: pattern.clone(),
            options: {
                let mut options = flags.chars().collect::<Vec<_>>();
                options.sort_unstable();
                options.into_iter().collect()
            },
        }),
        Value::Code(code) => Bson::JavaScriptCode(code.clone()),
        Value::MinKey => Bson::MinKey,
        Value::MaxKey => Bson::MaxKey,
        Value::Document(fields) => Bson::Document(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), bson(value)?)))
                .collect::<Result<Document, DbError>>()?,
        ),
        Value::Array(values) => Bson::Array(values.iter().map(bson).collect::<Result<_, _>>()?),
    })
}

/// Documents as rows. Every top-level field any of them has is a column, `_id`
/// first and the rest in the order first seen; a document without the field
/// leaves its cell [`MISSING`], which is not the null a document can hold.
fn documents(documents: Vec<Document>) -> QueryResult {
    let mut names: Vec<&str> = Vec::new();
    let mut positions = HashMap::new();
    for document in &documents {
        for name in document.keys() {
            positions.entry(name.as_str()).or_insert_with(|| {
                names.push(name);
                names.len() - 1
            });
        }
    }
    names.sort_by_key(|name| *name != "_id");
    for (at, name) in names.iter().enumerate() {
        positions.insert(name, at);
    }

    let mut rows = Vec::with_capacity(documents.len());
    let mut cell_types = Vec::with_capacity(documents.len());
    let mut bytes = 0;
    for document in &documents {
        let mut row = vec![None; names.len()];
        let mut types = vec![MISSING; names.len()];
        for (name, value) in document {
            let at = positions[name.as_str()];
            row[at] = cell(value);
            types[at] = type_alias(value);
        }
        bytes += row.iter().flatten().map(String::len).sum::<usize>();
        rows.push(row);
        cell_types.push(types);
    }
    let columns = names
        .iter()
        .enumerate()
        .map(|(at, name)| Column {
            name: name.to_string(),
            data_type: Some(shared_type(cell_types.iter().map(|types| types[at])).into()),
        })
        .collect();
    QueryResult {
        columns,
        rows_affected: Some(rows.len() as u64),
        rows,
        cell_types,
        bytes,
        ..QueryResult::default()
    }
}

/// A column's one type when its values agree, else `mixed`. A null or a
/// missing field agrees with anything; a column of nothing else is `null`.
fn shared_type<'a>(types: impl Iterator<Item = &'a str>) -> &'a str {
    let mut present = types.filter(|alias| ![MISSING, "null"].contains(alias));
    match present.next() {
        None => "null",
        Some(first) if present.all(|alias| alias == first) => first,
        Some(_) => "mixed",
    }
}

/// A value as its cell shows it: a scalar in the shell's own spelling, which a
/// statement reads back as the same value, and a document or an array as
/// Relaxed Extended JSON on one line, which the inspector pretty-prints.
fn cell(value: &Bson) -> Cell {
    Some(match value {
        Bson::Null => return None,
        Bson::String(text) => text.clone(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => double(*n),
        Bson::Decimal128(n) => n.to_string(),
        Bson::Boolean(value) => value.to_string(),
        Bson::DateTime(date) => iso_date(*date),
        Bson::ObjectId(id) => format!("ObjectId('{id}')"),
        Bson::Binary(binary) => match (
            u8::from(binary.subtype),
            <[u8; 16]>::try_from(binary.bytes.as_slice()),
        ) {
            (4, Ok(bytes)) => format!("UUID('{}')", Uuid::from_bytes(bytes)),
            (subtype, _) => format!("BinData({subtype}, '{}')", STANDARD.encode(&binary.bytes)),
        },
        Bson::Timestamp(at) => format!("Timestamp({{ t: {}, i: {} }})", at.time, at.increment),
        Bson::RegularExpression(regex) => regex_literal(&regex.pattern, &regex.options),
        Bson::JavaScriptCode(code) => format!("Code({})", single_quoted(code)),
        Bson::MinKey => "MinKey()".into(),
        Bson::MaxKey => "MaxKey()".into(),
        // Deprecated types too, which the shell has no constructor for.
        Bson::Document(_)
        | Bson::Array(_)
        | Bson::JavaScriptCodeWithScope(_)
        | Bson::Symbol(_)
        | Bson::Undefined
        | Bson::DbPointer(_) => value.clone().into_relaxed_extjson().to_string(),
    })
}

/// As JavaScript prints a number, so it reads back as the same one: `2` for
/// a whole double, an exponent past where JavaScript writes one.
fn double(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n != 0.0 && !(1e-6..1e21).contains(&n.abs()) {
        format!("{n:e}")
    } else {
        n.to_string()
    }
}

/// UTC with milliseconds, always three of them. A date past what ISO-8601
/// years spell is shown as the shell's `$date` wrapper instead.
fn iso_date(date: DateTime) -> String {
    const ISO: &[BorrowedFormatItem] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(date.timestamp_millis()) * 1_000_000)
        .ok()
        .and_then(|at| at.format(ISO).ok())
        .unwrap_or_else(|| Bson::DateTime(date).into_relaxed_extjson().to_string())
}

/// A regex as its literal: a `/` in the pattern escaped, as JavaScript's own
/// `source` escapes it, and an empty one as the `(?:)` it prints.
fn regex_literal(pattern: &str, options: &str) -> String {
    let mut source = String::with_capacity(pattern.len());
    let mut escaped = false;
    for c in pattern.chars() {
        if c == '/' && !escaped {
            source.push('\\');
        }
        escaped = c == '\\' && !escaped;
        source.push(c);
    }
    if source.is_empty() {
        source.push_str("(?:)");
    }
    format!("/{source}/{options}")
}

/// A JavaScript single-quoted string.
fn single_quoted(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ConnectionConfig, Engine, SshTunnel};

    fn url(url: &str) -> MongoConfig {
        config_from_url(url).unwrap_or_else(|error| panic!("{url}: {error}"))
    }

    fn mongo(host: &str, port: Option<u16>) -> MongoConfig {
        MongoConfig {
            server: ServerConfig {
                host: host.into(),
                port,
                ..ServerConfig::default()
            },
            ..MongoConfig::default()
        }
    }

    #[test]
    fn a_url_fills_the_fields_without_inventing_a_port_or_a_user() {
        let config = url("mongodb://dbdelve:dbdelve@127.0.0.1:57017/dbdelve_dev");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, Some(57017));
        assert_eq!(config.server.database, "dbdelve_dev");
        assert_eq!(config.server.user, "dbdelve");
        assert_eq!(config.server.password, "dbdelve");
        assert_eq!(config.server.sslmode, SslMode::Prefer);
        assert!(!config.srv);
        assert_eq!(config.options, "");

        let config = url("mongodb://db.example.test");
        assert_eq!(config.server.port, None);
        assert_eq!(config.server.database, "");
        // No credentials at all is an ordinary local server.
        assert_eq!(config.server.user, "");
        assert_eq!(config.server.password, "");

        let config = ConnectionConfig::from_url("mongodb://db.example.test/app").unwrap();
        assert_eq!(config.engine(), Engine::MongoDb);
        assert!(config.server().is_some(), "a server half, for the Keychain");
    }

    #[test]
    fn a_username_holding_an_at_and_a_blank_password_read_back_as_typed() {
        for url in [
            "mongodb://someone%40example.com:@db.example.test/app",
            "mongodb://someone@example.com:@db.example.test/app",
            "mongodb://someone@example.com@db.example.test/app",
        ] {
            let config = config_from_url(url).unwrap();
            assert_eq!(config.server.user, "someone@example.com", "{url}");
            assert_eq!(config.server.password, "", "{url}");
            assert_eq!(config.server.host, "db.example.test", "{url}");
        }
        assert_eq!(
            url("mongodb://u:p%40ss%3Aw%2Frd@h/caf%C3%A9").server,
            ServerConfig {
                host: "h".into(),
                database: "café".into(),
                user: "u".into(),
                password: "p@ss:w/rd".into(),
                ..ServerConfig::default()
            }
        );
    }

    #[test]
    fn a_seed_list_keeps_its_ports_and_its_options_pass_through_as_typed() {
        let config =
            url("mongodb://u:p@h1:27017,[::1]:27018/app?replicaSet=rs0&tls=true&authSource=admin");
        assert_eq!(config.server.host, "h1:27017,[::1]:27018");
        assert_eq!(config.server.port, None);
        assert_eq!(config.options, "replicaSet=rs0&authSource=admin");
        assert_eq!(config.server.sslmode, SslMode::VerifyFull);
    }

    #[test]
    fn an_srv_url_means_verified_tls_unless_it_says_otherwise() {
        let config = url("mongodb+srv://cluster0.example.net/app?retryWrites=true");
        assert!(config.srv);
        assert_eq!(config.server.host, "cluster0.example.net");
        assert_eq!(config.server.port, None);
        assert_eq!(config.server.sslmode, SslMode::VerifyFull);
        assert_eq!(config.options, "retryWrites=true");
        assert_eq!(config.endpoint(), "cluster0.example.net");

        assert_eq!(
            url("mongodb+srv://cluster0.example.net/?tls=false")
                .server
                .sslmode,
            SslMode::Disable
        );
        assert!(config_from_url("mongodb+srv://cluster0.example.net:27017/app").is_err());
    }

    #[test]
    fn the_tls_keys_become_the_mode_and_are_never_passed_through() {
        for (query, sslmode) in [
            ("tls=true", SslMode::VerifyFull),
            ("ssl=true", SslMode::VerifyFull),
            (
                "tls=true&tlsAllowInvalidCertificates=true",
                SslMode::Require,
            ),
            ("tlsInsecure=true", SslMode::Require),
            ("tls=true&tlsAllowInvalidHostnames=true", SslMode::VerifyCa),
            ("tls=false", SslMode::Disable),
        ] {
            let config = url(&format!("mongodb://h/app?{query}"));
            assert_eq!(config.server.sslmode, sslmode, "{query}");
            assert_eq!(config.options, "", "{query}");
        }

        let config = url("mongodb://h/app?tls=true&tlsCAFile=%2Fetc%2Fssl%2Fca.pem");
        assert_eq!(config.server.sslmode, SslMode::VerifyFull);
        assert_eq!(
            config.server.root_certificate.as_deref(),
            Some("/etc/ssl/ca.pem")
        );
        // A root nothing would consult is not kept to look as though it were.
        assert_eq!(
            url("mongodb://h/?tlsInsecure=true&tlsCAFile=/ca.pem")
                .server
                .root_certificate,
            None
        );

        for (query, named) in [
            ("tls=false&tlsCAFile=/ca.pem", "tls=false"),
            ("tlsCertificateKeyFile=/me.pem", "tlsCertificateKeyFile"),
            ("tls=maybe", "tls=maybe"),
            ("bogus=1", "bogus"),
        ] {
            let error = config_from_url(&format!("mongodb://h/app?{query}")).unwrap_err();
            assert!(error.contains(named), "{query}: {error}");
        }
    }

    #[test]
    fn the_connection_string_escapes_the_fields_the_driver_reads_back() {
        let config = MongoConfig {
            server: ServerConfig {
                host: "db.example.test".into(),
                port: Some(27018),
                database: "café".into(),
                user: "someone@example.com".into(),
                password: "p@ss:w/rd%41?#".into(),
                ..ServerConfig::default()
            },
            srv: false,
            options: "?authSource=admin&replicaSet=rs0".into(),
        };
        let written = connection_string(&config).unwrap();
        let read = ConnectionString::parse(&written).unwrap();
        let credential = read.credential.unwrap();
        assert_eq!(credential.username.as_deref(), Some("someone@example.com"));
        assert_eq!(credential.password.as_deref(), Some("p@ss:w/rd%41?#"));
        assert_eq!(credential.source.as_deref(), Some("admin"));
        assert_eq!(read.default_database.as_deref(), Some("café"));
        assert_eq!(read.replica_set.as_deref(), Some("rs0"));
        assert_eq!(
            read.host_info,
            HostInfo::HostIdentifiers(vec![ServerAddress::Tcp {
                host: "db.example.test".into(),
                port: Some(27018),
            }])
        );
    }

    #[test]
    fn the_connection_string_sends_no_credentials_it_was_not_given() {
        assert_eq!(
            connection_string(&mongo("h", None)).unwrap(),
            "mongodb://h/?"
        );
        let mut config = mongo("::1", Some(27017));
        config.server.user = "u".into();
        assert_eq!(
            connection_string(&config).unwrap(),
            "mongodb://u@[::1]:27017/?"
        );
        assert_eq!(
            connection_string(&mongo("localhost:27017", None)).unwrap(),
            "mongodb://localhost:27017/?"
        );
        assert_eq!(
            connection_string(&mongo("h1:1,h2:2", None)).unwrap(),
            "mongodb://h1:1,h2:2/?"
        );
        assert_eq!(
            connection_string(&MongoConfig {
                srv: true,
                ..mongo("cluster0.example.net", None)
            })
            .unwrap(),
            "mongodb+srv://cluster0.example.net/?"
        );
        for config in [
            mongo("h1:1,h2:2", Some(27017)),
            MongoConfig {
                srv: true,
                ..mongo("cluster0.example.net", Some(27017))
            },
            // Anything that would end the host part early names other things.
            mongo("evil@h", None),
            mongo("h/other", None),
            mongo("h?tls=false", None),
            mongo("", None),
        ] {
            assert!(
                connection_string(&config).is_err(),
                "{}",
                config.server.host
            );
        }
    }

    #[test]
    fn options_the_profile_decides_are_refused_by_name() {
        for options in [
            "tls=true",
            "authSource=admin&SSL=false",
            "tlsCAFile=/ca.pem",
        ] {
            assert!(owned_option(options, false).is_some(), "{options}");
        }
        let error = owned_option("directConnection=false", true).unwrap();
        assert!(error.contains("directConnection=false"), "{error}");
        assert_eq!(owned_option("directConnection=false", false), None);
        assert_eq!(owned_option("directConnection=true", true), None);
        assert_eq!(owned_option("authSource=admin&replicaSet=rs0", true), None);
        assert_eq!(owned_option("", true), None);
    }

    #[test]
    fn switching_database_keeps_the_login_where_it_was() {
        let mut config = url("mongodb://u:p@h/dbdelve_dev");
        config.set_database("dbdelve_archive".into());
        assert_eq!(config.server.database, "dbdelve_archive");
        assert_eq!(config.options, "authSource=dbdelve_dev");
        // The second switch finds a source already written and leaves it.
        config.set_database("other".into());
        assert_eq!(config.options, "authSource=dbdelve_dev");

        let mut blank = url("mongodb://u:p@h/?replicaSet=rs0");
        blank.set_database("app".into());
        assert_eq!(blank.options, "replicaSet=rs0&authSource=admin");

        for unpinned in [
            "mongodb://h/dbdelve_dev",
            "mongodb://u:p@h/dbdelve_dev?authSource=admin",
            "mongodb://u@h/dbdelve_dev?authMechanism=MONGODB-X509",
        ] {
            let mut config = url(unpinned);
            let options = config.options.clone();
            config.set_database("app".into());
            assert_eq!(config.options, options, "{unpinned}");
            assert_eq!(config.server.database, "app", "{unpinned}");
        }
        let mut scram = url("mongodb://u:p@h/r%26d?authMechanism=SCRAM-SHA-256");
        scram.set_database("app".into());
        assert_eq!(
            scram.options,
            "authMechanism=SCRAM-SHA-256&authSource=r%26d"
        );
    }

    #[test]
    fn every_rung_gets_the_checks_it_asked_for() {
        let tls_for = |sslmode| {
            tls(&ServerConfig {
                sslmode,
                root_certificate: Some("/etc/ssl/ca.pem".into()),
                ..ServerConfig::default()
            })
        };
        assert_eq!(tls_for(SslMode::Disable), Tls::Disabled);
        for mode in [SslMode::Prefer, SslMode::Require] {
            let Tls::Enabled(options) = tls_for(mode) else {
                panic!("{mode:?} must encrypt");
            };
            assert_eq!(options.allow_invalid_certificates, Some(true), "{mode:?}");
        }
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let Tls::Enabled(options) = tls_for(mode) else {
                panic!("{mode:?} must encrypt");
            };
            assert_eq!(options.allow_invalid_certificates, None, "{mode:?}");
            assert_eq!(
                options.ca_file_path,
                Some(PathBuf::from("/etc/ssl/ca.pem")),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn only_a_handshake_the_server_closed_is_retried_in_plaintext() {
        let io = |kind| Error::from(std::io::Error::from(kind));
        assert!(tls_not_offered(&io(std::io::ErrorKind::UnexpectedEof)));
        assert!(tls_not_offered(&io(std::io::ErrorKind::ConnectionReset)));
        // Refused or unanswered fails the same way in plaintext.
        assert!(!tls_not_offered(&io(std::io::ErrorKind::ConnectionRefused)));
        assert!(!tls_not_offered(&io(std::io::ErrorKind::TimedOut)));
    }

    #[test]
    fn a_refused_connect_names_the_profiles_endpoint() {
        let error = Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert_eq!(
            connect_error(&error, &mongo("db.example.test", Some(27017)), true).message,
            "Connection refused: nothing is listening on db.example.test:27017"
        );
    }

    fn tunnelled(config: MongoConfig, sslmode: SslMode) -> MongoConfig {
        MongoConfig {
            server: ServerConfig {
                sslmode,
                // Nothing that resolves: starting ssh at all would fail
                // differently.
                ssh: Some(SshTunnel {
                    host: "dbdelve-nowhere.invalid".into(),
                    ..SshTunnel::default()
                }),
                ..config.server
            },
            ..config
        }
    }

    #[test]
    fn what_one_forwarded_port_cannot_reach_is_refused_before_ssh_starts() {
        for config in [
            tunnelled(mongo("h1:1,h2:2", None), SslMode::Disable),
            tunnelled(
                MongoConfig {
                    srv: true,
                    ..mongo("cluster0.example.net", None)
                },
                SslMode::Disable,
            ),
        ] {
            let Err(error) = Connection::open(&config) else {
                panic!("{} connected through a tunnel", config.server.host);
            };
            assert!(error.message.contains("seed list"), "{}", error.message);
        }
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let Err(error) = Connection::open(&tunnelled(mongo("db.example.test", None), mode))
            else {
                panic!("{mode:?} through a tunnel connected");
            };
            assert!(
                error.message.starts_with(&format!(
                    "sslmode={} cannot be honoured through an SSH tunnel on MongoDB",
                    mode.as_str()
                )),
                "{}",
                error.message
            );
        }
        let mut owned = tunnelled(mongo("db.example.test", None), SslMode::Disable);
        owned.options = "directConnection=false".into();
        let Err(error) = Connection::open(&owned) else {
            panic!("an indirect connection through a tunnel connected");
        };
        assert!(
            error.message.contains("directConnection"),
            "{}",
            error.message
        );
        assert_eq!(
            unreachable_through_a_tunnel(&mongo("h1:1,h2:2", None)).map(|error| error.message),
            None
        );
    }

    #[test]
    fn the_servers_own_collections_are_not_relations() {
        let listed = |name: &str, kind: &str| relation(&doc! { "name": name, "type": kind });
        assert_eq!(listed("system.views", "collection"), None);
        assert_eq!(listed("system.buckets.sensor_readings", "collection"), None);
        assert_eq!(
            listed("account_overview", "view").map(|relation| relation.kind),
            Some(RelationKind::View)
        );
        for kind in ["collection", "timeseries"] {
            assert_eq!(
                listed("sensor_readings", kind).map(|relation| relation.kind),
                Some(RelationKind::Table),
                "{kind}"
            );
        }
    }

    #[test]
    fn statistics_sum_across_shards_and_a_missing_count_is_unknown() {
        let report = |stats: Document| doc! { "storageStats": stats };
        assert_eq!(
            statistics(&[
                report(doc! { "totalSize": 8192_i32, "count": 3_i64 }),
                report(doc! { "totalSize": 4096.0, "count": 2_i32 }),
            ]),
            Statistics {
                size: Some(12288),
                rows: Some(5),
            }
        );
        // What a time-series collection reports: bytes, and no count.
        assert_eq!(
            statistics(&[report(doc! { "totalSize": 40960_i32 })]),
            Statistics {
                size: Some(40960),
                rows: None,
            }
        );
        assert_eq!(statistics(&[]), Statistics::default());
    }

    #[test]
    fn a_sample_names_each_field_once_with_every_type_it_held() {
        let id = mongodb::bson::oid::ObjectId::new();
        let columns = sampled_columns(&[
            doc! { "name": "Ada", "_id": id, "email": "ada@example.test", "seats": 3_i32 },
            doc! { "_id": id, "name": "Edsger", "email": null, "seats": 4_i64 },
            doc! { "_id": id, "name": "Bruce", "seats": 5.5, "tags": [] },
        ]);
        let shape = columns
            .iter()
            .map(|column| {
                (
                    column.name.as_str(),
                    column.data_type.as_str(),
                    column.nullable,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                ("_id", "objectId", false),
                ("name", "string", false),
                ("email", "string | null", true),
                ("seats", "int | long | double", false),
                // Missing from two of three is as empty a cell as null.
                ("tags", "array", true),
            ]
        );
        assert!(sampled_columns(&[]).is_empty());
    }

    #[test]
    fn an_index_reads_as_its_key_and_what_changes_it() {
        assert_eq!(
            index_definition(&doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" }),
            r#"{"_id":1}"#
        );
        assert_eq!(
            index_definition(&doc! {
                "key": { "external_id": 1, "at": -1 },
                "name": "external_id_1_at_-1",
                "unique": true,
                "sparse": true,
            }),
            r#"{"external_id":1,"at":-1} unique sparse"#
        );
        assert_eq!(
            index_definition(&doc! {
                "key": { "seen": 1 },
                "expireAfterSeconds": 3600_i32,
                "partialFilterExpression": { "active": true },
                "collation": { "locale": "fr" },
            }),
            r#"{"seen":1} TTL 3600s partial {"active":true} collation {"locale":"fr"}"#
        );
    }

    /// What `text`, one statement, reads as: its first argument as BSON.
    fn literal(text: &str) -> Bson {
        let statement = mql::parse(&format!("db.c.insertOne({text})"))
            .unwrap()
            .remove(0);
        let Target::Collection { call, .. } = statement.target else {
            unreachable!()
        };
        bson(&call.args[0].value).unwrap()
    }

    #[test]
    fn a_cell_shows_its_value_as_the_shell_spells_it() {
        let oid = "65a4f1c0ffffffffffffffff";
        for (text, shown, alias) in [
            ("'text'", "text", "string"),
            ("42", "42", "int"),
            (
                "NumberLong('9223372036854775807')",
                "9223372036854775807",
                "long",
            ),
            ("42.5", "42.5", "double"),
            ("Double(2)", "2", "double"),
            ("NumberDecimal('42.50')", "42.50", "decimal"),
            ("true", "true", "bool"),
            (
                "ISODate('2024-01-15T09:30:00Z')",
                "2024-01-15T09:30:00.000Z",
                "date",
            ),
            (
                "ISODate('1815-12-10T00:00:00.5Z')",
                "1815-12-10T00:00:00.500Z",
                "date",
            ),
            (
                &format!("ObjectId('{oid}')"),
                &format!("ObjectId('{oid}')"),
                "objectId",
            ),
            (
                "UUID('018f1f6e-7c2a-7000-8000-0000000000ff')",
                "UUID('018f1f6e-7c2a-7000-8000-0000000000ff')",
                "binData",
            ),
            ("BinData(0, 'AP8=')", "BinData(0, 'AP8=')", "binData"),
            (
                "Timestamp({ t: 1705311000, i: 1 })",
                "Timestamp({ t: 1705311000, i: 1 })",
                "timestamp",
            ),
            ("/^a\\/b.*$/mi", "/^a\\/b.*$/im", "regex"),
            (
                "Code('function () {\\n  return \\'x\\';\\n}')",
                "Code('function () {\\n  return \\'x\\';\\n}')",
                "javascript",
            ),
            ("MinKey()", "MinKey()", "minKey"),
            ("MaxKey()", "MaxKey()", "maxKey"),
            (
                "{ b: ISODate('2024-01-15T09:30:00Z'), a: [1, null, 'x'] }",
                r#"{"b":{"$date":"2024-01-15T09:30:00Z"},"a":[1,null,"x"]}"#,
                "object",
            ),
        ] {
            let value = literal(text);
            assert_eq!(cell(&value).as_deref(), Some(shown), "{text}");
            assert_eq!(type_alias(&value), alias, "{text}");
            // A string, number or date reads back through its column's type,
            // which says which of them its text is; the rest spell their own.
            if !["string", "int", "long", "double", "decimal", "bool", "date"].contains(&alias) {
                assert_eq!(literal(shown), value, "{shown} reads back as {text}");
            }
        }
        assert_eq!(cell(&Bson::Null), None);
        assert_eq!(double(f64::INFINITY), "Infinity");
        assert_eq!(double(1e300), "1e300");
        assert_eq!(double(0.000001), "0.000001");
    }

    #[test]
    fn documents_become_rows_over_the_union_of_their_fields() {
        let result = documents(vec![
            doc! { "name": "Ada", "_id": 1, "email": "ada@example.test" },
            doc! { "_id": 2, "email": null, "seats": 3 },
            doc! { "_id": "three", "seats": 4.5 },
        ]);
        let columns = result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_deref().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            columns,
            [
                ("_id", "mixed"),
                ("name", "string"),
                // A null agrees with any type; it is not one of its own.
                ("email", "string"),
                ("seats", "mixed"),
            ]
        );
        assert_eq!(
            result.rows[1],
            [Some("2".into()), None, None, Some("3".into())]
        );
        // The two empty cells are not the same emptiness.
        assert_eq!(result.cell_types[1], ["int", MISSING, "null", "int"]);
        assert_eq!(result.rows_affected, Some(3));
        assert_eq!(
            documents(vec![doc! { "a": null }]).columns[0]
                .data_type
                .as_deref(),
            Some("null")
        );
    }

    #[test]
    fn an_update_and_a_replacement_are_never_taken_for_each_other() {
        let update = literal("{ $set: { a: 1 } }");
        let replacement = literal("{ a: 1 }");
        let pipeline = Bson::Array(vec![literal("{ $set: { a: 1 } }")]);
        for method in [
            Method::UpdateOne,
            Method::UpdateMany,
            Method::FindOneAndUpdate,
        ] {
            assert!(operators(method, &update).is_ok());
            assert!(operators(method, &pipeline).is_ok());
            assert!(operators(method, &replacement).is_err(), "{method:?}");
            assert!(operators(method, &literal("{}")).is_err(), "{method:?}");
            assert!(operators(method, &literal("{ $set: { a: 1 }, b: 2 }")).is_err());
        }
        for method in [Method::ReplaceOne, Method::FindOneAndReplace] {
            assert!(operators(method, &replacement).is_ok());
            assert!(operators(method, &update).is_err(), "{method:?}");
        }
    }

    #[test]
    fn an_index_nobody_named_is_named_as_the_shell_names_it() {
        assert_eq!(index_name(&doc! { "a": 1, "b": -1 }), "a_1_b_-1");
        assert_eq!(index_name(&doc! { "point": "2dsphere" }), "point_2dsphere");
        let (id, document) = with_id(doc! { "a": 1 });
        assert_eq!(document.keys().next().map(String::as_str), Some("_id"));
        assert_eq!(document.get("_id"), Some(&id));
        assert_eq!(with_id(doc! { "a": 1, "_id": 7 }).0, Bson::Int32(7));
    }

    /// The server the `live_` tests talk to, from `dbdelve_MONGO_URL`.
    fn live_config() -> MongoConfig {
        let url = std::env::var("dbdelve_MONGO_URL").expect("dbdelve_MONGO_URL is required");
        let mut config = config_from_url(&url).expect("dbdelve_MONGO_URL should parse");
        // The compose server speaks no TLS, and these tests are the one place
        // a plaintext connection is the point.
        config.server.sslmode = SslMode::Disable;
        config
    }

    fn live() -> Connection {
        Connection::open(&live_config()).expect("connection should open")
    }

    fn ran(connection: &Connection, statement: &str) -> QueryResult {
        connection
            .query(statement, &CancelToken::default())
            .unwrap_or_else(|error| panic!("{statement}: {}", error.message))
    }

    fn count(connection: &Connection, collection: &str) -> u64 {
        let counted = ran(
            connection,
            &format!("db.getCollection('{collection}').countDocuments()"),
        );
        counted.rows[0][0]
            .as_deref()
            .and_then(|n| n.parse().ok())
            .expect("a count")
    }

    /// The cell `row` holds under the column `name`, and its type.
    fn field<'r>(
        result: &'r QueryResult,
        row: usize,
        name: &str,
    ) -> (Option<&'r str>, &'static str) {
        let at = result
            .columns
            .iter()
            .position(|column| column.name == name)
            .unwrap_or_else(|| panic!("no column {name}: {:?}", result.columns));
        (result.rows[row][at].as_deref(), result.cell_types[row][at])
    }

    fn names(result: &QueryResult) -> Vec<&str> {
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect()
    }

    /// The server operations still listed under any of `comments`.
    fn still_running(connection: &Connection, comments: &[String]) -> Vec<Document> {
        let admin = connection.client().database("admin");
        connection
            .call(async {
                admin
                    .aggregate([
                        doc! { "$currentOp": {} },
                        doc! { "$match": { "command.comment": { "$in": comments } } },
                    ])
                    .await?
                    .try_collect()
                    .await
            })
            .expect("$currentOp should list")
    }

    /// Waits for the server to drop what was stopped: a killed operation ends
    /// at its next check for interruption, not at once.
    fn gone(connection: &Connection, comments: &[String]) -> bool {
        (0..40).any(|_| {
            let empty = still_running(connection, comments).is_empty();
            if !empty {
                std::thread::sleep(Duration::from_millis(50));
            }
            empty
        })
    }

    /// A statement that runs for as long as nothing stops it: a second per
    /// document of a million. Bounded on the server at a minute in case the
    /// test fails to stop it.
    const SLOW: &str = "db.events.find({ $where: 'sleep(1000) || false' }).maxTimeMS(60000)";

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_query_round_trip() {
        let connection = live();
        let accounts = ran(
            &connection,
            "db.accounts.find({ active: true }, { name: 1, plan: 1 })\n  .sort({ name: 1 })\n  .limit(3)",
        );
        assert_eq!(names(&accounts), ["_id", "name", "plan"]);
        assert_eq!(accounts.rows.len(), 3);
        assert_eq!(accounts.rows_affected, Some(3));
        assert_eq!(field(&accounts, 0, "name").0, Some("Ada Lovelace"));
        assert_eq!(accounts.columns[0].data_type.as_deref(), Some("objectId"));

        let counted = ran(&connection, "db.accounts.countDocuments({ active: true })");
        assert_eq!(field(&counted, 0, "count"), (Some("4"), "int"));
        let estimated = ran(&connection, "db.events.estimatedDocumentCount()");
        assert_eq!(field(&estimated, 0, "count").0, Some("1000000"));
        let plans = ran(&connection, "db.accounts.distinct('plan')");
        assert_eq!(names(&plans), ["plan"]);
        assert_eq!(
            plans.rows,
            [["enterprise"], ["free"], ["team"]]
                .map(|row| row.map(|plan| Some(plan.into())).to_vec())
        );
        let one = ran(
            &connection,
            "db.getCollection('accounts').findOne({ plan: 'team' }, { _id: 0, plan: 1 })",
        );
        assert_eq!(one.rows, [[Some("team".to_string())]]);
        let grouped = ran(
            &connection,
            "db.accounts.aggregate([{ $group: { _id: '$plan', n: { $sum: 1 } } }, { $sort: { _id: 1 } }])",
        );
        assert_eq!(names(&grouped), ["_id", "n"]);
        assert_eq!(field(&grouped, 0, "_id").0, Some("enterprise"));
        let archived = ran(
            &connection,
            "db.getSiblingDB('dbdelve_archive').closed_accounts.find()",
        );
        assert_eq!(archived.rows.len(), 2);
        let indexes = ran(&connection, "db.accounts.getIndexes()");
        assert_eq!(
            (0..2)
                .map(|row| field(&indexes, row, "name").0)
                .collect::<Vec<_>>(),
            [Some("_id_"), Some("external_id_1")]
        );

        let collections = ran(&connection, "show collections");
        assert_eq!(names(&collections), ["name"]);
        assert!(collections.rows.contains(&vec![Some("accounts".into())]));
        let databases = ran(&connection, "show dbs");
        assert_eq!(names(&databases), ["name", "sizeOnDisk"]);
        assert_eq!(field(&databases, 1, "name").0, Some("dbdelve_dev"));

        let ping = ran(&connection, "db.adminCommand({ ping: 1 })");
        assert_eq!(field(&ping, 0, "ok").0, Some("1"));
        let plan = ran(
            &connection,
            "db.accounts.find({ plan: 'free' }).explain('executionStats')",
        );
        assert!(
            names(&plan).contains(&"executionStats"),
            "{:?}",
            names(&plan)
        );

        // Two in one submission keep the last one's result.
        let last = ran(
            &connection,
            "db.accounts.countDocuments()\ndb.orders.countDocuments()",
        );
        assert_eq!(field(&last, 0, "count").0, Some("3"));

        let error = connection
            .query("db.accounts.find({ a: })", &CancelToken::default())
            .expect_err("a statement that does not parse is not sent");
        assert!(error.position.is_some(), "{}", error.message);
        let error = connection
            .query(
                "db.accounts.aggregate([]).sort({ a: 1 })",
                &CancelToken::default(),
            )
            .expect_err("aggregate's cursor has no sort");
        assert!(
            error.message.contains("$sort") || error.message.contains("pipeline stage"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_every_seeded_bson_type_renders_as_specified() {
        let result = ran(&live(), "db.bson_types.find()");
        for (name, shown, alias) in [
            ("_id", Some("every-type"), "string"),
            ("double", Some("3.14159"), "double"),
            ("double_whole", Some("2"), "double"),
            ("int32", Some("2147483647"), "int"),
            ("int64", Some("9223372036854775807"), "long"),
            (
                "decimal128",
                Some("1234567890.123456789012345678901234"),
                "decimal",
            ),
            ("string", Some("text"), "string"),
            ("empty_string", Some(""), "string"),
            ("boolean", Some("true"), "bool"),
            ("null", None, "null"),
            (
                "object_id",
                Some("ObjectId('65a4f1c0ffffffffffffffff')"),
                "objectId",
            ),
            ("date", Some("2024-01-15T09:30:00.123Z"), "date"),
            (
                "date_before_epoch",
                Some("1815-12-10T00:00:00.000Z"),
                "date",
            ),
            (
                "timestamp",
                Some("Timestamp({ t: 1705311000, i: 1 })"),
                "timestamp",
            ),
            ("binary_generic", Some("BinData(0, 'AP8=')"), "binData"),
            (
                "binary_uuid",
                Some("UUID('018f1f6e-7c2a-7000-8000-0000000000ff')"),
                "binData",
            ),
            ("regex", Some("/^dbdelve.*$/i"), "regex"),
            (
                "javascript",
                Some("Code('function () { return 1; }')"),
                "javascript",
            ),
            ("min_key", Some("MinKey()"), "minKey"),
            ("max_key", Some("MaxKey()"), "maxKey"),
            ("array", Some(r#"[1,"two",3,null,{"four":4},[5]]"#), "array"),
            ("empty_array", Some("[]"), "array"),
            ("object", Some(r#"{"nested":{"deeper":true}}"#), "object"),
            ("empty_object", Some("{}"), "object"),
        ] {
            assert_eq!(field(&result, 0, name), (shown, alias), "{name}");
            let column = result
                .columns
                .iter()
                .find(|column| column.name == name)
                .unwrap();
            // One document, so every column's type is its one cell's.
            let data_type = if alias == "null" { "null" } else { alias };
            assert_eq!(column.data_type.as_deref(), Some(data_type), "{name}");
        }
        assert!(crate::db::Engine::MongoDb.is_binary_type("binData"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_one_field_of_many_types_is_mixed_and_each_cell_says_its_own() {
        let result = ran(&live(), "db.mixed_shapes.find().sort({ _id: 1 })");
        assert_eq!(names(&result)[..2], ["_id", "value"]);
        let value = &result.columns[1];
        assert_eq!(value.data_type.as_deref(), Some("mixed"));
        let cells = (0..12)
            .map(|row| field(&result, row, "value"))
            .collect::<Vec<_>>();
        assert_eq!(
            cells,
            [
                (Some("a string"), "string"),
                (Some("42"), "int"),
                (Some("42"), "long"),
                (Some("42.5"), "double"),
                (Some("42.50"), "decimal"),
                (Some("true"), "bool"),
                (None, "null"),
                (None, MISSING),
                (Some("[1,2,3]"), "array"),
                (Some(r#"{"nested":"object"}"#), "object"),
                (Some("2024-06-01T00:00:00.000Z"), "date"),
                (Some("ObjectId('65a4f1c0000000000000000c')"), "objectId"),
            ]
        );
        // Columns are the union: a field one document has is a column for all.
        assert_eq!(field(&result, 11, "only_here").0, Some("sparse field"));
        assert_eq!(field(&result, 0, "only_here"), (None, MISSING));
        assert_eq!(field(&result, 12, "with.dot").0, Some("dotted name"));
        assert_eq!(field(&result, 12, "$dollar").0, Some("dollar name"));
        assert_eq!(field(&result, 12, "").0, Some("empty name"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_a_null_email_and_a_missing_one_are_told_apart() {
        let result = ran(&live(), "db.accounts.find({}, { name: 1, email: 1 })");
        let email = |name: &str| {
            let row = (0..result.rows.len())
                .find(|row| field(&result, *row, "name").0 == Some(name))
                .unwrap_or_else(|| panic!("{name} is seeded"));
            field(&result, row, "email")
        };
        assert_eq!(email("Edsger Dijkstra"), (None, "null"));
        assert_eq!(email("李小龍"), (None, MISSING));
        assert_eq!(email("Ada Lovelace"), (Some("ada@example.test"), "string"));
        assert_eq!(
            result
                .columns
                .iter()
                .find(|column| column.name == "email")
                .unwrap()
                .data_type
                .as_deref(),
            Some("string")
        );
    }

    /// A collection of its own for a test that writes, dropped however the
    /// test ends.
    struct Scratch<'a>(&'a Connection, String);

    impl Drop for Scratch<'_> {
        fn drop(&mut self) {
            let _ = self.0.query(
                &format!("db.getCollection('{}').drop()", self.1),
                &CancelToken::default(),
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_writes_report_what_they_did() {
        let connection = live();
        let scratch = Scratch(&connection, format!("dbdelve_test_{}", ObjectId::new()));
        let on = |method: &str| {
            ran(
                &connection,
                &format!("db.getCollection('{}').{method}", scratch.1),
            )
        };
        let refused = |method: &str| {
            connection
                .query(
                    &format!("db.getCollection('{}').{method}", scratch.1),
                    &CancelToken::default(),
                )
                .expect_err(method)
                .message
        };

        let inserted = on("insertOne({ _id: 1, n: 1 })");
        assert_eq!(names(&inserted), ["acknowledged", "insertedId"]);
        assert_eq!(field(&inserted, 0, "insertedId"), (Some("1"), "int"));
        assert_eq!(inserted.rows_affected, Some(1));
        // An `_id` the statement did not give is minted here, so it can be said.
        assert_eq!(
            field(&on("insertOne({ n: 2 })"), 0, "insertedId").1,
            "objectId"
        );
        let many = on("insertMany([{ _id: 3, n: 3 }, { _id: 4, n: 4 }])");
        assert_eq!(field(&many, 0, "insertedIds").0, Some("[3,4]"));
        assert_eq!(many.rows_affected, Some(2));
        let duplicate = refused("insertOne({ _id: 1 })");
        assert!(duplicate.contains("duplicate key"), "{duplicate}");

        let updated = on("updateMany({ n: { $gte: 3 } }, { $inc: { n: 10 } })");
        assert_eq!(
            names(&updated),
            [
                "acknowledged",
                "matchedCount",
                "modifiedCount",
                "upsertedId"
            ]
        );
        assert_eq!(field(&updated, 0, "matchedCount").0, Some("2"));
        assert_eq!(field(&updated, 0, "modifiedCount").0, Some("2"));
        assert_eq!(field(&updated, 0, "upsertedId"), (None, "null"));
        assert_eq!(updated.rows_affected, Some(2));
        let upserted = on("updateOne({ _id: 5 }, { $set: { n: 5 } }, { upsert: true })");
        assert_eq!(field(&upserted, 0, "upsertedId").0, Some("5"));
        assert_eq!(field(&upserted, 0, "matchedCount").0, Some("0"));
        let replacing = refused("updateOne({ _id: 1 }, { n: 9 })");
        assert!(replacing.contains("replaceOne"), "{replacing}");
        assert_eq!(
            field(
                &on("replaceOne({ _id: 1 }, { n: 100 })"),
                0,
                "modifiedCount"
            )
            .0,
            Some("1")
        );
        let found =
            on("findOneAndUpdate({ _id: 1 }, { $set: { n: 101 } }, { returnDocument: 'after' })");
        assert_eq!(field(&found, 0, "n").0, Some("101"));
        assert_eq!(found.rows_affected, Some(1));

        assert_eq!(
            field(&on("createIndex({ n: -1 })"), 0, "name").0,
            Some("n_-1")
        );
        assert_eq!(field(&on("dropIndex('n_-1')"), 0, "ok").0, Some("1"));

        let deleted = on("deleteOne({ _id: 1 })");
        assert_eq!(field(&deleted, 0, "deletedCount").0, Some("1"));
        assert_eq!(deleted.rows_affected, Some(1));
        assert_eq!(
            field(&on("deleteMany({ n: { $gte: 0 } })"), 0, "deletedCount").0,
            Some("4")
        );
        assert_eq!(count(&connection, &scratch.1), 0);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_a_cancel_stops_the_statement_on_the_server_and_the_session_carries_on() {
        let connection = live();
        let run = CancelToken::default();
        let slow = std::thread::spawn({
            let (connection, run) = (connection.clone(), run.clone());
            move || connection.query(SLOW, &run)
        });
        let comments = (0..100)
            .find_map(|_| {
                let comments = run.0.lock().unwrap().handles.clone();
                if comments.is_empty() || still_running(&connection, &comments).is_empty() {
                    std::thread::sleep(Duration::from_millis(50));
                    return None;
                }
                Some(comments)
            })
            .expect("the statement should reach the server");

        let asked = Instant::now();
        connection
            .cancel(&run)
            .expect("the cancel should reach the server");
        let error = slow
            .join()
            .unwrap()
            .expect_err("a cancelled statement fails");
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "{}",
            error.message
        );
        assert!(
            gone(&connection, &comments),
            "the server is still running it"
        );
        assert_eq!(count(&connection, "accounts"), 5);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_the_statement_timeout_stops_a_statement_on_the_server() {
        let mut config = live_config();
        config.server.statement_timeout = 1;
        let connection = Connection::open(&config).expect("connection should open");
        let run = CancelToken::default();
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            let slow =
                scope.spawn(|| connection.query(SLOW.trim_end_matches(".maxTimeMS(60000)"), &run));
            slow.join()
                .unwrap()
                .expect_err("the timeout should stop it")
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("time limit") || error.message.contains("statement timeout"),
            "{}",
            error.message
        );
        assert_eq!(count(&connection, "accounts"), 5);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_the_development_database_is_fully_seeded() {
        // `wide_metrics` fills last, so a complete one means the whole seed ran.
        let connection = live();
        assert_eq!(count(&connection, "wide_metrics"), 25);
        assert_eq!(count(&connection, "events"), 1_000_000);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_the_databases_list_flags_the_one_connected_to() {
        let databases = live().databases().expect("databases should list");
        // The app user holds a role on these two, and listDatabases answers
        // with exactly the ones it holds a role on.
        assert_eq!(databases.names, ["dbdelve_archive", "dbdelve_dev"]);
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_a_blank_database_is_in_none_and_lists_no_collections() {
        let mut config = live_config();
        config.server.database = String::new();
        // The user lives in dbdelve_dev, which was its source only by being
        // the database the URL named.
        config.options = "authSource=dbdelve_dev".into();
        let connection = Connection::open(&config).expect("a blank database should connect");

        let databases = connection.databases().expect("databases should list");
        assert_eq!(databases.current, None);
        assert!(databases.names.contains(&"dbdelve_dev".to_string()));
        assert_eq!(connection.catalog().unwrap(), Catalog::default());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_a_switch_to_a_database_the_user_is_not_defined_in_still_logs_in() {
        let mut config = ConnectionConfig::MongoDb(live_config());
        config.set_database("dbdelve_archive".into());
        let ConnectionConfig::MongoDb(config) = config else {
            unreachable!()
        };
        let catalog = Connection::open(&config)
            .expect("the switched profile should log in")
            .catalog()
            .expect("catalog should load");
        assert_eq!(catalog.schemas[0].name, "dbdelve_archive");
        assert!(
            catalog.schemas[0]
                .relations
                .iter()
                .any(|relation| relation.name == "closed_accounts")
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_catalog_round_trip() {
        let catalog = live().catalog().expect("catalog should load");
        let [schema] = catalog.schemas.as_slice() else {
            panic!("one schema, the connected database: {catalog:?}");
        };
        assert_eq!(schema.name, "dbdelve_dev");
        assert!(schema.routines.is_empty());
        let kind = |name: &str| {
            schema
                .relations
                .iter()
                .find(|relation| relation.name == name)
                .map(|relation| relation.kind)
        };
        assert_eq!(kind("accounts"), Some(RelationKind::Table));
        assert_eq!(kind("sensor_readings"), Some(RelationKind::Table));
        assert_eq!(kind("account_overview"), Some(RelationKind::View));
        // Another database's collection is that database's to list.
        assert_eq!(kind("closed_accounts"), None);
        assert!(
            schema
                .relations
                .iter()
                .all(|relation| !relation.name.starts_with("system.")),
            "{:?}",
            schema.relations
        );
        let names = schema
            .relations
            .iter()
            .map(|relation| relation.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.is_sorted(), "{names:?}");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_sizes_land_on_collections_and_not_on_views() {
        let sizes = live().sizes().expect("sizes should load");
        let sizes = &sizes["dbdelve_dev"];
        let events = sizes["events"];
        assert_eq!(events.rows, Some(1_000_000));
        assert!(events.size.is_some_and(|size| size > 0), "{events:?}");
        assert!(sizes["sensor_readings"].size.is_some());
        assert_eq!(sizes["sensor_readings"].rows, None);
        assert!(!sizes.contains_key("account_overview"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_structure_round_trip() {
        let connection = live();
        let accounts = connection
            .structure("dbdelve_dev", "accounts")
            .expect("structure should load");
        let column = |structure: &Structure, name: &str| {
            structure
                .columns
                .iter()
                .find(|column| column.name == name)
                .map(|column| (column.data_type.clone(), column.nullable))
                .unwrap_or_else(|| panic!("{name} is sampled: {:?}", structure.columns))
        };
        assert_eq!(accounts.columns[0].name, "_id");
        assert_eq!(column(&accounts, "_id"), ("objectId".into(), false));
        // Dijkstra's is null and Bruce Lee has none.
        assert_eq!(column(&accounts, "email"), ("string | null".into(), true));
        assert_eq!(column(&accounts, "balance"), ("decimal".into(), false));
        assert_eq!(accounts.row_key(), ["_id"]);
        assert_eq!(accounts.primary_key(), ["_id"]);
        let index = |name: &str| {
            accounts
                .indexes
                .iter()
                .find(|index| index.name == name)
                .map(|index| index.definition.as_str())
        };
        assert_eq!(index("_id_"), Some(r#"{"_id":1}"#));
        assert_eq!(index("external_id_1"), Some(r#"{"external_id":1} unique"#));

        let mixed = connection
            .structure("dbdelve_dev", "mixed_shapes")
            .expect("structure should load");
        assert_eq!(
            column(&mixed, "value"),
            (
                [
                    "objectId", "string", "int", "long", "double", "decimal", "bool", "date",
                    "object", "array", "null",
                ]
                .join(" | "),
                true
            )
        );

        let view = connection
            .structure("dbdelve_dev", "account_overview")
            .expect("a view's structure should load");
        assert!(view.row_key().is_empty(), "{:?}", view.constraints);
        assert!(view.indexes.is_empty());
        assert_eq!(column(&view, "_id"), ("string".into(), false));

        let readings = connection
            .structure("dbdelve_dev", "sensor_readings")
            .expect("a time-series collection's structure should load");
        assert_eq!(readings.row_key(), ["_id"]);
        assert_eq!(column(&readings, "recorded_at"), ("date".into(), false));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_only_the_modes_the_server_can_satisfy_connect() {
        let connect = |sslmode| {
            let mut config = live_config();
            config.server.sslmode = sslmode;
            Connection::open(&config)
        };
        // `prefer` against a server with no TLS is plaintext, and only because
        // the server closed the TLS hello unanswered.
        for mode in [SslMode::Disable, SslMode::Prefer] {
            assert!(connect(mode).is_ok(), "{mode:?} should connect");
        }
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let started = std::time::Instant::now();
            let Err(error) = connect(mode) else {
                panic!("{mode:?} must not connect in plaintext");
            };
            assert!(
                error.message.contains("TLS handshake"),
                "{mode:?} failed without saying why: {}",
                error.message
            );
            // The first failed check of the one server, not the selection
            // timeout.
            assert!(
                started.elapsed() < Duration::from_secs(CONNECT_TIMEOUT_SECONDS / 2),
                "{mode:?}"
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MONGO_URL"]
    fn live_a_wrong_password_fails_the_connect_in_the_servers_words() {
        let mut config = live_config();
        config.server.password = "not the password".into();
        let Err(error) = Connection::open(&config) else {
            panic!("a wrong password connected");
        };
        assert!(
            error.message.contains("Authentication failed"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the dev bastions and MongoDB configured through dbdelve_SSH_CONFIG and dbdelve_MONGO_URL"]
    fn live_ssh_the_catalog_loads_through_the_bastion() {
        // The compose server as the bastion sees it, by service name and the
        // port inside the network.
        let mut config = live_config();
        config.server.host = "mongo".into();
        config.server.port = Some(27017);
        config.server.ssh = crate::db::ssh::live_bastion("dbdelve-bastion");
        let connection = Connection::open(&config).expect("connection should open");

        let catalog = connection.catalog().expect("catalog should load");
        assert!(
            catalog.schemas[0]
                .relations
                .iter()
                .any(|relation| relation.name == "accounts")
        );
        assert_eq!(count(&connection, "wide_metrics"), 25);
    }
}

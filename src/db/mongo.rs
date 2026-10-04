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
//! engines with one socket.

use std::future::IntoFuture;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::error::{Error, ErrorKind};
use mongodb::event::{EventHandler, sdam::SdamEvent};
use mongodb::options::{ClientOptions, ConnectionString, HostInfo, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, Database};
use percent_encoding::{AsciiSet, CONTROLS, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use super::ssh::{Tunnel, tunnelled};
use super::{
    Catalog, ColumnDefinition, DbError, NamedDefinition, QueryResult, Relation, RelationKind,
    Schema, ServerConfig, Sizes, SslMode, Statistics, Structure, plain_error,
};

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

    pub fn query(&self, _statement: &str) -> Result<QueryResult, DbError> {
        Err(plain_error(
            "Running statements on MongoDB is not wired yet.".into(),
        ))
    }

    pub fn cancel(&self) -> Result<(), DbError> {
        Ok(())
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

    /// Statements do not run yet, so the count goes to the driver directly.
    fn count(connection: &Connection, collection: &str) -> u64 {
        connection
            .call(
                connection
                    .database()
                    .collection::<Document>(collection)
                    .count_documents(doc! {}),
            )
            .expect("count should succeed")
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

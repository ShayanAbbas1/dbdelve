//! A connection through an engine plugin: one child process per connection,
//! spoken to in `dbdelve-plugin-protocol` frames.
//!
//! A plugin that dies or breaks the protocol loses its connection, which is
//! reported as a reset and never reopened or replayed here: the workspace's own
//! reconnect opens a fresh one and re-applies the Read-only hold before
//! anything else runs on it.

#[cfg(test)]
mod db2;
#[cfg(test)]
pub(crate) mod echo;
mod edit;
mod host;
#[cfg(test)]
mod psqlodbc;
mod rows;

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use dbdelve_plugin_protocol::arrow::{decode_batch, decode_schema};
use dbdelve_plugin_protocol::limits::{
    CANCEL_DEADLINE, HELLO_DEADLINE, OPEN_DEADLINE, credit_cost,
};
use dbdelve_plugin_protocol::{
    ErrorKind, PROTOCOL, Password, REFERENCES, RelationKind as WireKind, Request, Response,
    Structure as WireStructure, WRITE,
};

use super::odbc_string::{self, Refusal};
use super::plugin::Plugin;
use super::ssh::Tunnel;
use super::{
    CancelToken, Cancelling, Catalog, ColumnDefinition, DbError, Engine, Fetch, ForeignKey, Index,
    IndexKind, NamedDefinition, QueryResult, Reference, Relation, RelationKind, Schema, SshTunnel,
    Stopped, Structure,
};
use crate::sql;
use host::{Host, Routed, failure};
use rows::{Rows, Said};

pub const CANCELLED: &str = "Cancelled.";

/// The dialects whose rows a plugin driver may write back: those with a
/// conformance subject passing `generated_writes_round_trip` through a real
/// driver (Postgres through psqlODBC, Generic SQL through Db2's driver and
/// psqlODBC). Every other dialect is read-only through a plugin, whatever it
/// can do built in, until a subject proves it. The driver earns the rest per
/// connection: the plugin offers `write`, and marks the key columns it reads
/// back exactly.
pub(super) fn writes_rows(engine: Engine) -> bool {
    matches!(engine, Engine::Generic | Engine::Postgres)
}

/// Why rows read through a plugin speaking `engine` are never written back,
/// when they are not.
pub(super) fn unwritable(engine: Engine) -> Option<String> {
    (!writes_rows(engine)).then(|| {
        format!(
            "Editing rows through a plugin driver is not supported for {} yet, so these rows \
             are read-only.",
            engine.label()
        )
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExternalConfig {
    pub plugin: &'static Plugin,
    /// The built-in dialect DBDelve writes SQL in for this connection.
    pub speaks: Engine,
    /// As the user typed it, and as it is stored. Through a tunnel the
    /// plugin is handed a copy naming the forward instead, which exists only
    /// in the `open` request.
    pub connection: String,
    pub user: String,
    pub password: String,
    /// Seconds, 0 for none.
    pub statement_timeout: u32,
    pub ssh: Option<SshTunnel>,
}

/// By hand, so no `{:?}` prints the password, nor the connection string, which
/// can carry one of its own (`PWD=`).
impl fmt::Debug for ExternalConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalConfig")
            .field("plugin", &self.plugin.id)
            .field("speaks", &self.speaks)
            .field("user", &self.user)
            .field("statement_timeout", &self.statement_timeout)
            .field("ssh", &self.ssh)
            .finish_non_exhaustive()
    }
}

/// What the plugin said about the connection it opened.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Opened {
    pub(super) dbms_name: String,
    pub(super) dbms_version: String,
    pub(super) driver: String,
    pub(super) stops_early: bool,
    /// The optional messages both `hello` and `opened` listed: the only ones
    /// this connection ever sends.
    pub(super) optional: Vec<String>,
}

#[derive(Clone)]
pub struct Connection {
    host: Arc<Host>,
    opened: Arc<Opened>,
    /// What [`Self::apart`] opens again.
    config: Arc<ExternalConfig>,
    /// Lives while any clone does; [`Self::apart`] dials it again.
    tunnel: Option<Arc<Tunnel>>,
}

impl Connection {
    /// Refuses a dialect a plugin may not carry, and a string a tunnel cannot
    /// be sure of, before ssh or the plugin starts, for a config that reached
    /// here by any path, a hand-edited profile's included.
    pub fn open(config: &ExternalConfig) -> Result<Self, DbError> {
        if !config.speaks.speaks_through_plugins() {
            return Err(failure(format!(
                "{}'s SQL cannot be spoken through a plugin driver.",
                config.speaks.label()
            )));
        }
        // Refused whatever the mode, since the mode can change once connected.
        if config.speaks.holds_read_only() && odbc_string::reconnects_silently(&config.connection) {
            return Err(failure(format!(
                "AUTO_RECONNECT lets the driver open a new session without saying so, which would \
                 drop the Read-only hold DBDelve puts on a {} session. Remove it to connect.",
                config.speaks.label()
            )));
        }
        let forward = match &config.ssh {
            Some(ssh) => Some((
                ssh,
                odbc_string::endpoint(&config.connection).map_err(refused)?,
            )),
            None => None,
        };
        let tunnel = || {
            forward
                .map(|(ssh, endpoint)| {
                    Tunnel::open(ssh, &endpoint.host, endpoint.port).map(Arc::new)
                })
                .transpose()
        };
        Self::handshake(Host::start(config.plugin)?, config, tunnel)
    }

    /// A session of its own: a second process on the same config, through
    /// this one's tunnel.
    pub fn apart(&self) -> Result<Self, DbError> {
        let tunnel = || Ok(self.tunnel.clone());
        Self::handshake(Host::start(self.config.plugin)?, &self.config, tunnel)
    }

    pub fn speaks(&self) -> Engine {
        self.config.speaks
    }

    /// The dialect's own session statement, run through the plugin as any
    /// built-in runs it. A hold that fails to land loses the session for
    /// good: the gate counts on it for a statement it cannot read, so this
    /// session must run nothing more, and the workspace's reconnect either
    /// lands the hold or fails the connection. A Generic session holds
    /// nothing and sends nothing, which the gate already allows for.
    pub fn set_read_only(&self, read_only: bool) -> Result<(), DbError> {
        let Some(statement) = super::read_only_statement(self.config.speaks, read_only) else {
            return Ok(());
        };
        // Held across the loss too, so no statement waiting on the session
        // runs between a hold that failed and the session being given up.
        let _session = self.host.session();
        let held = self.query_held(statement, &CancelToken::default(), Fetch::default());
        if read_only && held.is_err() {
            self.host.lose_session();
        }
        held.map(|_| ())
    }

    /// `tunnel` is opened only once the plugin has answered `prepare`, so a
    /// plugin that cannot start fails without an ssh session first.
    fn handshake(
        host: Arc<Host>,
        config: &ExternalConfig,
        tunnel: impl FnOnce() -> Result<Option<Arc<Tunnel>>, DbError>,
    ) -> Result<Self, DbError> {
        let name = &host.plugin.name;
        let hello = Request::Hello { protocol: PROTOCOL };
        let Response::Hello {
            protocol, optional, ..
        } = ask(&host, hello, "hello", HELLO_DEADLINE)?
        else {
            return Err(unexpected(&host, "hello"));
        };
        if protocol != PROTOCOL {
            return Err(failure(format!(
                "The {name} plugin speaks protocol {protocol}; this DBDelve speaks {PROTOCOL}."
            )));
        }

        let fields = BTreeMap::from([
            ("connection".to_owned(), config.connection.clone()),
            ("user".to_owned(), config.user.clone()),
        ]);
        let prepare = Request::Prepare {
            fields: fields.clone(),
        };
        let Response::Prepared { .. } = ask(&host, prepare, "prepare", HELLO_DEADLINE)? else {
            return Err(unexpected(&host, "prepare"));
        };

        let tunnel = tunnel()?;
        let opened = || {
            let forward = tunnel.as_deref().map(Tunnel::dial).transpose()?;
            let mut fields = fields;
            fields.insert("connection".to_owned(), dialled(config, forward)?);
            let open = Request::Open {
                fields,
                password: Password(config.password.clone()),
                statement_timeout: config.statement_timeout,
            };
            ask(&host, open, "open", OPEN_DEADLINE)
        };
        let answer = match &tunnel {
            Some(tunnel) => opened().map_err(|error| tunnel.explain(error)),
            None => opened(),
        };
        let Response::Opened {
            dbms_name,
            dbms_version,
            driver,
            stops_early,
            optional: offered,
        } = answer?
        else {
            return Err(unexpected(&host, "open"));
        };
        let opened = Opened {
            dbms_name,
            dbms_version,
            driver,
            stops_early,
            optional: offered
                .into_iter()
                .filter(|message| optional.contains(message))
                .collect(),
        };
        Ok(Self {
            host,
            opened: Arc::new(opened),
            config: Arc::new(config.clone()),
            tunnel,
        })
    }

    pub(super) fn opened(&self) -> &Opened {
        &self.opened
    }

    pub fn is_lost(&self) -> bool {
        self.host.is_lost()
    }

    /// Whether a read stopped at the row limit can end early here, rather
    /// than always being drained to its end by the plugin.
    pub fn stops_at_limit(&self) -> bool {
        self.opened.stops_early
    }

    /// Runs one statement as typed, keeping at most `fetch.limit` rows. Past
    /// the limit the plugin is told to `stop`: it sends no more rows and
    /// either ends the statement, when `eligible` and provably safe, or drains
    /// it on its own side. A plugin that dies on the way is a reset, and
    /// nothing here sends the statement again.
    ///
    /// Every statement queues on the one process, so `cancel` is what tells
    /// this statement from the ones beside it: a Cancel under it that came
    /// while it waited for the session drops it unsent, and one that comes
    /// once it is sent stops it and nothing else.
    pub fn query(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        let read = {
            let _session = self.host.session();
            self.read_held(sql, cancel, fetch)?
        };
        Ok(self.editable(sql, read))
    }

    /// A statement DBDelve wrote: a preview, count or lookup runs as
    /// [`Self::query`] does, and a batch of the grid's writes as one
    /// [`Request::Write`] ([`Self::write`]), the transaction its brackets
    /// asked for being the driver's own rather than their text.
    pub fn generated(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        match sql::generated_writes(self.config.speaks, sql) {
            Some(statements) => self.write(statements, cancel),
            None => self.query(sql, cancel, fetch),
        }
    }

    /// Whether the plugin runs DBDelve's writes, all or none, through its
    /// driver's own transaction: only then are its rows offered for editing.
    fn writes(&self) -> bool {
        self.opened.optional.iter().any(|name| name == WRITE)
    }

    /// `statements` as one transaction of the driver's, committed only if
    /// each changed exactly one row. Sent like a query, so a Cancel under
    /// `cancel` reaches it, and refused unsent by a plugin that did not offer
    /// `write` or speaks a dialect DBDelve does not write through a plugin,
    /// and while the user's own statements have left a transaction open:
    /// the driver's commit or rollback would end theirs with it.
    fn write(&self, statements: Vec<String>, cancel: &CancelToken) -> Result<QueryResult, DbError> {
        if let Some(reason) = unwritable(self.config.speaks) {
            return Err(failure(reason));
        }
        if !self.writes() {
            return Err(failure(format!(
                "This version of the {} plugin cannot save edits, so nothing was sent.",
                self.host.plugin.name
            )));
        }
        let _session = self.host.session();
        if self.host.in_transaction.load(Ordering::Relaxed) {
            return Err(failure(
                "Commit or roll back the open transaction first: an edit through a plugin driver \
                 runs as a transaction of its own, so nothing was sent."
                    .into(),
            ));
        }
        let started = Instant::now();
        let (result, _) = self.send(
            Request::Write { statements },
            None,
            cancel,
            Fetch::default(),
            started,
        )?;
        Ok(result)
    }

    /// The edit target of the rows `sql` read, or why it has none, put on
    /// the result. Asked of the plugin only for a dialect DBDelve writes
    /// through a plugin, on a connection whose plugin offered `write`.
    fn editable(&self, sql: &str, read: (QueryResult, Vec<Said>)) -> QueryResult {
        let (mut result, said) = read;
        if result.columns.is_empty() {
            return result;
        }
        let speaks = self.config.speaks;
        if let Some(reason) = unwritable(speaks) {
            result.read_only_reason = Some(reason);
            return result;
        }
        if !self.writes() {
            result.read_only_reason = Some(format!(
                "This version of the {} plugin cannot save edits, so these rows are read-only.",
                self.host.plugin.name
            ));
            return result;
        }
        let describe = |schema: &str, relation: &str| self.described(schema, relation);
        (result.edit, result.read_only_reason) =
            edit::target(speaks, sql, &result.columns, &said, describe);
        result
    }

    /// [`Self::query`] with the session already held.
    fn query_held(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        self.read_held(sql, cancel, fetch).map(|(result, _)| result)
    }

    /// [`Self::query_held`], with what the plugin said of each of the
    /// result's columns.
    fn read_held(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<(QueryResult, Vec<Said>), DbError> {
        fetch.hold()?;
        let started = Instant::now();
        let eligible = fetch.reads_only && fetch.limit.is_some() && self.opened.stops_early;
        let query = Request::Query {
            sql: sql.to_owned(),
            eligible,
        };
        let leaves = sql::leaves(self.config.speaks, sql).transaction;
        self.send(query, leaves, cancel, fetch, started)
    }

    /// A query or write, sent and read under `cancel`, with the session
    /// already held. `leaves` is whether the request leaves a transaction
    /// open, where it says.
    fn send(
        &self,
        request: Request,
        leaves: Option<bool>,
        cancel: &CancelToken,
        fetch: Fetch,
        started: Instant,
    ) -> Result<(QueryResult, Vec<Said>), DbError> {
        let eligible = matches!(request, Request::Query { eligible: true, .. });
        // Sent and claimed under the run's lock, so a Cancel either finds the
        // run asked before anything went out, or finds the id with its request
        // already queued ahead of the cancel's frame.
        let (in_flight, answers) = {
            let mut run = claims(cancel);
            if run.asked {
                return Err(failure("Cancelled before it was sent.".into()));
            }
            let sent = self.host.request(request)?;
            // Opening is marked once sent, ending only once answered (in
            // `read`): a request sent is only queued, and a plugin answers
            // one cancelled before it ran without running it, so a COMMIT
            // cancelled unsent or unrun leaves the user's transaction open.
            if leaves == Some(true) {
                self.host.in_transaction.store(true, Ordering::Relaxed);
            }
            run.handles.push(sent.0.id.to_string());
            sent
        };
        let _claim = Claim {
            cancel,
            handle: in_flight.id.to_string(),
        };
        let ends = leaves == Some(false);
        self.read(
            in_flight.id,
            &answers,
            Read::new(fetch, eligible, started),
            ends,
        )
    }

    /// The user's Cancel, which may abort an open transaction, as cancelling
    /// a built-in's statement does. It stops only the statements `cancel`
    /// sent; one still waiting for the session is dropped when its turn
    /// comes. A catalog request is no run's, so no Cancel reaches it.
    pub fn cancel(&self, cancel: &CancelToken) -> Result<(), DbError> {
        let mut run = claims(cancel);
        run.asked = true;
        for id in run.handles.iter().filter_map(|handle| handle.parse().ok()) {
            self.host.cancel(id);
        }
        Ok(())
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let Response::Catalog { schemas } = self.call(Request::Catalog)? else {
            return Err(self
                .host
                .violated("an answer to `catalog` that is not `catalog`", false));
        };
        let schemas = schemas
            .into_iter()
            .map(|schema| Schema {
                name: schema.name,
                relations: schema
                    .relations
                    .into_iter()
                    .map(|relation| Relation {
                        name: relation.name,
                        kind: match relation.kind {
                            WireKind::Table => RelationKind::Table,
                            WireKind::View => RelationKind::View,
                            WireKind::MaterializedView => RelationKind::MaterializedView,
                        },
                        partition_of: None,
                        size: None,
                        rows: None,
                    })
                    .collect(),
                routines: Vec::new(),
            })
            .collect();
        Ok(Catalog {
            schemas,
            database: None,
        })
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        self.described(schema, relation).map(structure_from)
    }

    /// The plugin's own answer to `structure`, its key's names as the
    /// catalog holds them.
    fn described(&self, schema: &str, relation: &str) -> Result<WireStructure, DbError> {
        let request = Request::Structure {
            schema: schema.to_owned(),
            relation: relation.to_owned(),
        };
        match self.call(request)? {
            Response::Structure(wire) => Ok(wire),
            _ => Err(self
                .host
                .violated("an answer to `structure` that is not `structure`", false)),
        }
    }

    /// Empty when the plugin did not offer `references`: nothing is sent it
    /// never listed.
    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        if !self.opened.optional.iter().any(|name| name == REFERENCES) {
            return Ok(Vec::new());
        }
        let request = Request::References {
            schema: schema.to_owned(),
            relation: relation.to_owned(),
        };
        match self.call(request)? {
            Response::References { references } => Ok(references
                .into_iter()
                .map(|wire| Reference {
                    schema: wire.schema,
                    table: wire.table,
                    column: wire.column,
                    referenced_column: wire.referenced_column,
                })
                .collect()),
            _ => Err(self
                .host
                .violated("an answer to `references` that is not `references`", false)),
        }
    }

    /// One non-query request, under the session like a statement, and
    /// bounded by [`catalog_deadline`], since no Cancel reaches it: once while
    /// it waits for the session, which a statement may hold for as long as
    /// the server takes, and once for the plugin's answer. The answer gets
    /// the whole deadline rather than what the wait left of it, because past
    /// it the plugin is ended and the session reset, which a request that
    /// waited its turn has done nothing to deserve.
    fn call(&self, request: Request) -> Result<Response, DbError> {
        let what = match &request {
            Request::Catalog => "catalog",
            Request::Structure { .. } => "structure",
            Request::References { .. } => "references",
            _ => "a request",
        };
        let deadline = catalog_deadline(self.config.statement_timeout);
        let Some(_session) = self.host.session_by(Instant::now() + deadline) else {
            return Err(failure(format!(
                "`{what}` waited {} s for the {} plugin's session, busy with another \
                 statement, and was not sent.",
                deadline.as_secs(),
                self.host.plugin.name
            )));
        };
        let (_in_flight, answers) = self.host.request(request)?;
        match answers.recv_timeout(deadline) {
            Ok(Routed::Control(Response::Error {
                kind,
                message,
                position,
                lost,
                ..
            })) => Err(self.plugin_error(kind, message, position, lost)),
            Ok(Routed::Control(response)) => Ok(response),
            Ok(Routed::Data(_)) => Err(self
                .host
                .violated("data in answer to a catalog request", false)),
            Ok(Routed::Reset(_)) | Err(RecvTimeoutError::Disconnected) => {
                Err(self.host.reset_error(false))
            }
            Err(RecvTimeoutError::Timeout) => Err(self.host.end(
                format!(
                    "The {} plugin did not answer `{what}` within {} s and was stopped.",
                    self.host.plugin.name,
                    deadline.as_secs()
                ),
                false,
            )),
        }
    }

    fn plugin_error(
        &self,
        kind: ErrorKind,
        message: String,
        position: Option<u32>,
        lost: bool,
    ) -> DbError {
        if lost {
            self.host.lose_session();
        }
        match kind {
            ErrorKind::Cancelled => failure(CANCELLED.to_owned()),
            _ => DbError {
                message,
                position: position.and_then(|at| usize::try_from(at).ok()),
            },
        }
    }

    /// `ends` clears the open transaction once the statement has run: on its
    /// `end`, or on a server's error, since a server ends a transaction whose
    /// COMMIT fails. Every other way out leaves it marked.
    fn read(
        &self,
        id: u64,
        answers: &Receiver<Routed>,
        mut read: Read<'_>,
        ends: bool,
    ) -> Result<(QueryResult, Vec<Said>), DbError> {
        loop {
            let Ok(routed) = answers.recv() else {
                return Err(self.host.reset_error(true));
            };
            match routed {
                Routed::Data(payload) => {
                    read.data(&payload)
                        .map_err(|violation| self.host.violated(&violation, true))?;
                    if read.stop_due() {
                        self.host.send(id, Request::Stop);
                    }
                    // Only now, with every kept row handed on: a feed whose
                    // taker has stopped taking holds this back, and the plugin
                    // stops sending without the reader ever pausing.
                    let bytes = credit_cost(payload.len());
                    self.host.send(id, Request::Credit { bytes });
                }
                Routed::Control(Response::End {
                    rows,
                    rows_affected,
                    stopped,
                    messages,
                    more_sets,
                }) => {
                    if ends {
                        self.host.in_transaction.store(false, Ordering::Relaxed);
                    }
                    let end = End {
                        rows,
                        rows_affected,
                        stopped,
                        messages,
                        more_sets,
                    };
                    return read
                        .finish(end)
                        .map_err(|violation| self.host.violated(&violation, true));
                }
                Routed::Control(Response::Error {
                    kind,
                    message,
                    position,
                    lost,
                    ..
                }) => {
                    if ends && kind == ErrorKind::Server && !lost {
                        self.host.in_transaction.store(false, Ordering::Relaxed);
                    }
                    return Err(self.plugin_error(kind, message, position, lost));
                }
                Routed::Control(_) => {
                    return Err(self.host.violated(
                        "an answer to a query that is neither `end` nor `error`",
                        true,
                    ));
                }
                Routed::Reset(_) => return Err(self.host.reset_error(true)),
            }
        }
    }
}

/// The string the plugin is to dial: the user's own without a tunnel, and with
/// one the same string pointed at `forward`, checked to name nothing else.
fn dialled(config: &ExternalConfig, forward: Option<SocketAddr>) -> Result<String, DbError> {
    match forward {
        None => Ok(config.connection.clone()),
        Some(local) => odbc_string::through_tunnel(&config.connection, local).map_err(refused),
    }
}

fn refused(Refusal(sentence): Refusal) -> DbError {
    failure(sentence)
}

fn claims(cancel: &CancelToken) -> MutexGuard<'_, Cancelling> {
    cancel.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A statement's request id on its run's token until it returns, unwinding
/// included.
struct Claim<'a> {
    cancel: &'a CancelToken,
    handle: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        claims(self.cancel)
            .handles
            .retain(|handle| *handle != self.handle);
    }
}

/// The key goes into `constraints` in the `PRIMARY KEY (a, b)` rendering
/// `Structure::primary_key` and `row_key` read back. Names are written bare,
/// since the plugin's dialect is not known here; a column whose name holds `, `
/// cannot be told from two, so it reads back as no key and the grid stays
/// read-only rather than guess.
fn structure_from(wire: WireStructure) -> Structure {
    let constraints = match wire.primary_key.is_empty() {
        true => Vec::new(),
        false => vec![NamedDefinition {
            name: "PRIMARY KEY".into(),
            definition: format!("PRIMARY KEY ({})", wire.primary_key.join(", ")),
        }],
    };
    Structure {
        columns: wire
            .columns
            .into_iter()
            .map(|column| ColumnDefinition {
                name: column.name,
                data_type: column.data_type,
                nullable: column.nullable,
                default: column.default,
            })
            .collect(),
        indexes: wire
            .indexes
            .into_iter()
            .map(|index| Index {
                kind: match (index.primary, index.unique) {
                    (true, _) => IndexKind::Primary,
                    (false, true) => IndexKind::Unique,
                    (false, false) => IndexKind::Plain,
                },
                definition: format!(
                    "{}INDEX {} ({})",
                    if index.unique { "UNIQUE " } else { "" },
                    index.name,
                    index.columns.join(", ")
                ),
                name: index.name,
                columns: index.columns,
                predicate: None,
            })
            .collect(),
        constraints,
        foreign_keys: wire
            .foreign_keys
            .into_iter()
            .map(|key| ForeignKey {
                column: key.column,
                referenced_schema: key.referenced_schema,
                referenced_table: key.referenced_table,
                referenced_column: key.referenced_column,
            })
            .collect(),
        referenced_by: Vec::new(),
    }
}

/// A query's `end`, as the read takes it.
struct End {
    rows: u64,
    rows_affected: Option<u64>,
    stopped: bool,
    messages: Vec<String>,
    more_sets: u32,
}

/// One query's rows as they arrive, kept up to the limit.
struct Read<'a> {
    fetch: Fetch<'a>,
    eligible: bool,
    started: Instant,
    set: Option<(SchemaRef, Rows)>,
    result: QueryResult,
    kept: usize,
    /// Rows in the batches decoded, kept or not: what `end` may not undercount.
    seen: u64,
    /// When the kept rows reached the limit.
    filled: Option<Duration>,
    stop_sent: bool,
}

impl<'a> Read<'a> {
    fn new(fetch: Fetch<'a>, eligible: bool, started: Instant) -> Self {
        Self {
            fetch,
            eligible,
            started,
            set: None,
            result: QueryResult::default(),
            kept: 0,
            seen: 0,
            filled: None,
            stop_sent: false,
        }
    }

    /// One data frame: the result set's schema, then its batches. `Err` is
    /// the violation in it.
    fn data(&mut self, payload: &[u8]) -> Result<(), String> {
        let Some((schema, rows)) = &mut self.set else {
            let schema = decode_schema(payload).map_err(|violation| violation.to_string())?;
            let rows = Rows::new(&schema);
            if let Some(feed) = self.fetch.feed {
                feed.begin(rows.columns().to_vec());
            }
            self.result.columns = rows.columns().to_vec();
            self.set = Some((schema, rows));
            return Ok(());
        };
        let room = self
            .fetch
            .limit
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.kept));
        // In flight when `stop` went: the plugin's `end` counts these rows,
        // so they are dropped without decoding.
        if room == 0 {
            return Ok(());
        }
        let batch = decode_batch(schema, payload).map_err(|violation| violation.to_string())?;
        self.seen += batch.num_rows() as u64;
        let batch = batch.slice(0, batch.num_rows().min(room));
        let (feed, result) = (self.fetch.feed, &mut self.result);
        // Decoding validated every buffer, but the formatters are arrow's, and
        // a panic in them is the plugin's data, never DBDelve's crash.
        let pushed = panic::catch_unwind(AssertUnwindSafe(|| {
            rows.push_batch(&batch, |row| {
                result.bytes += row.text_len();
                match feed {
                    Some(feed) => feed.push(row),
                    None => result.rows.push(row),
                }
            })
        }))
        .map_err(|_| "result data that made rendering panic".to_owned())?;
        pushed.map_err(|error| error.message)?;
        self.kept += batch.num_rows();
        if batch.num_rows() > 0 && self.fetch.limit == Some(self.kept) {
            self.filled = Some(self.started.elapsed());
            if let Some(feed) = self.fetch.feed {
                feed.fill();
            }
        }
        Ok(())
    }

    /// Whether `stop` is to be sent now: once, for every capped query, as
    /// soon as the limit's rows are kept.
    fn stop_due(&mut self) -> bool {
        let due = !self.stop_sent && self.fetch.limit.is_some_and(|limit| self.kept >= limit);
        self.stop_sent |= due;
        due
    }

    /// The result, timed as Postgres's is: to the last kept row when rows
    /// past the limit came after it. `Err` is a plugin claiming to have ended
    /// early a statement it was never allowed to.
    fn finish(mut self, end: End) -> Result<(QueryResult, Vec<Said>), String> {
        if end.stopped && !(self.eligible && self.stop_sent) {
            return Err("`end` saying it stopped a statement it was not asked to stop".into());
        }
        // Short of the limit every row the statement made has arrived, so the
        // count must match exactly; past it, rows were dropped unread and it
        // can only be larger. Trusting it otherwise shows a cut result whole.
        let reached = self.fetch.limit.is_some_and(|limit| self.kept >= limit);
        if end.rows < self.seen || (!reached && end.rows > self.seen) {
            return Err(format!(
                "`end` counting {} rows where {} arrived",
                end.rows, self.seen
            ));
        }
        let capped = !end.stopped && reached && end.rows > self.kept as u64;
        self.result.capped_from = capped.then(|| usize::try_from(end.rows).unwrap_or(usize::MAX));
        self.result.stopped = end.stopped.then_some(Stopped::AtLimit);
        self.result.elapsed = match self.filled {
            Some(filled) if capped || end.stopped => filled,
            _ => self.started.elapsed(),
        };
        self.result.rows_affected = end.rows_affected;
        self.result.messages = end.messages;
        // ponytail: only the first result set crosses protocol 1; a later
        // protocol carries the rest as `QueryResult::rest`.
        if end.more_sets > 0 {
            let sets = match end.more_sets {
                1 => "set",
                _ => "sets",
            };
            self.result.messages.push(format!(
                "{} more result {sets} followed, which DBDelve does not show through a plugin.",
                end.more_sets
            ));
        }
        let said = self
            .set
            .as_ref()
            .map(|(_, rows)| rows.said().to_vec())
            .unwrap_or_default();
        Ok((self.result, said))
    }
}

/// How long a catalog, structure or references request may take before the
/// plugin is ended. These are no run's, so no Cancel reaches them, and a
/// driver wedged in one would hold the session until disconnect. A plugin is
/// handed the statement timeout for its catalog calls too, so past it the
/// grace covers only a driver that ignores it; with no timeout the bound is
/// generous, because listing a large catalog is slow without being stuck.
fn catalog_deadline(statement_timeout: u32) -> Duration {
    match statement_timeout {
        0 => CATALOG_BOUND,
        seconds => Duration::from_secs(seconds.into()) + CANCEL_DEADLINE,
    }
}

const CATALOG_BOUND: Duration = Duration::from_secs(300);

/// One step of opening: its answer, or the error that ends the open. A plugin
/// that dies on the way is reported with its stderr, since a missing driver
/// library says why there and nowhere else.
fn ask(host: &Host, request: Request, what: &str, deadline: Duration) -> Result<Response, DbError> {
    let (_, answers) = host.request(request).map_err(|_| host.startup_error())?;
    match answers.recv_timeout(deadline) {
        Ok(Routed::Control(Response::Error { message, .. })) => Err(failure(message)),
        Ok(Routed::Control(response)) => Ok(response),
        Ok(Routed::Data(_)) => Err(unexpected(host, what)),
        Ok(Routed::Reset(_)) | Err(RecvTimeoutError::Disconnected) => Err(host.startup_error()),
        Err(RecvTimeoutError::Timeout) => {
            host.kill();
            Err(failure(format!(
                "The {} plugin did not answer `{what}` within {} s.",
                host.plugin.name,
                deadline.as_secs()
            )))
        }
    }
}

fn unexpected(host: &Host, what: &str) -> DbError {
    host.kill();
    failure(format!(
        "The {} plugin answered `{what}` with something protocol {PROTOCOL} does not allow there.",
        host.plugin.name
    ))
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use dbdelve_plugin_protocol::limits::CLOSE_GRACE;

    use super::*;
    use crate::db::Feed;

    fn config() -> ExternalConfig {
        ExternalConfig {
            plugin: echo::plugin(),
            speaks: Engine::Postgres,
            connection: "DSN=echo".to_owned(),
            user: "someone@example.com".to_owned(),
            password: String::new(),
            statement_timeout: 0,
            ssh: None,
        }
    }

    fn open_with(env: &[(&str, &str)]) -> Result<Connection, DbError> {
        open_as(&config(), env)
    }

    fn open_as(config: &ExternalConfig, env: &[(&str, &str)]) -> Result<Connection, DbError> {
        let plugin = echo::plugin();
        let mut command = Command::new(&plugin.executable);
        command.envs(env.iter().copied());
        Connection::handshake(Host::spawn(plugin, command)?, config, || Ok(None))
    }

    fn gone_within(pid: u32, deadline: Duration) -> bool {
        let start = Instant::now();
        while echo::alive(pid) {
            if start.elapsed() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn open_answers_and_dropping_the_last_handle_ends_the_process() {
        let connection = Connection::open(&config()).unwrap();
        assert_eq!(
            *connection.opened(),
            Opened {
                dbms_name: "Echo".to_owned(),
                dbms_version: env!("CARGO_PKG_VERSION").to_owned(),
                driver: "dbdelve-echo".to_owned(),
                stops_early: true,
                optional: vec!["references".to_owned(), "write".to_owned()],
            }
        );
        let pid = connection.host.pid().unwrap();
        assert!(echo::alive(pid));

        let clone = connection.clone();
        drop(connection);
        assert!(echo::alive(pid), "a clone still holds the connection");
        drop(clone);
        assert!(gone_within(pid, CLOSE_GRACE + Duration::from_secs(1)));
    }

    /// Echo takes a moment over `close` and exits on EOF, so it writes its
    /// marker only if stdin stayed open until it had answered.
    #[test]
    fn a_close_is_answered_before_the_plugin_reads_the_end_of_its_stdin() {
        let marker =
            std::env::temp_dir().join(format!("dbdelve-echo-closed-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let connection = open_with(&[("DBDELVE_ECHO_CLOSED", marker.to_str().unwrap())]).unwrap();
        let pid = connection.host.pid().unwrap();
        drop(connection);
        assert!(gone_within(pid, CLOSE_GRACE + Duration::from_secs(1)));
        let answered = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(answered, "echo read EOF before it had answered `close`");
    }

    fn through_the_engine_enum(config: ExternalConfig) -> crate::db::Connection {
        crate::db::Connection::open(crate::db::ConnectionConfig::External(config)).unwrap()
    }

    fn one_cell(result: &QueryResult) -> String {
        result.rows[0].get(0).unwrap().to_owned()
    }

    #[test]
    fn a_plugin_connection_answers_through_the_engine_enum() {
        let connection = through_the_engine_enum(config());
        let cancel = crate::db::CancelToken::default();
        let read = connection
            .query("rows 10", &cancel, Fetch::default())
            .unwrap();
        assert_eq!(read.rows.len(), 10);
        let preview = connection
            .generated("rows 10", &cancel, None, Some(3))
            .unwrap();
        assert_eq!(preview.rows.len(), 3);
        assert!(!connection.catalog().unwrap().schemas.is_empty());
        for result in [&read, &preview] {
            assert!(result.edit.is_none());
            assert_eq!(
                result.read_only_reason.as_deref(),
                Some(crate::sql::Unread::Unparsed.reason())
            );
        }
        assert_eq!(connection.reported_dbms(), Some("Echo"));
        assert!(connection.stops_at_limit());
    }

    fn speaking(speaks: Engine) -> crate::db::Connection {
        through_the_engine_enum(ExternalConfig { speaks, ..config() })
    }

    fn read(connection: &crate::db::Connection, sql: &str) -> QueryResult {
        connection
            .query(sql, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
    }

    #[test]
    fn a_preview_and_a_query_of_a_keyed_table_name_their_rows_by_its_key() {
        let connection = speaking(Engine::Postgres);
        let preview = connection
            .generated(
                "SELECT * FROM \"main\".\"accounts\" LIMIT 10",
                &CancelToken::default(),
                None,
                None,
            )
            .unwrap();
        let typed = read(&connection, "SELECT ID, Label FROM Main.Accounts");
        for result in [preview, typed] {
            let edit = result
                .edit
                .unwrap_or_else(|| panic!("{:?}", result.read_only_reason));
            assert_eq!(
                (edit.schema.as_str(), edit.table.as_str()),
                ("main", "accounts")
            );
            assert_eq!(edit.columns, [Some("id".into()), Some("label".into())]);
            assert_eq!(edit.keys, [0]);
        }
    }

    #[test]
    fn generic_sql_finds_the_one_table_a_bare_name_folds_to() {
        let connection = speaking(Engine::Generic);
        let items = read(&connection, "SELECT id, label FROM main.items")
            .edit
            .unwrap();
        assert_eq!(
            (items.schema.as_str(), items.table.as_str()),
            ("MAIN", "ITEMS")
        );
        assert_eq!(items.columns, [Some("ID".into()), Some("LABEL".into())]);
        let accounts = read(&connection, "SELECT * FROM main.accounts")
            .edit
            .unwrap();
        assert_eq!(accounts.schema, "main");
        let cased = read(&connection, "SELECT id, Label FROM \"main\".Cased")
            .edit
            .unwrap();
        assert_eq!(
            (cased.table.as_str(), cased.columns.as_slice()),
            ("Cased", &[Some("id".into()), Some("Label".into())][..])
        );
    }

    #[test]
    fn rows_whose_table_or_key_is_in_doubt_stay_read_only_and_say_why() {
        let connection = speaking(Engine::Generic);
        for (sql, starts) in [
            ("SELECT * FROM accounts", "Name the table with its schema"),
            (
                "SELECT * FROM main.both",
                "Both main.both and MAIN.BOTH exist",
            ),
            (
                "SELECT * FROM main.Cased",
                "Both main.Cased and MAIN.CASED exist",
            ),
            (
                "SELECT * FROM main.keyless",
                "main.keyless has no primary key",
            ),
            (
                "SELECT label FROM main.accounts",
                "The key column id is not among",
            ),
            (
                "SELECT * FROM main.inexact",
                "The driver does not read the key column id",
            ),
            (
                "SELECT id, upper(label) FROM main.accounts",
                "Only rows selected as",
            ),
            (
                "SELECT id, label FROM main.mixed",
                "mixed has both a column label and a column LABEL",
            ),
        ] {
            let result = read(&connection, sql);
            assert!(result.edit.is_none(), "{sql}");
            let reason = result.read_only_reason.unwrap_or_default();
            assert!(reason.starts_with(starts), "{sql}: {reason}");
        }
    }

    #[test]
    fn a_dialect_not_proven_through_a_plugin_or_a_plugin_without_write_reads_only() {
        let reason = |connection: &crate::db::Connection| {
            read(connection, "SELECT * FROM main.accounts").read_only_reason
        };
        let mysql = speaking(Engine::MySql);
        assert_eq!(reason(&mysql), unwritable(Engine::MySql));
        let refused = mysql
            .generated(
                &update_batch(Engine::MySql, &["1"]),
                &CancelToken::default(),
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(Some(refused.message), unwritable(Engine::MySql));
        let plain = open_with(&[("DBDELVE_ECHO_PLAIN", "1")]).unwrap();
        let result = plain
            .query(
                "SELECT * FROM main.accounts",
                &CancelToken::default(),
                Fetch::default(),
            )
            .unwrap();
        assert!(result.edit.is_none());
        assert_eq!(
            result.read_only_reason.as_deref(),
            Some("This version of the Echo plugin cannot save edits, so these rows are read-only.")
        );
    }

    fn update_batch(engine: Engine, ids: &[&str]) -> String {
        use crate::result_grid::{NewValue, PendingRow};
        let rows: Vec<PendingRow> = ids
            .iter()
            .map(|id| PendingRow {
                schema: "main".into(),
                table: "accounts".into(),
                sets: vec![("label".into(), NewValue::Value("new".into()))],
                keys: vec![("id".into(), (*id).into())],
                ..PendingRow::default()
            })
            .collect();
        let batch = crate::sql::update_batch(engine, &rows).unwrap();
        assert!(crate::sql::is_generated_write_on(engine, &batch), "{batch}");
        batch
    }

    fn last(connection: &crate::db::Connection) -> String {
        one_cell(&read(connection, "last"))
    }

    #[test]
    fn a_batch_of_writes_is_one_write_request_without_its_brackets() {
        let connection = speaking(Engine::Generic);
        let batch = update_batch(Engine::Generic, &["1", "2"]);
        assert!(batch.starts_with("BEGIN;"), "{batch}");
        let written = connection
            .generated(&batch, &CancelToken::default(), None, None)
            .unwrap();
        assert_eq!(written.rows_affected, Some(2));
        assert_eq!(
            last(&connection),
            "UPDATE \"main\".\"accounts\" SET \"label\" = 'new' WHERE \"id\" = '1'\n\
             UPDATE \"main\".\"accounts\" SET \"label\" = 'new' WHERE \"id\" = '2'"
        );

        let postgres = speaking(Engine::Postgres);
        let delete =
            crate::sql::delete_row(Engine::Postgres, "main", "accounts", &[("id", "1")], &[])
                .unwrap();
        let deleted = postgres
            .generated(&delete, &CancelToken::default(), None, None)
            .unwrap();
        assert_eq!(deleted.rows_affected, Some(1));
        assert_eq!(last(&postgres), delete.trim_end_matches(';'));
    }

    #[test]
    fn a_write_that_changes_no_row_saves_nothing_and_says_so() {
        let connection = speaking(Engine::Postgres);
        let batch = update_batch(Engine::Postgres, &["1", "missing"]);
        let error = connection
            .generated(&batch, &CancelToken::default(), None, None)
            .unwrap_err();
        assert_eq!(
            error.message,
            "Statement 2 changed 0 rows, so nothing was saved."
        );
        assert!(!connection.is_lost());
    }

    #[test]
    fn a_write_waits_for_the_users_own_transaction_to_end() {
        let connection = speaking(Engine::Postgres);
        let batch = update_batch(Engine::Postgres, &["1"]);
        let write = || connection.generated(&batch, &CancelToken::default(), None, None);
        for end in ["commit", "rollback"] {
            read(&connection, "begin");
            let refused = write().unwrap_err();
            assert_eq!(
                refused.message,
                "Commit or roll back the open transaction first: an edit through a plugin driver \
                 runs as a transaction of its own, so nothing was sent."
            );
            assert_eq!(last(&connection), "begin");
            read(&connection, end);
            assert_eq!(write().unwrap().rows_affected, Some(1));
        }
    }

    #[test]
    fn a_commit_cancelled_unsent_leaves_writes_refused() {
        let connection = speaking(Engine::Postgres);
        read(&connection, "begin");
        let cancelled = CancelToken::alongside();
        connection.cancel(&cancelled).unwrap();
        let unsent = connection
            .query("commit", &cancelled, Fetch::default())
            .unwrap_err();
        assert_eq!(unsent.message, "Cancelled before it was sent.");
        let batch = update_batch(Engine::Postgres, &["1"]);
        let refused = connection
            .generated(&batch, &CancelToken::default(), None, None)
            .unwrap_err();
        assert!(
            refused.message.starts_with("Commit or roll back"),
            "{refused}"
        );
        assert_eq!(last(&connection), "begin");
    }

    /// Sent is only queued: a plugin answers a COMMIT cancelled before it ran
    /// without running it, and the user's transaction is still open.
    #[test]
    fn a_commit_cancelled_before_it_ran_leaves_writes_refused() {
        let connection = open_with(&[("DBDELVE_ECHO_HELD_ENDS", "1")]).unwrap();
        connection
            .query("begin", &CancelToken::default(), Fetch::default())
            .unwrap();
        let (error, _) = cancelled(&connection, "commit", Duration::from_millis(200));
        assert!(error.message.starts_with(CANCELLED), "{error}");
        let refused = connection
            .generated(
                &update_batch(Engine::Postgres, &["1"]),
                &CancelToken::default(),
                Fetch::default(),
            )
            .unwrap_err();
        assert!(
            refused.message.starts_with("Commit or roll back"),
            "{refused}"
        );
        let last = connection
            .query("last", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&last), Some("commit"));
    }

    #[test]
    fn a_write_the_plugin_did_not_offer_is_never_sent() {
        let connection = open_with(&[("DBDELVE_ECHO_PLAIN", "1")]).unwrap();
        connection
            .query("echo before", &CancelToken::default(), Fetch::default())
            .unwrap();
        let error = connection
            .generated(
                &update_batch(Engine::Postgres, &["1"]),
                &CancelToken::default(),
                Fetch::default(),
            )
            .unwrap_err();
        assert_eq!(
            error.message,
            "This version of the Echo plugin cannot save edits, so nothing was sent."
        );
        let last = connection
            .query("last", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&last), Some("echo before"));
    }

    #[test]
    fn statements_alongside_share_the_process_and_a_session_apart_starts_its_own() {
        let connection = through_the_engine_enum(config());
        let crate::db::Connection::External(inner) = &connection else {
            panic!("not a plugin connection");
        };
        let own = inner.host.pid().unwrap().to_string();
        let alongside = connection
            .query_alongside(
                "pid",
                &crate::db::CancelToken::alongside(),
                Fetch::default(),
                true,
            )
            .unwrap();
        assert_eq!(one_cell(&alongside), own);
        let apart = connection.apart().unwrap();
        let theirs = apart
            .query("pid", &crate::db::CancelToken::default(), Fetch::default())
            .unwrap();
        assert_ne!(one_cell(&theirs), own);
    }

    #[test]
    fn the_read_only_hold_is_the_dialects_own_statement_as_written() {
        for (speaks, sent) in [
            (Engine::Postgres, "SET default_transaction_read_only = on"),
            (Engine::MySql, "SET SESSION TRANSACTION READ ONLY"),
            // Holds nothing, so sends nothing; the gate asks for Read-write
            // for what it cannot read instead.
            (Engine::Generic, ""),
        ] {
            let connection = through_the_engine_enum(ExternalConfig { speaks, ..config() });
            connection.set_read_only(true).unwrap();
            let last = connection
                .query("last", &crate::db::CancelToken::default(), Fetch::default())
                .unwrap();
            assert_eq!(one_cell(&last), sent, "{speaks:?}");
        }
    }

    #[test]
    fn a_hold_that_fails_loses_the_session_rather_than_run_unheld() {
        let connection = open_with(&[("DBDELVE_ECHO_REFUSE_SET", "1")]).unwrap();
        let error = connection.set_read_only(true).err().unwrap();
        assert_eq!(error.message, "echo refused the setting, as asked");
        assert!(connection.is_lost());
        assert!(
            connection
                .query("rows 1", &CancelToken::default(), Fetch::default())
                .is_err()
        );

        // Failing to let go leaves the server held tighter than asked, which
        // costs nothing the gate relies on.
        let connection = open_with(&[("DBDELVE_ECHO_REFUSE_SET", "1")]).unwrap();
        assert!(connection.set_read_only(false).is_err());
        assert!(!connection.is_lost());
    }

    #[test]
    fn a_dialect_no_plugin_can_hold_is_refused_before_anything_starts() {
        for speaks in [Engine::ClickHouse, Engine::MongoDb, Engine::Redis] {
            let error = Connection::open(&ExternalConfig { speaks, ..config() })
                .err()
                .unwrap();
            assert_eq!(
                error.message,
                format!(
                    "{}'s SQL cannot be spoken through a plugin driver.",
                    speaks.label()
                )
            );
        }
    }

    #[test]
    fn a_silent_reconnect_is_refused_only_where_a_hold_would_be_dropped() {
        let reconnecting = |speaks| ExternalConfig {
            speaks,
            connection: "DSN=echo;AUTO_RECONNECT=1".into(),
            ..config()
        };
        for speaks in [Engine::MySql, Engine::Postgres] {
            let error = Connection::open(&reconnecting(speaks)).err().unwrap();
            assert!(
                error.message.starts_with("AUTO_RECONNECT"),
                "{}",
                error.message
            );
        }
        Connection::open(&reconnecting(Engine::Generic)).unwrap();
    }

    #[test]
    fn the_plugin_dials_the_stored_string_or_its_rewrite_through_the_tunnel() {
        let config = ExternalConfig {
            connection: "DRIVER=Db2;HOSTNAME=db.example;PORT=50000;UID=a".into(),
            ..config()
        };
        let local = "127.0.0.1:61234".parse().unwrap();
        assert_eq!(dialled(&config, None).unwrap(), config.connection);
        assert_eq!(
            dialled(&config, Some(local)).unwrap(),
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a"
        );
        let dsn = ExternalConfig {
            connection: "Db2Dev".into(),
            ..config
        };
        assert_eq!(dialled(&dsn, None).unwrap(), "Db2Dev");
        let refused = dialled(&dsn, Some(local)).err().unwrap();
        assert!(
            refused.message.starts_with("A DSN keeps"),
            "{}",
            refused.message
        );
    }

    /// Neither the SSH host nor the plugin's executable exists, so any error
    /// but the refusal would mean one of them was started first.
    #[test]
    fn a_string_the_tunnel_cannot_route_is_refused_before_ssh_or_the_plugin_starts() {
        let plugin = Box::leak(Box::new(Plugin {
            id: "echo".to_owned(),
            name: "Echo".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executable: echo::plugin().executable.with_file_name("dbdelve-nowhere"),
        }));
        let config = ExternalConfig {
            plugin,
            connection: "Db2Dev".into(),
            ssh: Some(SshTunnel {
                host: "unreachable.invalid".into(),
                ..SshTunnel::default()
            }),
            ..config()
        };
        let error = Connection::open(&config).err().unwrap();
        assert!(
            error
                .message
                .starts_with("A DSN keeps its server's address"),
            "{}",
            error.message
        );
        assert_eq!(
            crate::db::ConnectionConfig::External(config).ssh(),
            Some(&SshTunnel {
                host: "unreachable.invalid".into(),
                ..SshTunnel::default()
            })
        );
    }

    #[test]
    fn a_config_never_prints_its_secrets() {
        let config = ExternalConfig {
            connection: "DSN=echo;PWD=hunter2".to_owned(),
            password: "hunter3".to_owned(),
            ..config()
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("hunter"), "{printed}");
    }

    #[test]
    fn an_optional_message_only_one_side_lists_is_not_negotiated() {
        let connection = open_with(&[("DBDELVE_ECHO_PLAIN", "1")]).unwrap();
        assert!(connection.opened().optional.is_empty());
    }

    #[test]
    fn a_protocol_mismatch_fails_the_open_naming_both() {
        let error = open_with(&[("DBDELVE_ECHO_PROTOCOL", "99")]).err().unwrap();
        assert_eq!(
            error.message,
            "The Echo plugin speaks protocol 99; this DBDelve speaks 1."
        );
    }

    #[test]
    fn a_plugin_that_dies_before_hello_says_why_through_its_stderr() {
        let said = "libodbc.so.2: cannot open shared object file";
        let error = open_with(&[("DBDELVE_ECHO_STDERR", said)]).err().unwrap();
        assert_eq!(
            error.message,
            format!("The Echo plugin could not start: {said}")
        );
    }

    #[test]
    fn a_missing_executable_fails_the_open() {
        let plugin = Box::leak(Box::new(Plugin {
            id: "echo".to_owned(),
            name: "Echo".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executable: echo::plugin().executable.with_file_name("dbdelve-nowhere"),
        }));
        let error = Connection::open(&ExternalConfig { plugin, ..config() })
            .err()
            .unwrap();
        assert!(
            error
                .message
                .starts_with("The Echo plugin could not start: "),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_violation_after_open_is_a_reset_and_ends_the_process() {
        for statement in ["garbage", "oversize", "empty"] {
            let connection = Connection::open(&config()).unwrap();
            let pid = connection.host.pid().unwrap();
            let query = Request::Query {
                sql: statement.to_owned(),
                eligible: false,
            };
            let (_, answers) = connection.host.request(query).unwrap();
            let Ok(Routed::Reset(cause)) = answers.recv_timeout(Duration::from_secs(5)) else {
                panic!("{statement}: no reset");
            };
            assert!(
                cause.starts_with("The Echo plugin broke protocol 1 ("),
                "{statement}: {cause}"
            );
            assert!(connection.is_lost());
            assert!(
                !echo::alive(pid),
                "{statement}: the plugin outlived its reset"
            );

            let again = connection.host.request(Request::Catalog).err().unwrap();
            assert!(
                again.message.starts_with(&format!(
                    "{cause} The connection was reset: any open transaction"
                )),
                "{statement}: {}",
                again.message
            );
            assert!(!again.message.contains("committed is unknown"));
            let running = connection.host.reset_error(true).message;
            assert!(
                running.ends_with("Whether the statement that was running committed is unknown.")
            );
        }
    }

    /// Past the window echo waits for grants, so reaching `end` proves each
    /// one was counted and none was spent before the reader knew of it.
    #[test]
    fn a_read_larger_than_the_window_flows_on_granted_credit() {
        use dbdelve_plugin_protocol::limits::WINDOW;

        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "rows 1000000".to_owned(),
            eligible: false,
        };
        let (in_flight, answers) = connection.host.request(query).unwrap();
        let id = in_flight.id;
        let mut received = 0;
        let rows = loop {
            match answers.recv_timeout(Duration::from_secs(10)).unwrap() {
                Routed::Data(payload) => {
                    received += payload.len() as u64;
                    let bytes = credit_cost(payload.len());
                    connection.host.send(id, Request::Credit { bytes });
                }
                Routed::Control(Response::End { rows, .. }) => break rows,
                Routed::Control(other) => panic!("{other:?}"),
                Routed::Reset(cause) => panic!("{cause}"),
            }
        };
        assert_eq!(rows, 1_000_000);
        assert!(
            received > 2 * WINDOW,
            "{received} bytes never needed a grant"
        );
        assert!(!connection.is_lost());
    }

    #[test]
    fn data_beyond_credit_is_a_reset() {
        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "flood".to_owned(),
            eligible: false,
        };
        let (_, answers) = connection.host.request(query).unwrap();
        let cause = loop {
            match answers.recv_timeout(Duration::from_secs(10)).unwrap() {
                Routed::Data(_) => {}
                Routed::Reset(cause) => break cause,
                Routed::Control(other) => panic!("{other:?}"),
            }
        };
        assert!(cause.contains("of credit left"), "{cause}");
        assert!(connection.is_lost());
    }

    /// The grandchild holds stdout open for 20 s, so only the exit itself can
    /// tell the caller in time.
    #[test]
    fn a_plugin_that_exits_is_a_reset_even_while_its_stdout_stays_open() {
        let connection = Connection::open(&config()).unwrap();
        let started = Instant::now();
        let error = connection
            .query("orphan", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            error.message.starts_with("The Echo plugin stopped ("),
            "{}",
            error.message
        );
        assert!(error.message.ends_with("committed is unknown."));
        assert!(connection.is_lost());
        let again = connection.catalog().unwrap_err();
        assert!(again.message.contains("The connection was reset"));
    }

    #[test]
    fn a_crash_mid_frame_is_a_stop_with_its_status_not_a_violation() {
        let connection = Connection::open(&config()).unwrap();
        let error = connection
            .query("cut", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert!(
            error.message.starts_with("The Echo plugin stopped ("),
            "{}",
            error.message
        );
        assert!(error.message.contains('4'), "{}", error.message);
    }

    #[test]
    fn a_catalog_answer_out_of_place_says_nothing_of_a_commit() {
        let connection = open_with(&[]).unwrap();
        let error = connection.host.violated("a test's violation", false);
        assert!(
            error
                .message
                .contains("broke protocol 1 (a test's violation)")
        );
        assert!(!error.message.contains("committed is unknown"));
    }

    #[test]
    fn an_exit_after_open_is_reported_with_its_status() {
        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "exit 3".to_owned(),
            eligible: false,
        };
        let (_, answers) = connection.host.request(query).unwrap();
        let Ok(Routed::Reset(cause)) = answers.recv_timeout(Duration::from_secs(5)) else {
            panic!("no reset");
        };
        // ExitStatus's own wording differs on Windows; the prefix is ours.
        assert!(cause.starts_with("The Echo plugin stopped ("), "{cause}");
        assert!(cause.contains('3'), "{cause}");
        assert!(connection.is_lost());
    }

    fn capped(limit: usize, reads_only: bool) -> Fetch<'static> {
        Fetch {
            limit: Some(limit),
            reads_only,
            ..Fetch::default()
        }
    }

    fn cell(result: &QueryResult) -> Option<&str> {
        result.rows.first()?.get(0)
    }

    #[test]
    fn every_row_arrives_in_order_with_its_nulls() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection
            .query("rows 10000", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(result.rows.len(), 10_000);
        for (n, row) in (1..).zip(&result.rows) {
            assert_eq!(row.get(0), Some(n.to_string().as_str()));
            let label = (n % 7 != 0).then(|| format!("row {n}"));
            assert_eq!(row.get(1), label.as_deref(), "row {n}");
        }
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["n", "label"]
        );
        assert_eq!((result.capped_from, result.stopped), (None, None));
        assert!(result.edit.is_none() && result.read_only_reason.is_some());

        let typed = "echo SELECT 'a' -- x";
        let result = connection
            .query(typed, &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&result), Some(typed));
    }

    #[test]
    fn rows_go_to_the_feed_when_there_is_one() {
        let connection = Connection::open(&config()).unwrap();
        let feed = Feed::default();
        let fetch = Fetch {
            feed: Some(&feed),
            ..capped(1500, false)
        };
        let result = connection
            .query("rows 3000", &CancelToken::default(), fetch)
            .unwrap();
        assert!(result.rows.is_empty());
        let fed = feed.take();
        assert_eq!(fed.rows.len(), 1500);
        assert_eq!(fed.columns.len(), 2);
        assert!(fed.filled.is_some());
        assert_eq!(result.capped_from, Some(3000));
    }

    #[test]
    fn the_row_limit_stops_an_eligible_read() {
        let connection = Connection::open(&config()).unwrap();
        assert!(connection.stops_at_limit());
        let started = Instant::now();
        let result = connection
            .query("rows 1000000", &CancelToken::default(), capped(100, true))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.capped_from, None);
        assert_eq!(result.rows.len(), 100);
        assert_eq!(result.rows[99].get(0), Some("100"));
    }

    #[test]
    fn an_ineligible_read_drains_without_cancelling() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection
            .query("rows 1000000", &CancelToken::default(), capped(100, false))
            .unwrap();
        assert_eq!(result.capped_from, Some(1_000_000));
        assert_eq!(result.stopped, None);
        assert_eq!(result.rows.len(), 100);

        let cancels = connection
            .query("cancels", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&cancels), Some("0"));
    }

    #[test]
    fn a_plugin_that_cannot_stop_early_is_never_told_it_may() {
        let connection = open_with(&[("DBDELVE_ECHO_DRAINS", "1")]).unwrap();
        assert!(!connection.stops_at_limit());
        let result = connection
            .query("rows 1000000", &CancelToken::default(), capped(100, true))
            .unwrap();
        assert_eq!(result.stopped, None);
        assert_eq!(result.capped_from, Some(1_000_000));
    }

    #[test]
    fn an_end_whose_count_contradicts_the_rows_that_arrived_is_a_violation() {
        for (statement, limit) in [
            ("miscount 10 11", None),
            ("miscount 10 9", None),
            ("miscount 2000 1024", Some(1500)),
        ] {
            let connection = Connection::open(&config()).unwrap();
            let fetch = Fetch {
                limit,
                ..Fetch::default()
            };
            let error = connection
                .query(statement, &CancelToken::default(), fetch)
                .unwrap_err();
            assert!(
                error.message.contains("broke protocol 1 (`end` counting"),
                "{statement}: {}",
                error.message
            );
        }
    }

    #[test]
    fn a_capped_read_keeps_the_users_transaction() {
        let connection = Connection::open(&config()).unwrap();
        connection
            .query("begin", &CancelToken::default(), Fetch::default())
            .unwrap();
        let result = connection
            .query("rows 1000000", &CancelToken::default(), capped(2, true))
            .unwrap();
        assert_eq!(result.stopped, None, "stopping would end the transaction");
        assert_eq!(result.capped_from, Some(1_000_000));
        assert_eq!(result.rows.len(), 2);

        connection
            .query("commit", &CancelToken::default(), Fetch::default())
            .unwrap();
        let after = connection
            .query("rows 1000000", &CancelToken::default(), capped(2, true))
            .unwrap();
        assert_eq!(after.stopped, Some(Stopped::AtLimit));
    }

    #[test]
    fn a_limit_of_zero_keeps_no_rows_and_still_says_how_many_there_were() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection
            .query("rows 5000", &CancelToken::default(), capped(0, false))
            .unwrap();
        assert!(result.rows.is_empty());
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.capped_from, Some(5000));
    }

    #[test]
    fn a_statement_not_held_is_never_sent() {
        let connection = Connection::open(&config()).unwrap();
        let held = || false;
        let fetch = Fetch {
            held: Some(&held),
            ..Fetch::default()
        };
        let error = connection
            .query("begin", &CancelToken::default(), fetch)
            .err()
            .unwrap();
        assert_eq!(error.message, "Cancelled before it was sent.");
        connection
            .query("rows 3", &CancelToken::default(), Fetch::default())
            .unwrap();
    }

    #[test]
    fn a_server_error_is_the_statements_and_lost_marks_the_connection() {
        let connection = Connection::open(&config()).unwrap();
        let error = connection
            .query("fail", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert_eq!(error.message, "echo failed, as asked");
        assert!(!connection.is_lost());

        connection
            .query("lost", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert!(connection.is_lost());
        for refused in [
            connection
                .query("rows 1", &CancelToken::default(), Fetch::default())
                .unwrap_err(),
            connection.catalog().unwrap_err(),
        ] {
            assert!(
                refused
                    .message
                    .starts_with("The Echo plugin lost its session. The connection was reset"),
                "{}",
                refused.message
            );
        }
    }

    #[test]
    fn result_sets_after_the_first_are_said_and_not_shown() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection
            .query("sets 3", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.messages,
            ["2 more result sets followed, which DBDelve does not show through a plugin."]
        );
    }

    #[test]
    fn a_crash_mid_stream_is_a_reset_and_nothing_runs_again() {
        let connection = Connection::open(&config()).unwrap();
        let error = connection
            .query("crash", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert!(error.message.contains("stopped ("), "{}", error.message);
        assert!(
            error.message.contains("committed is unknown"),
            "{}",
            error.message
        );
        assert!(connection.is_lost());

        let again = connection
            .query("rows 1", &CancelToken::default(), Fetch::default())
            .err()
            .unwrap();
        assert!(
            again.message.contains("The connection was reset"),
            "{}",
            again.message
        );
        assert!(!again.message.contains("committed is unknown"));
        assert_eq!(connection.host.pid(), None, "no process was started again");
    }

    #[test]
    fn result_data_the_protocol_does_not_carry_is_a_reset_never_a_panic() {
        for statement in ["compressed", "dictionary"] {
            let connection = Connection::open(&config()).unwrap();
            let pid = connection.host.pid().unwrap();
            let error = connection
                .query(statement, &CancelToken::default(), Fetch::default())
                .err()
                .unwrap();
            assert!(
                error.message.contains("broke protocol 1 ("),
                "{statement}: {}",
                error.message
            );
            assert!(error.message.contains("committed is unknown"));
            assert!(connection.is_lost());
            assert!(gone_within(pid, Duration::from_secs(5)), "{statement}");
        }
    }

    /// `sql` on its own thread under a run of its own, cancelled after
    /// `after`: the result, and how long it took to come back once cancelled.
    fn cancelled(connection: &Connection, sql: &str, after: Duration) -> (DbError, Duration) {
        let run = CancelToken::default();
        let worker = {
            let (running, sql, run) = (connection.clone(), sql.to_owned(), run.clone());
            std::thread::spawn(move || running.query(&sql, &run, Fetch::default()))
        };
        std::thread::sleep(after);
        let cancelled = Instant::now();
        connection.cancel(&run).unwrap();
        let error = worker.join().unwrap().err().unwrap();
        (error, cancelled.elapsed())
    }

    #[test]
    fn a_cancel_ends_the_running_statement() {
        let connection = Connection::open(&config()).unwrap();
        let (error, took) = cancelled(&connection, "sleep 60000", Duration::from_millis(200));
        assert!(error.message.starts_with(CANCELLED), "{}", error.message);
        assert!(took < CANCEL_DEADLINE / 2, "{took:?}");
        assert!(!connection.is_lost());

        let cancels = connection
            .query("cancels", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&cancels), Some("1"));
    }

    #[test]
    fn a_cancel_with_nothing_running_sends_nothing() {
        let connection = Connection::open(&config()).unwrap();
        connection.cancel(&CancelToken::default()).unwrap();
        let run = CancelToken::default();
        connection.query("rows 1", &run, Fetch::default()).unwrap();
        connection.cancel(&run).unwrap();
        let cancels = connection
            .query("cancels", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(cell(&cancels), Some("0"));
    }

    /// Two lanes of a queue run at once, queued on the one process: a Cancel
    /// under the waiting lane's token drops it unsent and leaves the running
    /// one be, and one under the running lane's stops it and not the lane
    /// waiting behind it.
    #[test]
    fn a_cancel_stops_only_what_its_own_run_sent() {
        let connection = Connection::open(&config()).unwrap();
        let lane = |run: &CancelToken, sql: &'static str| {
            let (connection, run) = (connection.clone(), run.clone());
            let lane = std::thread::spawn(move || connection.query(sql, &run, Fetch::default()));
            std::thread::sleep(Duration::from_millis(200));
            lane
        };
        let take = |run: &CancelToken, sql| {
            let result = connection.query(sql, run, Fetch::default()).unwrap();
            cell(&result).map(str::to_owned)
        };

        let (running, waiting) = (CancelToken::alongside(), CancelToken::alongside());
        let ran = lane(&running, "sleep 1000");
        let dropped = lane(&waiting, "echo waiting");
        connection.cancel(&waiting).unwrap();
        ran.join().unwrap().unwrap();
        let error = dropped.join().unwrap().unwrap_err();
        assert_eq!(error.message, "Cancelled before it was sent.");
        let idle = CancelToken::default();
        assert_eq!(take(&idle, "last").as_deref(), Some("sleep 1000"));
        assert_eq!(take(&idle, "cancels").as_deref(), Some("0"));

        let (running, waiting) = (CancelToken::alongside(), CancelToken::alongside());
        let stopped = lane(&running, "sleep 60000");
        let behind = lane(&waiting, "echo behind");
        connection.cancel(&running).unwrap();
        let error = stopped.join().unwrap().unwrap_err();
        assert!(error.message.starts_with(CANCELLED), "{}", error.message);
        let behind = behind.join().unwrap().unwrap();
        assert_eq!(cell(&behind), Some("echo behind"));
        assert_eq!(take(&idle, "cancels").as_deref(), Some("1"));
    }

    #[test]
    fn a_plugin_that_does_not_answer_a_cancel_is_killed_at_the_deadline() {
        let connection = Connection::open(&config()).unwrap();
        let pid = connection.host.pid().unwrap();
        let (error, took) = cancelled(&connection, "hang", Duration::from_millis(200));
        assert!(took < CANCEL_DEADLINE + Duration::from_secs(1), "{took:?}");
        assert!(
            error.message.starts_with(
                "The Echo plugin did not answer a cancel within 5 s and was stopped. The \
                 connection was reset"
            ),
            "{}",
            error.message
        );
        assert!(error.message.ends_with("committed is unknown."));
        assert!(connection.is_lost());
        assert!(gone_within(pid, Duration::from_secs(1)));
    }

    /// The feed holds ten rows and nobody takes them, so the query's thread
    /// sits in `push` while the plugin spends its window and waits.
    #[test]
    fn a_cancel_is_answered_under_a_stalled_consumer_and_the_window_holds() {
        use dbdelve_plugin_protocol::MAX_PAYLOAD;
        use dbdelve_plugin_protocol::limits::{BATCH_TARGET, WINDOW};

        let connection = Connection::open(&config()).unwrap();
        let pid = connection.host.pid().unwrap();
        let feed = Feed::bounded(10);
        let run = CancelToken::default();
        let worker = {
            let (connection, feed, run) = (connection.clone(), feed.clone(), run.clone());
            std::thread::spawn(move || {
                let fetch = Fetch {
                    feed: Some(&feed),
                    ..Fetch::default()
                };
                connection.query("rows 10000000", &run, fetch)
            })
        };

        let waiting = Instant::now();
        while connection.host.peak() < (WINDOW - BATCH_TARGET as u64) as i64 {
            assert!(
                waiting.elapsed() < Duration::from_secs(20),
                "the window never filled"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let id = connection.host.running().unwrap();
        connection.cancel(&run).unwrap();
        let cancelled = Instant::now();
        while connection.host.is_pending(id) && echo::alive(pid) {
            assert!(cancelled.elapsed() < CANCEL_DEADLINE + Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished(), "the consumer was stalled throughout");
        let bound = WINDOW.max(credit_cost(MAX_PAYLOAD)) as i64;
        assert!(
            connection.host.peak() <= bound,
            "{}",
            connection.host.peak()
        );

        while !worker.is_finished() {
            feed.take();
            std::thread::yield_now();
        }
        let error = worker.join().unwrap().err().unwrap();
        assert_eq!(error.message, CANCELLED);
        assert!(!connection.is_lost());
    }

    #[test]
    fn the_catalog_lists_schemas_and_kinds() {
        let catalog = open_with(&[]).unwrap().catalog().unwrap();
        let names: Vec<_> = catalog.schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["main", "audit"]);
        let kinds: Vec<_> = catalog.schemas[0]
            .relations
            .iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                RelationKind::Table,
                RelationKind::View,
                RelationKind::MaterializedView
            ]
        );
    }

    #[test]
    fn structure_round_trips_and_its_key_reads_back() {
        let structure = open_with(&[])
            .unwrap()
            .structure("main", "accounts")
            .unwrap();
        assert_eq!(structure.primary_key(), ["id"]);
        assert_eq!(structure.row_key(), ["id"]);
        assert_eq!(structure.indexes[0].kind, IndexKind::Primary);
        assert_eq!(structure.indexes[1].kind, IndexKind::Plain);
        assert_eq!(structure.foreign_keys[0].referenced_schema, "main");
        assert_eq!(structure.foreign_keys[0].referenced_table, "accounts");
        assert_eq!(structure.columns[1].default.as_deref(), Some("'none'"));
    }

    #[test]
    fn a_column_name_holding_a_comma_reads_back_as_no_key() {
        let wire = WireStructure {
            columns: ["a, b"]
                .map(|name| dbdelve_plugin_protocol::Column {
                    name: name.into(),
                    data_type: "int".into(),
                    nullable: false,
                    default: None,
                })
                .into(),
            primary_key: vec!["a, b".into()],
            ..Default::default()
        };
        assert!(structure_from(wire).row_key().is_empty());
    }

    #[test]
    fn references_are_asked_only_when_negotiated() {
        let references = open_with(&[])
            .unwrap()
            .references("main", "accounts")
            .unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].table, "main.accounts");
        let plain = open_with(&[("DBDELVE_ECHO_PLAIN", "1")]).unwrap();
        assert!(plain.references("main", "accounts").unwrap().is_empty());
    }

    #[test]
    fn a_catalog_error_is_the_calls_and_lost_marks_the_connection() {
        let connection = open_with(&[]).unwrap();
        let error = connection.structure("main", "fail").unwrap_err();
        assert_eq!(error.message, "no such relation");
        assert!(!connection.is_lost());
        assert_eq!(
            connection.structure("main", "lost").unwrap_err().message,
            "session gone"
        );
        assert!(connection.is_lost());
    }

    #[test]
    fn a_catalog_call_after_a_reset_is_the_reset() {
        let connection = open_with(&[]).unwrap();
        let _ = connection.query("exit 3", &CancelToken::default(), capped(10, false));
        let error = connection.catalog().unwrap_err();
        assert!(
            error.message.contains("connection was reset"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_catalog_call_past_its_deadline_ends_the_plugin_and_names_the_call() {
        let config = ExternalConfig {
            statement_timeout: 1,
            ..config()
        };
        let connection = open_as(&config, &[("DBDELVE_ECHO_WEDGED", "1")]).unwrap();
        let pid = connection.host.pid().unwrap();
        let started = Instant::now();
        let error = connection.structure("main", "accounts").unwrap_err();
        let took = started.elapsed();
        let deadline = catalog_deadline(1);
        assert!(
            took >= deadline && took < deadline + Duration::from_secs(1),
            "{took:?}"
        );
        assert!(
            error.message.starts_with(
                "The Echo plugin did not answer `structure` within 6 s and was stopped. The \
                 connection was reset"
            ),
            "{}",
            error.message
        );
        assert!(connection.is_lost());
        assert!(gone_within(pid, Duration::from_secs(1)));
        assert_eq!(catalog_deadline(0), CATALOG_BOUND);
    }

    #[test]
    fn a_catalog_call_queued_behind_a_statement_gives_up_at_its_deadline_and_keeps_the_session() {
        let config = ExternalConfig {
            statement_timeout: 1,
            ..config()
        };
        let connection = Connection::open(&config).unwrap();
        let run = CancelToken::default();
        let running = {
            let (connection, run) = (connection.clone(), run.clone());
            std::thread::spawn(move || connection.query("sleep 60000", &run, Fetch::default()))
        };
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let error = connection.catalog().unwrap_err();
        let took = started.elapsed();
        let deadline = catalog_deadline(1);
        assert!(
            took >= deadline && took < deadline + Duration::from_secs(1),
            "{took:?}"
        );
        assert_eq!(
            error.message,
            "`catalog` waited 6 s for the Echo plugin's session, busy with another statement, \
             and was not sent."
        );
        assert!(!connection.is_lost());

        connection.cancel(&run).unwrap();
        let cancelled = running.join().unwrap().unwrap_err();
        assert!(
            cancelled.message.starts_with(CANCELLED),
            "{}",
            cancelled.message
        );
        assert!(!connection.catalog().unwrap().schemas.is_empty());
    }

    #[test]
    fn stdin_eof_ends_echo() {
        let mut child = crate::db::spawn(
            Command::new(&echo::plugin().executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::null()),
        )
        .unwrap();
        drop(child.stdin.take());
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(15) {
                let _ = child.kill();
                panic!("echo outlived its stdin");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

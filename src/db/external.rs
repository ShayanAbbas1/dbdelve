//! A connection through an engine plugin: one child process per connection,
//! spoken to in `dbdelve-plugin-protocol` frames.
//!
//! A plugin that dies or breaks the protocol loses its connection, which is
//! reported as a reset and never reopened or replayed here: the workspace's own
//! reconnect opens a fresh one and re-applies the Read-only hold before
//! anything else runs on it.

#[cfg(test)]
mod echo;
mod host;
mod rows;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use dbdelve_plugin_protocol::arrow::{decode_batch, decode_schema};
use dbdelve_plugin_protocol::limits::{HELLO_DEADLINE, OPEN_DEADLINE};
use dbdelve_plugin_protocol::{
    ErrorKind, PROTOCOL, Password, REFERENCES, RelationKind as WireKind, Request, Response,
    Structure as WireStructure,
};

use super::plugin::Plugin;
use super::{
    Catalog, ColumnDefinition, DbError, Engine, Fetch, ForeignKey, Index, IndexKind,
    NamedDefinition, QueryResult, Reference, Relation, RelationKind, Schema, Stopped, Structure,
};
use host::{Host, Routed, failure};
use rows::Rows;

pub const CANCELLED: &str = "Cancelled.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalConfig {
    pub plugin: &'static Plugin,
    /// The built-in dialect DBDelve writes SQL in for this connection.
    pub speaks: Engine,
    /// As the user typed it; the plugin dials it.
    pub connection: String,
    pub user: String,
    pub password: String,
    /// Seconds, 0 for none.
    pub statement_timeout: u32,
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
}

impl Connection {
    pub fn open(config: &ExternalConfig) -> Result<Self, DbError> {
        Self::handshake(Host::start(config.plugin)?, config)
    }

    fn handshake(host: Arc<Host>, config: &ExternalConfig) -> Result<Self, DbError> {
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

        let open = Request::Open {
            fields,
            password: Password(config.password.clone()),
            statement_timeout: config.statement_timeout,
        };
        let Response::Opened {
            dbms_name,
            dbms_version,
            driver,
            stops_early,
            optional: offered,
        } = ask(&host, open, "open", OPEN_DEADLINE)?
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
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        let _session = self.host.session();
        fetch.hold()?;
        let started = Instant::now();
        let eligible = fetch.reads_only && fetch.limit.is_some();
        let query = Request::Query {
            sql: sql.to_owned(),
            eligible,
        };
        let (id, answers) = self.host.request(query)?;
        let result = self.read(id, &answers, Read::new(fetch, eligible, started));
        self.host.done(id);
        result
    }

    /// The user's Cancel, which may abort an open transaction, as cancelling
    /// a built-in's statement does. With nothing running it does nothing, as
    /// on Postgres.
    pub fn cancel(&self) -> Result<(), DbError> {
        self.host.cancel();
        Ok(())
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let Response::Catalog { schemas } = self.call(Request::Catalog)? else {
            return Err(self
                .host
                .violated("an answer to `catalog` that is not `catalog`"));
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
        let request = Request::Structure {
            schema: schema.to_owned(),
            relation: relation.to_owned(),
        };
        match self.call(request)? {
            Response::Structure(wire) => Ok(structure_from(wire)),
            _ => Err(self
                .host
                .violated("an answer to `structure` that is not `structure`")),
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
                .violated("an answer to `references` that is not `references`")),
        }
    }

    /// One non-query request, under the session like a statement.
    fn call(&self, request: Request) -> Result<Response, DbError> {
        let _session = self.host.session();
        let (_, answers) = self.host.request(request)?;
        match answers.recv() {
            Ok(Routed::Control(Response::Error {
                kind,
                message,
                position,
                lost,
                ..
            })) => Err(self.plugin_error(kind, message, position, lost)),
            Ok(Routed::Control(response)) => Ok(response),
            Ok(Routed::Data(_)) => Err(self.host.violated("data in answer to a catalog request")),
            Ok(Routed::Reset(_)) | Err(_) => Err(self.host.reset_error(false)),
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

    fn read(
        &self,
        id: u64,
        answers: &Receiver<Routed>,
        mut read: Read<'_>,
    ) -> Result<QueryResult, DbError> {
        loop {
            let Ok(routed) = answers.recv() else {
                return Err(self.host.reset_error(true));
            };
            match routed {
                Routed::Data(payload) => {
                    read.data(&payload)
                        .map_err(|violation| self.host.violated(&violation))?;
                    if read.stop_due() {
                        self.host.send(id, Request::Stop);
                    }
                    // Only now, with every kept row handed on: a feed whose
                    // taker has stopped taking holds this back, and the plugin
                    // stops sending without the reader ever pausing.
                    let bytes = payload.len() as u64;
                    self.host.send(id, Request::Credit { bytes });
                }
                Routed::Control(Response::End {
                    rows,
                    rows_affected,
                    stopped,
                    messages,
                    more_sets,
                }) => {
                    let end = End {
                        rows,
                        rows_affected,
                        stopped,
                        messages,
                        more_sets,
                    };
                    return read
                        .finish(end)
                        .map_err(|violation| self.host.violated(&violation));
                }
                Routed::Control(Response::Error {
                    kind,
                    message,
                    position,
                    lost,
                    ..
                }) => {
                    return Err(self.plugin_error(kind, message, position, lost));
                }
                Routed::Control(_) => {
                    return Err(self
                        .host
                        .violated("an answer to a query that is neither `end` nor `error`"));
                }
                Routed::Reset(_) => return Err(self.host.reset_error(true)),
            }
        }
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
        let batch = batch.slice(0, batch.num_rows().min(room));
        let (feed, result) = (self.fetch.feed, &mut self.result);
        rows.push_batch(&batch, |row| {
            result.bytes += row.text_len();
            match feed {
                Some(feed) => feed.push(row),
                None => result.rows.push(row),
            }
        })
        .map_err(|error| error.message)?;
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
    fn finish(mut self, end: End) -> Result<QueryResult, String> {
        if end.stopped && !(self.eligible && self.stop_sent) {
            return Err("`end` saying it stopped a statement it was not asked to stop".into());
        }
        let capped = !end.stopped
            && self.fetch.limit.is_some_and(|limit| self.kept >= limit)
            && end.rows > self.kept as u64;
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
        self.result.read_only_reason =
            Some("Rows read through a plugin driver are read-only.".into());
        Ok(self.result)
    }
}

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

    use dbdelve_plugin_protocol::limits::{CANCEL_DEADLINE, CLOSE_GRACE};

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
        }
    }

    fn open_with(env: &[(&str, &str)]) -> Result<Connection, DbError> {
        let plugin = echo::plugin();
        let mut command = Command::new(&plugin.executable);
        command.envs(env.iter().copied());
        Connection::handshake(Host::spawn(plugin, command)?, &config())
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
                optional: vec!["references".to_owned()],
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
        for statement in ["garbage", "oversize"] {
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
        let (id, answers) = connection.host.request(query).unwrap();
        let mut received = 0;
        let rows = loop {
            match answers.recv_timeout(Duration::from_secs(10)).unwrap() {
                Routed::Data(payload) => {
                    received += payload.len() as u64;
                    let bytes = payload.len() as u64;
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
        assert!(cause.contains("bytes of credit left"), "{cause}");
        assert!(connection.is_lost());
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
        let result = connection.query("rows 10000", Fetch::default()).unwrap();
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
        let result = connection.query(typed, Fetch::default()).unwrap();
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
        let result = connection.query("rows 3000", fetch).unwrap();
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
        let result = connection.query("rows 1000000", capped(100, true)).unwrap();
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
            .query("rows 1000000", capped(100, false))
            .unwrap();
        assert_eq!(result.capped_from, Some(1_000_000));
        assert_eq!(result.stopped, None);
        assert_eq!(result.rows.len(), 100);

        let cancels = connection.query("cancels", Fetch::default()).unwrap();
        assert_eq!(cell(&cancels), Some("0"));
    }

    #[test]
    fn a_capped_read_keeps_the_users_transaction() {
        let connection = Connection::open(&config()).unwrap();
        connection.query("begin", Fetch::default()).unwrap();
        let result = connection.query("rows 1000000", capped(2, true)).unwrap();
        assert_eq!(result.stopped, None, "stopping would end the transaction");
        assert_eq!(result.capped_from, Some(1_000_000));
        assert_eq!(result.rows.len(), 2);

        connection.query("commit", Fetch::default()).unwrap();
        let after = connection.query("rows 1000000", capped(2, true)).unwrap();
        assert_eq!(after.stopped, Some(Stopped::AtLimit));
    }

    #[test]
    fn a_limit_of_zero_keeps_no_rows_and_still_says_how_many_there_were() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection.query("rows 5000", capped(0, false)).unwrap();
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
        let error = connection.query("begin", fetch).err().unwrap();
        assert_eq!(error.message, "Cancelled before it was sent.");
        connection.query("rows 3", Fetch::default()).unwrap();
    }

    #[test]
    fn a_server_error_is_the_statements_and_lost_marks_the_connection() {
        let connection = Connection::open(&config()).unwrap();
        let error = connection.query("fail", Fetch::default()).err().unwrap();
        assert_eq!(error.message, "echo failed, as asked");
        assert!(!connection.is_lost());

        connection.query("lost", Fetch::default()).err().unwrap();
        assert!(connection.is_lost());
    }

    #[test]
    fn result_sets_after_the_first_are_said_and_not_shown() {
        let connection = Connection::open(&config()).unwrap();
        let result = connection.query("sets 3", Fetch::default()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.messages,
            ["2 more result sets followed, which DBDelve does not show through a plugin."]
        );
    }

    #[test]
    fn a_crash_mid_stream_is_a_reset_and_nothing_runs_again() {
        let connection = Connection::open(&config()).unwrap();
        let error = connection.query("crash", Fetch::default()).err().unwrap();
        assert!(error.message.contains("stopped ("), "{}", error.message);
        assert!(
            error.message.contains("committed is unknown"),
            "{}",
            error.message
        );
        assert!(connection.is_lost());

        let again = connection.query("rows 1", Fetch::default()).err().unwrap();
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
            let error = connection.query(statement, Fetch::default()).err().unwrap();
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

    /// `sql` on its own thread, cancelled after `after`: the result, and how
    /// long it took to come back once cancelled.
    fn cancelled(connection: &Connection, sql: &str, after: Duration) -> (DbError, Duration) {
        let running = connection.clone();
        let sql = sql.to_owned();
        let worker = std::thread::spawn(move || running.query(&sql, Fetch::default()));
        std::thread::sleep(after);
        let cancelled = Instant::now();
        connection.cancel().unwrap();
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

        let cancels = connection.query("cancels", Fetch::default()).unwrap();
        assert_eq!(cell(&cancels), Some("1"));
    }

    #[test]
    fn a_cancel_with_nothing_running_sends_nothing() {
        let connection = Connection::open(&config()).unwrap();
        connection.cancel().unwrap();
        connection.query("rows 1", Fetch::default()).unwrap();
        connection.cancel().unwrap();
        let cancels = connection.query("cancels", Fetch::default()).unwrap();
        assert_eq!(cell(&cancels), Some("0"));
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
        use dbdelve_plugin_protocol::limits::{BATCH_TARGET, MAX_FRAME, WINDOW};

        let connection = Connection::open(&config()).unwrap();
        let pid = connection.host.pid().unwrap();
        let feed = Feed::bounded(10);
        let worker = {
            let (connection, feed) = (connection.clone(), feed.clone());
            std::thread::spawn(move || {
                let fetch = Fetch {
                    feed: Some(&feed),
                    ..Fetch::default()
                };
                connection.query("rows 10000000", fetch)
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
        connection.cancel().unwrap();
        let cancelled = Instant::now();
        while connection.host.is_pending(id) && echo::alive(pid) {
            assert!(cancelled.elapsed() < CANCEL_DEADLINE + Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished(), "the consumer was stalled throughout");
        let bound = WINDOW.max(u64::from(MAX_FRAME)) as i64;
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
        let _ = connection.query("exit 3", capped(10, false));
        let error = connection.catalog().unwrap_err();
        assert!(
            error.message.contains("connection was reset"),
            "{}",
            error.message
        );
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

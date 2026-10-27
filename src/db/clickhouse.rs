//! ClickHouse, over its HTTP interface: one POST per statement, the statement
//! as the body byte for byte, every result asked for as
//! `TabSeparatedWithNamesAndTypes`.
//!
//! The user's statements share one server session, so `USE`, `SET` and
//! temporary tables carry from one run to the next. DBDelve's own go without
//! one: a session runs one query at a time, and a preview or a catalog load
//! must neither queue behind a long run nor take its lock.

mod edit;
mod http;
#[cfg(test)]
mod mock;
mod tsv;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use ureq::http::{HeaderMap, Response};
use ureq::{Agent, Body, Timeout};

use self::edit::{
    DESCRIBE_COLUMNS_SQL, DESCRIBE_SETTINGS_SQL, DESCRIBE_TABLE_SQL, Described, Halt, Stage, Table,
    TableFacts, UNCOUNTED, Write, count_refusal, explained, halted, hinted, query_shape, unread,
};
pub(super) use self::edit::{match_value, review_notes};
use self::http::{Bounds, Scheme, Stop, Watch};
use self::tsv::{Framed, Next, Reader, Untagged, is_cut};
use super::ssh::tunnelled;
use super::{
    CancelToken, Cancelling, Catalog, Column, Databases, DbError, EditTarget, Engine, Fetch,
    QueryResult, RelationKind, RowBuilder, ServerConfig, Stopped, Structure, UpdateSpelling,
    assemble_catalog, assemble_databases, assemble_structure, plain_error, required_cell,
    terminated,
};
use crate::sql::{Projected, Unread, bare_words, queued_statements, row_guard, table_read};
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

/// How a killed query's error ends, before the body or in the tagged frame.
const WAS_CANCELLED: &str = "(QUERY_WAS_CANCELLED)";
/// A `KILL` that matched nothing, where `kill_throw_if_noop` makes that an
/// error rather than no rows.
const NOTHING_TO_KILL: &str = "(NOTHING_TO_KILL)";

/// How long a Cancel keeps sending `KILL` to a statement the server has not
/// registered yet, before it lets go of the read instead. Short under test,
/// where it is waited out for real.
const KILL_WITHIN: Duration = match cfg!(test) {
    true => Duration::from_secs(2),
    false => Duration::from_secs(10),
};
const KILL_RETRY: Duration = Duration::from_millis(100);

const UNREACHED: &str =
    "Cancel could not reach the statement on the server; it may still be running.";

const SESSION_ENDED: &str = "The server ended the session after it sat idle; USE, SET and \
                             temporary tables from earlier runs are gone. Run it again to run it \
                             in a new session.";

const DATABASES_SQL: &str = "
SELECT name, name = currentDatabase() AS is_current
FROM system.databases
ORDER BY name";

/// `total_bytes` and `total_rows` are what the server keeps for the table, so
/// they ride along here instead of in `sizes`. Both are NULL for a view and a
/// dictionary, and count rows still waiting for a merge. The `.inner` tables
/// are what a materialized view with no `TO` stores its rows in.
const RELATIONS_SQL: &str = "
SELECT
    database AS schema_name,
    name AS relation_name,
    multiIf(
        engine IN ('View', 'LiveView', 'WindowView'), 'view',
        engine = 'MaterializedView', 'materialized_view',
        engine = 'Dictionary', 'dictionary',
        engine IN ('MySQL', 'PostgreSQL', 'MongoDB', 'S3', 'S3Queue', 'URL', 'HDFS', 'Kafka',
                   'RabbitMQ', 'NATS', 'JDBC', 'ODBC', 'Hudi', 'Redis', 'AzureBlobStorage')
            OR startsWith(engine, 'Iceberg') OR startsWith(engine, 'DeltaLake'), 'foreign_table',
        'table'
    ) AS relation_kind,
    total_bytes AS size_bytes,
    total_rows AS row_estimate
FROM system.tables
WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema')
    AND NOT is_temporary
    AND NOT startsWith(name, '.inner')
ORDER BY database, name";

// The structure queries name one relation, so they carry `{schema}` and
// `{relation}` placeholders substituted by `structure_sql`.

/// A column is nullable when its type is, through `LowCardinality` or not. The
/// three kinds of computed default are written with their keyword, so the grid
/// and New row can tell a column the server fills in from a plain default.
const STRUCTURE_COLUMNS_SQL: &str = "
SELECT
    name AS column_name,
    type AS data_type,
    if(match(type, '^(LowCardinality[(])?Nullable[(]'), 'yes', 'no') AS nullable,
    multiIf(
        default_kind = '', '',
        default_kind = 'DEFAULT', default_expression,
        trim(concat(default_kind, ' ', default_expression))
    ) AS column_default
FROM system.columns
WHERE database = {schema} AND table = {relation}
ORDER BY position";

/// What the table is stored by, then each skip index. A view and a dictionary
/// have none of these keys and so give no row.
const STRUCTURE_INDEXES_SQL: &str = "
SELECT object_name, definition FROM (
    SELECT 0 AS position, 'sorting key' AS object_name,
        concat('ORDER BY (', sorting_key, ')') AS definition
    FROM system.tables
    WHERE database = {schema} AND name = {relation} AND sorting_key != ''
    UNION ALL
    SELECT 1, 'partition key', concat('PARTITION BY ', partition_key)
    FROM system.tables
    WHERE database = {schema} AND name = {relation} AND partition_key != ''
    UNION ALL
    SELECT 2, 'sampling key', concat('SAMPLE BY ', sampling_key)
    FROM system.tables
    WHERE database = {schema} AND name = {relation} AND sampling_key != ''
    UNION ALL
    SELECT 3, name, concat(expr, ' TYPE ', type_full, ' GRANULARITY ', toString(granularity))
    FROM system.data_skipping_indices
    WHERE database = {schema} AND table = {relation}
)
ORDER BY position, object_name";

/// Not unique: ClickHouse takes a repeated primary key. It orders paging and
/// marks the key columns, and is never what an edit matches a row by.
const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT 'PRIMARY' AS object_name, concat('PRIMARY KEY (', primary_key, ')') AS definition
FROM system.tables
WHERE database = {schema} AND name = {relation} AND primary_key != ''";

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
    /// Each request in flight, by query id, so a Cancel that cannot reach a
    /// statement on the server can still end its read here.
    running: Arc<Mutex<HashMap<String, Watch>>>,
    /// What each table allows writing. Emptied when a statement that may
    /// change a table's definition starts, and again when it ends.
    described: Arc<Mutex<Tables>>,
}

#[derive(Default)]
struct Tables {
    /// Each described table, by schema and name.
    known: HashMap<(String, String), Arc<Table>>,
    /// How many times `known` was emptied, so a describe that began before
    /// one does not keep what it may have read from before the change.
    forgotten: u64,
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
    /// A statement that may change the settings the session keeps was sent
    /// on it ([`sets_session`]), so what it reads may be shown otherwise
    /// than a request without the session would show it.
    set: bool,
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
    /// DBDelve's `KILL` of another request, never stopped or killed itself.
    Kill { within: Duration },
    /// The `USE` that puts a new session in the profile's database, on the
    /// session and under the run's Cancel.
    Enter,
}

impl Kind {
    fn bounds(self, statement_timeout: u32) -> Bounds {
        match self {
            Self::Internal | Self::Enter => Bounds::internal(),
            Self::Kill { within } => Bounds::within(within),
            Self::User | Self::Generated | Self::Alongside { .. } => {
                Bounds::user(statement_timeout)
            }
        }
    }
}

/// What a `KILL QUERY … SYNC` found.
#[derive(Debug, PartialEq, Eq)]
enum Kill {
    /// Stopped, or ended before the `KILL` reached it.
    Finished,
    /// Not on the server: not registered yet, or already gone.
    NotFound,
    /// Found and asked to stop, and not stopped yet.
    Waiting,
    CannotCancel,
}

/// Where [`Connection::settle`]'s `KILL`s left a query.
enum Settled {
    Stopped,
    /// Still not found, or found and not stopped, at the deadline.
    Unconfirmed,
    CannotCancel,
    Failed(DbError),
}

/// How a request failed: before any answer, kept whole so the connect can
/// tell a plaintext answer to a TLS handshake; in the server's words before
/// the body; or once the body had begun, with the query perhaps still running.
enum Failed {
    Unreached(ureq::Error),
    Error(DbError),
    Broken(DbError),
    /// Never sent: a Cancel or a hold came first.
    Unsent(DbError),
}

impl From<DbError> for Failed {
    fn from(error: DbError) -> Self {
        Self::Error(error)
    }
}

/// Takes a query id back out of its run's handles, and its watch out of the
/// connection's, however the request ends.
struct Running<'a> {
    token: &'a Mutex<Cancelling>,
    watches: &'a Mutex<HashMap<String, Watch>>,
    query_id: String,
}

impl<'a> Running<'a> {
    /// Registered before the request is sent, so a Cancel from then on finds
    /// it; a Cancel already asked drops it unsent.
    fn register(
        cancel: &'a CancelToken,
        watches: &'a Mutex<HashMap<String, Watch>>,
        query_id: &str,
        watch: &Watch,
    ) -> Result<Self, DbError> {
        let mut token = lock(&cancel.0);
        if token.asked {
            return Err(plain_error(CANCELLED.into()));
        }
        // The watch first: a Cancel that finds the handle finds the watch.
        lock(watches).insert(query_id.to_string(), watch.clone());
        token.handles.push(query_id.to_string());
        Ok(Self {
            token: &cancel.0,
            watches,
            query_id: query_id.to_string(),
        })
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        lock(self.token)
            .handles
            .retain(|handle| *handle != self.query_id);
        lock(self.watches).remove(&self.query_id);
    }
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
            running: Arc::default(),
            described: Arc::default(),
        }
    }

    pub fn query(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        self.forget_if_changing(sql);
        let result = self.request(sql, Kind::User, cancel, fetch);
        self.forget_if_changing(sql);
        let mut result = result.map_err(hinted)?;
        (result.edit, result.read_only_reason) =
            self.edit_of_query(sql, &result.columns, Kind::User, cancel);
        Ok(result)
    }

    /// A preview of a table also says whether its rows can be written back, or
    /// why not; see [`Self::edit_of`]. A submission of more than one
    /// statement, or of a write that names rows, goes out as [`Self::writes`].
    pub fn generated(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        let statements = queued_statements(Engine::ClickHouse, sql, None);
        if statements.len() > 1 || statements.iter().any(|at| guarded(&sql[at.clone()])) {
            return self.writes(sql, &statements, cancel);
        }
        let mut result = self.request(sql, Kind::Generated, cancel, fetch)?;
        if let Some((schema, table)) = preview_of(sql) {
            (result.edit, result.read_only_reason) = self.edit_of(&schema, &table, &result.columns);
        }
        Ok(result)
    }

    /// Each statement in turn, one per request as the HTTP interface takes
    /// them, stopping at the first that is refused or fails. ClickHouse has no
    /// transaction, so the ones before it stay, and the error says so.
    fn writes(
        &self,
        sql: &str,
        statements: &[Range<usize>],
        cancel: &CancelToken,
    ) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let mut affected: Option<u64> = None;
        for (at, range) in statements.iter().enumerate() {
            let statement = &sql[range.clone()];
            let ran = match guarded(statement) {
                true => self.guarded_write(statement, cancel),
                false => self.submit(statement, cancel),
            };
            match ran {
                Ok(result) => {
                    if let Some(rows) = result.rows_affected {
                        affected = Some(affected.unwrap_or(0) + rows);
                    }
                }
                Err(halt) => {
                    return Err(halted(at, statements.len(), halt, statement, range.start));
                }
            }
        }
        Ok(QueryResult {
            elapsed: started.elapsed(),
            rows_affected: affected,
            ..QueryResult::default()
        })
    }

    /// `statement` only once its guard, cut from the same tree the write gate
    /// reads, counts exactly one row. Between the count and the write another
    /// client can still add an identical row or change this one; nothing in
    /// ClickHouse closes that window.
    fn guarded_write(&self, statement: &str, cancel: &CancelToken) -> Result<QueryResult, Halt> {
        let uncounted = || Halt::Refused(UNCOUNTED.into());
        let guard = row_guard(Engine::ClickHouse, statement).ok_or_else(uncounted)?;
        let (schema, table) = counted_table(&guard).ok_or_else(uncounted)?;
        if let Some(described) = self
            .describe(&schema, &table)
            .map_err(|error| Halt::Failed(Stage::Describe, error))?
            && let Some(reason) = described.refuses(write_of(statement))
        {
            return Err(Halt::Refused(reason));
        }
        let counted = self
            .request(&guard, Kind::Generated, cancel, Fetch::default())
            .map_err(|error| Halt::Failed(Stage::Count, error))?;
        let count = counted
            .rows
            .first()
            .and_then(|row| row.get(0)?.parse::<u64>().ok())
            .ok_or_else(uncounted)?;
        if let Some(reason) = count_refusal(count) {
            return Err(Halt::Refused(reason));
        }
        self.submit(statement, cancel).map_err(|halt| match halt {
            Halt::Failed(Stage::Write, error) => Halt::Failed(Stage::Write, explained(error)),
            halt => halt,
        })
    }

    /// One statement of a submission, and whether it failed before it went
    /// out.
    fn submit(&self, statement: &str, cancel: &CancelToken) -> Result<QueryResult, Halt> {
        self.run(statement, Kind::Generated, cancel, Fetch::default())
            .map_err(|failed| match failed {
                Failed::Unsent(error) => Halt::Failed(Stage::Unsent, error),
                failed => Halt::Failed(Stage::Write, self.explain(failed)),
            })
    }

    /// The edit target of a preview's result, or the reason it has none. The
    /// statement has already succeeded, so a describe that fails answers
    /// neither: the rows stay unwritable without a reason.
    fn edit_of(
        &self,
        schema: &str,
        table: &str,
        columns: &[Column],
    ) -> (Option<EditTarget>, Option<String>) {
        match self.describe(schema, table) {
            Ok(Some(described)) => described.target(schema, table, columns),
            _ => (None, None),
        }
    }

    /// The edit target of the rows a user's statement read, as `kind` ran it,
    /// or the reason it has none. Only a select of one table's own columns
    /// has one ([`query_shape`]). The statement has already succeeded, so a
    /// lookup that fails answers neither.
    fn edit_of_query(
        &self,
        sql: &str,
        columns: &[Column],
        kind: Kind,
        cancel: &CancelToken,
    ) -> (Option<EditTarget>, Option<String>) {
        if columns.is_empty() {
            return (None, None);
        }
        let refused = |reason: &str| (None, Some(reason.to_string()));
        let (read, shadows) = match query_shape(sql) {
            Ok(read) => read,
            Err(reason) => return refused(reason),
        };
        // The guard and the write go without the session, and match each
        // value as the text it was shown as: after a `SET session_timezone`,
        // say, that text would name another instant, and so another row.
        if matches!(kind, Kind::User) && lock(&self.session).set {
            return refused(edit::SESSION_SET);
        }
        let (schema, table) = match read.table.as_slice() {
            [schema, table] => (schema.clone(), table.clone()),
            [table] => match self.resolve(table, kind, cancel) {
                Ok(Some(schema)) => (schema, table.clone()),
                Ok(None) => return refused(edit::TEMPORARY),
                Err(_) => return (None, None),
            },
            _ => return refused(unread(Unread::NotATable)),
        };
        let described = match self.describe(&schema, &table) {
            Ok(Some(described)) => described,
            Ok(None) => {
                return refused(&format!(
                    "DBDelve found no table {schema}.{table}, so these rows cannot be edited."
                ));
            }
            Err(_) => return (None, None),
        };
        if let Some(reason) = &described.refusal {
            return refused(reason);
        }
        match edit::origins(&read, &shadows, columns, &described, &table) {
            Ok(origins) => described.target_of(&schema, &table, columns, &origins),
            Err(reason) => (None, Some(reason)),
        }
    }

    /// The database an unqualified `table` was read from, as `kind` reads it
    /// (a `USE` of the user's moves the session's), or `None` where it names
    /// a temporary table of the session's, which is read first and no other
    /// request can see.
    fn resolve(
        &self,
        table: &str,
        kind: Kind,
        cancel: &CancelToken,
    ) -> Result<Option<String>, DbError> {
        let sql = format!(
            "SELECT currentDatabase() AS database, \
             (SELECT count() FROM system.tables WHERE is_temporary AND name = {}) AS temporary",
            Engine::ClickHouse.quote_literal(table)
        );
        let read = self.request(&sql, kind, cancel, Fetch::default())?;
        let row = read
            .rows
            .first()
            .ok_or_else(|| plain_error("The current database could not be read.".into()))?;
        (required_cell(&read, row, "temporary")? == "0")
            .then(|| required_cell(&read, row, "database").map(str::to_string))
            .transpose()
    }

    /// One sessionless look at `schema.table`, kept for the connection. `None`
    /// where the server has no such table.
    fn describe(&self, schema: &str, table: &str) -> Result<Option<Arc<Table>>, DbError> {
        let key = (schema.to_string(), table.to_string());
        let began = {
            let described = lock(&self.described);
            if let Some(known) = described.known.get(&key) {
                return Ok(Some(known.clone()));
            }
            described.forgotten
        };
        let ask = |template| self.internal_query(&structure_sql(template, schema, table));
        let tables = ask(DESCRIBE_TABLE_SQL)?;
        let Some(row) = tables.rows.first() else {
            return Ok(None);
        };
        let columns = ask(DESCRIBE_COLUMNS_SQL)?;
        let settings = ask(DESCRIBE_SETTINGS_SQL)?;
        let facts = TableFacts {
            engine: required_cell(&tables, row, "engine")?.to_string(),
            engine_full: required_cell(&tables, row, "engine_full")?.to_string(),
            create_table_query: required_cell(&tables, row, "create_table_query")?.to_string(),
            columns: columns
                .rows
                .iter()
                .map(|row| {
                    Ok(Described {
                        name: required_cell(&columns, row, "name")?.to_string(),
                        data_type: required_cell(&columns, row, "type")?.to_string(),
                        default_kind: required_cell(&columns, row, "default_kind")?.to_string(),
                        in_key: required_cell(&columns, row, "in_key")? == "1",
                    })
                })
                .collect::<Result<_, DbError>>()?,
            server: settings
                .rows
                .iter()
                .map(|row| {
                    Ok((
                        required_cell(&settings, row, "name")?.to_string(),
                        required_cell(&settings, row, "value")?.to_string(),
                    ))
                })
                .collect::<Result<_, DbError>>()?,
        };
        let table = Arc::new(Table::of(facts));
        let mut described = lock(&self.described);
        if described.forgotten == began {
            described.known.insert(key, table.clone());
        }
        Ok(Some(table))
    }

    /// A definition may have changed under any statement that does not open
    /// as a plain read, so what was described is asked again. Read off the
    /// first word alone: a statement can be megabytes of `VALUES`, and
    /// parsing it here would delay its send.
    ///
    /// Called as the statement starts and again as it ends: a describe from
    /// another tab, which takes no session, can run while an `ALTER` does and
    /// read the settings from before it.
    fn forget_if_changing(&self, sql: &str) {
        if !opens_with(sql, &READS) {
            let mut described = lock(&self.described);
            described.known.clear();
            described.forgotten += 1;
        }
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
        let kind = Kind::Alongside { read_only };
        self.forget_if_changing(sql);
        let result = self.request(sql, kind, cancel, fetch);
        self.forget_if_changing(sql);
        let mut result = result.map_err(hinted)?;
        (result.edit, result.read_only_reason) =
            self.edit_of_query(sql, &result.columns, kind, cancel);
        Ok(result)
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
        assemble_catalog(self.internal_query(RELATIONS_SQL)?, QueryResult::default())
    }

    pub fn databases(&self) -> Result<Databases, DbError> {
        assemble_databases(&self.internal_query(DATABASES_SQL)?)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let columns =
            self.internal_query(&structure_sql(STRUCTURE_COLUMNS_SQL, schema, relation))?;
        let indexes =
            self.internal_query(&structure_sql(STRUCTURE_INDEXES_SQL, schema, relation))?;
        let constraints =
            self.internal_query(&structure_sql(STRUCTURE_CONSTRAINTS_SQL, schema, relation))?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        for constraint in &mut structure.constraints {
            if let Some(columns) = constraint
                .definition
                .strip_prefix("PRIMARY KEY (")
                .and_then(|list| list.strip_suffix(')'))
                .and_then(key_columns)
            {
                constraint.definition = format!("PRIMARY KEY ({})", columns.join(", "));
            }
        }
        Ok(structure)
    }

    pub fn ddl(
        &self,
        schema: &str,
        relation: &str,
        _kind: RelationKind,
    ) -> Result<String, DbError> {
        let name = format!(
            "{}.{}",
            Engine::ClickHouse.quote_identifier(schema),
            Engine::ClickHouse.quote_identifier(relation)
        );
        let result = self.internal_query(&format!("SHOW CREATE TABLE {name}"))?;
        result
            .rows
            .first()
            .and_then(|row| row.get(0))
            .map(terminated)
            .ok_or_else(|| plain_error(format!("{name} has no definition to show.")))
    }

    /// Stop the statements running under `cancel`, side by side, each by
    /// `KILL`. Without a session, which would queue it behind the very
    /// statement it is for.
    ///
    /// The query id is chosen here, so a `KILL` can arrive before the server
    /// has registered the request it names, and find nothing: it is sent
    /// again until it lands or the request has returned. One that never
    /// lands ends the read here instead, and says the statement may still be
    /// running; the run itself ends as cancelled either way.
    pub fn cancel(&self, cancel: &CancelToken) -> Result<(), DbError> {
        let handles = {
            let mut token = lock(&cancel.0);
            token.asked = true;
            token.handles.clone()
        };
        std::thread::scope(|scope| {
            let stops: Vec<_> = handles
                .iter()
                .map(|query_id| scope.spawn(|| self.stop(cancel, query_id)))
                .collect();
            stops
                .into_iter()
                .map(|stop| {
                    stop.join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .fold(Ok(()), Result::and)
        })
    }

    fn stop(&self, cancel: &CancelToken, query_id: &str) -> Result<(), DbError> {
        let on_token = || lock(&cancel.0).handles.iter().any(|id| id == query_id);
        let failed = match self.settle(query_id, false, on_token) {
            Settled::Stopped => return Ok(()),
            Settled::Failed(error) => Some(error),
            Settled::Unconfirmed | Settled::CannotCancel => None,
        };
        if !on_token() {
            return Ok(());
        }
        if let Some(watch) = lock(&self.running).get(query_id) {
            watch.abort();
        }
        Err(plain_error(match failed {
            Some(error) => format!("{UNREACHED} The KILL QUERY failed: {}", error.message),
            None => UNREACHED.into(),
        }))
    }

    /// `KILL`s `query_id` until it is known to be over, for at most
    /// [`KILL_WITHIN`], while `wanted` holds; a query no longer wanted counts
    /// as stopped. A query the server has `answered` for was registered
    /// before it answered, so not finding it means it is over; one it has not
    /// answered may be on its way, and is looked for again.
    fn settle(&self, query_id: &str, answered: bool, wanted: impl Fn() -> bool) -> Settled {
        let deadline = Instant::now() + KILL_WITHIN;
        loop {
            if !wanted() {
                return Settled::Stopped;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Settled::Unconfirmed;
            }
            match self.kill(query_id, left) {
                Ok(Kill::Finished) => return Settled::Stopped,
                Ok(Kill::NotFound) if answered => return Settled::Stopped,
                Ok(Kill::NotFound | Kill::Waiting) if Instant::now() + KILL_RETRY < deadline => {
                    std::thread::sleep(KILL_RETRY);
                }
                Ok(Kill::NotFound | Kill::Waiting) => return Settled::Unconfirmed,
                Ok(Kill::CannotCancel) => return Settled::CannotCancel,
                // An attempt that ran out of the time left is the deadline
                // arriving, not the server failing it.
                Err(_) if Instant::now() >= deadline => return Settled::Unconfirmed,
                Err(error) => return Settled::Failed(error),
            }
        }
    }

    /// One `KILL QUERY … SYNC`, its outcome read from the row it returns.
    /// `query_id` is one of ours, `dbdelve-` and hex, so it is spliced as is.
    fn kill(&self, query_id: &str, within: Duration) -> Result<Kill, DbError> {
        let sql = format!("KILL QUERY WHERE query_id = '{query_id}' SYNC");
        let found = match self.request(
            &sql,
            Kind::Kill { within },
            &CancelToken::default(),
            Fetch::default(),
        ) {
            Err(error) if error.message.ends_with(NOTHING_TO_KILL) => return Ok(Kill::NotFound),
            found => found?,
        };
        let Some(row) = found.rows.first() else {
            return Ok(Kill::NotFound);
        };
        Ok(match required_cell(&found, row, "kill_status")? {
            "finished" => Kill::Finished,
            "cant_cancel" => Kill::CannotCancel,
            _ => Kill::Waiting,
        })
    }

    /// Kills a statement whose response was given up on: dropping the
    /// connection alone does not stop it on the server. `answered` as in
    /// [`Self::settle`].
    fn abandon(&self, query_id: &str, answered: bool) -> Result<(), DbError> {
        match self.settle(query_id, answered, || true) {
            Settled::Stopped => Ok(()),
            Settled::Unconfirmed => Err(plain_error(
                "the server did not confirm that it stopped".into(),
            )),
            Settled::CannotCancel => Err(plain_error("the server cannot cancel it".into())),
            Settled::Failed(error) => Err(error),
        }
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
        fetch.hold().map_err(Failed::Unsent)?;
        if let Some(session) = session.as_deref_mut() {
            if session.id.is_none() {
                *session = Session {
                    id: Some(random_id()?),
                    opened: false,
                    default_timeout: session.default_timeout,
                    set: false,
                };
            }
            self.enter(session, cancel)?;
            session.set |= sets_session(sql);
        }
        self.send(sql, kind, cancel, fetch, session.as_deref_mut(), started)
    }

    /// One statement, under `cancel` and its own watch, and on `session` when
    /// there is one.
    fn send(
        &self,
        sql: &str,
        kind: Kind,
        cancel: &CancelToken,
        fetch: Fetch,
        mut session: Option<&mut Session>,
        started: Instant,
    ) -> Result<QueryResult, Failed> {
        let query_id = random_id()?;
        let read_only = match kind {
            Kind::Alongside { read_only } => read_only,
            Kind::Kill { .. } => false,
            _ => self.read_only.load(Ordering::SeqCst),
        };
        let bounds = kind.bounds(self.config.statement_timeout);
        let watch = Watch::new(Arc::default(), bounds.limit());
        let _running =
            Running::register(cancel, &self.running, &query_id, &watch).map_err(Failed::Unsent)?;
        let _watching = watch.enter();
        let outcome = loop {
            let params = self.params(&query_id, session.as_deref(), read_only);
            let response = match self.post(sql, &params, bounds) {
                Ok(response) => response,
                Err(error) => break Err(Failed::Unreached(error)),
            };
            let status = response.status().as_u16();
            if !http::failed(status, response.headers()) {
                if let Some(session) = session.as_deref_mut() {
                    session.opened = true;
                }
                break self
                    .read(response, sql, &query_id, fetch, started)
                    .map_err(Failed::Broken);
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
                        break Err(plain_error(SESSION_ENDED.into()).into());
                    }
                    _ => {}
                }
            }
            break Err(failure(response, sql).into());
        };
        match kind {
            Kind::Kill { .. } => outcome,
            _ => outcome.map_err(|failed| self.ended(failed, &query_id, cancel, &watch, bounds)),
        }
    }

    /// What a request that did not end cleanly reports, and the `KILL` it
    /// owes when it may still be running.
    ///
    /// `DbError` has no code for `is_cancel` to read, so the cancelled ends
    /// are made [`CANCELLED`] here: a run whose Cancel was asked that ended
    /// in the server's `QUERY_WAS_CANCELLED`, in a body cut off, or without
    /// an answer, and a read the Cancel had to let go of. A
    /// `QUERY_WAS_CANCELLED` nobody here asked for keeps the server's words.
    fn ended(
        &self,
        failed: Failed,
        query_id: &str,
        cancel: &CancelToken,
        watch: &Watch,
        bounds: Bounds,
    ) -> Failed {
        let asked = lock(&cancel.0).asked;
        let cancelled = || Failed::Error(plain_error(CANCELLED.into()));
        let idle = watch.stopped() == Some(Stop::Idle)
            || matches!(
                failed,
                Failed::Unreached(ureq::Error::Timeout(Timeout::RecvResponse))
            );
        let answered = !matches!(failed, Failed::Unreached(_));
        match failed {
            _ if watch.stopped() == Some(Stop::Aborted) => cancelled(),
            Failed::Error(error) | Failed::Broken(error)
                if asked && error.message.ends_with(WAS_CANCELLED) =>
            {
                cancelled()
            }
            Failed::Broken(error) if asked && is_cut(&error) => cancelled(),
            Failed::Unreached(_) if asked => cancelled(),
            _ if idle => {
                let seconds = bounds.limit().unwrap_or_default().as_secs();
                let message = match self.abandon(query_id, answered) {
                    Ok(()) => format!(
                        "The server sent nothing for {seconds} seconds; the statement was stopped."
                    ),
                    Err(error) => format!(
                        "The server sent nothing for {seconds} seconds, and stopping the \
                         statement failed ({}); it may still be running.",
                        error.message
                    ),
                };
                Failed::Error(plain_error(message))
            }
            Failed::Broken(error) if !asked => {
                Failed::Broken(self.abandoned(error, query_id, true))
            }
            // Lost after the request went out, before any answer: the server
            // may have it, and dropping the connection does not stop it. A
            // connect or handshake that failed sent nothing, and `prefer`
            // reads its error whole.
            Failed::Unreached(error) if watch.sent() => {
                let error = plain_error(format!("{} did not answer: {error}.", self.endpoint()));
                Failed::Broken(self.abandoned(error, query_id, false))
            }
            failed => failed,
        }
    }

    /// `error`, and when the `KILL` it owes fails, that it may still be
    /// running.
    fn abandoned(&self, error: DbError, query_id: &str, answered: bool) -> DbError {
        match self.abandon(query_id, answered) {
            Ok(()) => error,
            Err(stop) => plain_error(format!(
                "{} It may still be running: stopping it failed too ({}).",
                error.message, stop.message
            )),
        }
    }

    /// Puts a new session in the profile's database. The `database` parameter
    /// alone does not: it holds for its own request and the session stays in
    /// `default`, so the user's second statement would not find the tables the
    /// first did. A `USE` does, and a later `USE` of the user's is then the
    /// session's own, which a per-request parameter would override.
    ///
    /// The `USE` is a request of the run's like any other: a Cancel finds and
    /// stops it, and its failure fails the run before the statement is sent.
    /// A statement sent anyway would mark the session open while it is still
    /// in `default`, where every later one would then run.
    fn enter(&self, session: &mut Session, cancel: &CancelToken) -> Result<(), Failed> {
        let database = self.config.database.trim();
        if session.opened || database.is_empty() {
            return Ok(());
        }
        let sql = format!("USE {}", Engine::ClickHouse.quote_identifier(database));
        let entered = self.send(
            &sql,
            Kind::Enter,
            cancel,
            Fetch::default(),
            Some(&mut *session),
            Instant::now(),
        );
        // Open from its headers, but in the database only once its body has
        // ended cleanly.
        if entered.is_err() {
            session.opened = false;
        }
        entered.map(drop)
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
            .timeout_global(bounds.whole())
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
        query_id: &str,
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
        let mut result = collect(&mut reader, fetch, started)?;
        if result.stopped == Some(Stopped::AtLimit) {
            // Closed before the `KILL`: a server blocked writing to a socket
            // nobody reads would not get to the cancel.
            drop(reader);
            if let Err(error) = self.abandon(query_id, true) {
                result.notice = Some(format!(
                    "Stopping the statement at the row limit failed ({}); it may still be \
                     running on the server.",
                    error.message
                ));
            }
        }
        Ok(result)
    }

    fn endpoint(&self) -> String {
        format!("{}:{}", self.config.host.trim(), http::port(&self.config))
    }

    fn explain(&self, failed: Failed) -> DbError {
        let error = match failed {
            Failed::Error(error) | Failed::Broken(error) | Failed::Unsent(error) => return error,
            Failed::Unreached(error) => error,
        };
        let endpoint = self.endpoint();
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
/// limit the first set is drained to its end and counted, never kept, or for
/// a read that may stop there, left unread for the caller to `KILL`.
fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    let mut sql = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        sql.push_str(&rest[..start]);
        rest = &rest[start..];
        let (placeholder, value) = if rest.starts_with("{schema}") {
            ("{schema}", schema)
        } else if rest.starts_with("{relation}") {
            ("{relation}", relation)
        } else {
            sql.push('{');
            rest = &rest[1..];
            continue;
        };
        sql.push_str(&Engine::ClickHouse.quote_literal(value));
        rest = &rest[placeholder.len()..];
    }
    sql.push_str(rest);
    sql
}

/// The first words of the statements that only read.
const READS: [&str; 7] = [
    "SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "EXISTS",
];

/// Whether `sql`'s first word is one of `words`, read off that word alone.
fn opens_with(sql: &str, words: &[&str]) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    words.iter().any(|word| first.eq_ignore_ascii_case(word))
}

/// Whether `statement` may change a setting the session it runs on keeps: a
/// `SET` (`SET ROLE` among them, a role's profile can carry settings) or an
/// `EXECUTE AS`, read past any comment. One the tokenizer cannot read counts.
/// Tokenized only when it does not open with a word, so a megabyte of
/// `VALUES` is not read twice on its way out.
fn sets_session(statement: &str) -> bool {
    const SETS: [&str; 2] = ["SET", "EXECUTE"];
    if statement
        .trim_start()
        .starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return opens_with(statement, &SETS);
    }
    bare_words(Engine::ClickHouse, statement).is_none_or(|words| {
        words
            .first()
            .is_some_and(|first| SETS.iter().any(|word| first.eq_ignore_ascii_case(word)))
    })
}

/// Whether a generated statement runs only behind a guard: anything but a
/// read or an `INSERT`, so one this cannot place is refused rather than sent
/// unguarded.
fn guarded(statement: &str) -> bool {
    !opens_with(statement, &READS) && !opens_with(statement, &["INSERT"])
}

/// What a statement the write gate admitted, and so one of its three
/// spellings, does.
fn write_of(statement: &str) -> Write {
    if opens_with(statement, &["DELETE"]) {
        Write::Delete
    } else if opens_with(statement, &["ALTER"]) {
        Write::Update(UpdateSpelling::AlterUpdate)
    } else {
        Write::Update(UpdateSpelling::Update)
    }
}

/// The schema and table a guard counts in, which are the write's own.
fn counted_table(guard: &str) -> Option<(String, String)> {
    match table_read(Engine::ClickHouse, guard)?.table.as_slice() {
        [schema, table] => Some((schema.clone(), table.clone())),
        _ => None,
    }
}

/// The schema and table of a `SELECT *` from one table, written with both
/// names as DBDelve's previews are.
fn preview_of(sql: &str) -> Option<(String, String)> {
    let read = table_read(Engine::ClickHouse, sql)?;
    match (read.table.as_slice(), read.columns.as_slice()) {
        ([schema, table], [Projected::Every]) => Some((schema.clone(), table.clone())),
        _ => None,
    }
}

/// The columns of a key the server printed as SQL: `system.tables` gives
/// `user id` as `` `user id` ``, with backslash escapes inside. `None` when
/// any part is an expression rather than a bare or quoted name, so the key is
/// left as written and never taken for columns.
fn key_columns(list: &str) -> Option<Vec<String>> {
    let list = match list
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        Some(inner) => inner,
        None => list,
    };
    let mut names = Vec::new();
    let mut chars = list.trim().chars().peekable();
    loop {
        let name = match chars.peek()? {
            quote @ ('`' | '"') => {
                let quote = *quote;
                chars.next();
                let mut name = String::new();
                loop {
                    match chars.next()? {
                        '\\' => name.push(chars.next()?),
                        c if c == quote => break,
                        c => name.push(c),
                    }
                }
                name
            }
            c if c.is_alphabetic() || *c == '_' => {
                let mut name = String::new();
                while let Some(c) = chars.next_if(|c| c.is_alphanumeric() || *c == '_') {
                    name.push(c);
                }
                name
            }
            _ => return None,
        };
        names.push(name);
        match chars.next() {
            None => return Some(names),
            Some(',') => while chars.next_if(|c| *c == ' ').is_some() {},
            Some(_) => return None,
        }
    }
}

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
    let mut at_limit = false;
    let mut builder = RowBuilder::default();
    let mut drained = None;
    loop {
        let next = match drained.take() {
            Some(next) => next,
            None => reader.next_row(&mut builder)?,
        };
        match next {
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
                    if first && fetch.reads_only {
                        at_limit = true;
                        break;
                    }
                    if first {
                        let (rows, next) = reader.drain()?;
                        returned += rows;
                        drained = Some(next);
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
    if at_limit {
        // How many rows there were is what stopping does not find out.
        result.capped_from = None;
        result.stopped = Some(Stopped::AtLimit);
    }
    result.elapsed = match filled {
        Some(filled) if result.capped_from.is_some() || at_limit => filled,
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
    use super::mock::{Aftermath, Mock, Registration, Request, Response};
    use super::*;
    use crate::db::{Column, Engine, Feed, is_cancel};
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
        mock.on_statement("USE `", [Response::empty()]);
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
    fn a_users_statements_open_one_session_in_the_database_then_check_it_is_still_there() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        run(&connection, "SELECT n").unwrap();
        let [enter] = sent(&mock, "USE `").try_into().unwrap();
        assert_eq!(enter.statement, "USE `dbdelve_dev`");
        let session = enter.param("session_id").unwrap();
        assert!(session.starts_with("dbdelve-"), "{session}");
        assert_eq!(
            names(&enter),
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
        assert_eq!(enter.param("session_timeout"), Some(SESSION_TIMEOUT));
        // Only a `USE` puts a session in a database, and a per-request
        // `database` would override the user's own.
        let [first, second] = sent(&mock, "SELECT n").try_into().unwrap();
        for request in [&first, &second] {
            assert_eq!(request.param("session_id"), Some(session));
            assert_eq!(request.param("database"), None);
            assert_eq!(request.param("session_check"), Some("1"));
        }
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
        assert_eq!(ids.len(), 5);
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
        let [_, enter_fresh] = sent(&mock, "USE `").try_into().unwrap();
        assert_eq!(expired.param("session_id"), first.param("session_id"));
        assert_ne!(fresh.param("session_id"), first.param("session_id"));
        assert_eq!(enter_fresh.param("session_id"), fresh.param("session_id"));
        assert_eq!(enter_fresh.param("session_check"), None);
        assert_eq!(enter_fresh.param("database"), Some("dbdelve_dev"));
        assert_eq!(fresh.param("session_check"), Some("1"));
    }

    #[test]
    fn a_use_that_fails_fails_the_run_unsent_and_the_next_run_enters_again() {
        for failure in [Response::text(502, "Bad Gateway"), Response::dropped()] {
            let mock = Mock::start();
            let connection = open(&mock);
            numbers(&mock);
            mock.on_statement("USE `", [failure, Response::empty()]);
            assert!(run(&connection, "SELECT n").is_err());
            assert!(sent(&mock, "SELECT n").is_empty());
            // Unqualified, so each would run in `default` had the session
            // been taken as open without its `USE`.
            run(&connection, "SELECT n").unwrap();
            run(&connection, "SELECT n").unwrap();
            let [failed, entered] = sent(&mock, "USE `").try_into().unwrap();
            assert_eq!(entered.param("session_id"), failed.param("session_id"));
            assert_eq!(entered.param("session_check"), None);
            assert_eq!(entered.param("database"), Some("dbdelve_dev"));
            for request in sent(&mock, "SELECT n") {
                assert_eq!(request.param("session_id"), entered.param("session_id"));
                assert_eq!(request.param("session_check"), Some("1"));
            }
        }
    }

    #[test]
    fn a_session_timeout_over_the_servers_ceiling_is_dropped_and_the_open_sent_again() {
        let mock = Mock::start();
        mock.max_session_timeout(60);
        let connection = open(&mock);
        numbers(&mock);
        assert_eq!(run(&connection, "SELECT n").unwrap().rows.len(), 2);
        run(&connection, "SELECT n").unwrap();
        let [refused, retried] = sent(&mock, "USE `").try_into().unwrap();
        assert_eq!(refused.param("session_timeout"), Some(SESSION_TIMEOUT));
        assert_eq!(retried.param("session_timeout"), None);
        assert_eq!(retried.param("session_id"), refused.param("session_id"));
        assert_eq!(retried.param("session_check"), None);
        for request in sent(&mock, "SELECT n") {
            assert_eq!(request.param("session_timeout"), None);
            assert_eq!(request.param("session_check"), Some("1"));
        }
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

    const PREVIEW: &str = "SELECT * FROM `d`.`t` LIMIT 100";

    fn described(mock: &Mock, engine: &str, engine_full: &str) {
        mock.on_statement(
            "FROM `d`.`t`",
            [Response::rows(
                &[("id", "UInt32"), ("label", "String")],
                &[&["1", "a"]],
            )],
        );
        mock.on_statement(
            "FROM system.tables",
            [Response::rows(
                &[
                    ("engine", "String"),
                    ("engine_full", "String"),
                    ("create_table_query", "String"),
                ],
                &[&[engine, engine_full, "CREATE TABLE d.t (id UInt32)"]],
            )],
        );
        mock.on_statement(
            "FROM system.columns",
            [Response::rows(
                &[
                    ("name", "String"),
                    ("type", "String"),
                    ("default_kind", "String"),
                    ("in_key", "UInt8"),
                ],
                &[&["id", "UInt32", "", "1"], &["label", "String", "", "0"]],
            )],
        );
        mock.on_statement(
            "FROM system.merge_tree_settings",
            [Response::rows(
                &[("name", "String"), ("value", "String")],
                &[&["server.enable_lightweight_update", "1"]],
            )],
        );
    }

    fn preview(connection: &Connection, sql: &str) -> QueryResult {
        connection
            .generated(sql, &CancelToken::default(), Fetch::default())
            .unwrap()
    }

    #[test]
    fn a_preview_of_a_merge_tree_carries_its_edit_target_described_once() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(
            &mock,
            "MergeTree",
            "MergeTree ORDER BY id SETTINGS enable_block_number_column = 1, \
             enable_block_offset_column = 1",
        );
        let result = preview(&connection, PREVIEW);
        let target = result.edit.expect("a MergeTree table is editable");
        assert_eq!(result.read_only_reason, None);
        assert_eq!(target.update, crate::db::UpdateSpelling::Update);
        assert_eq!(
            target.row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![0] }
        );
        preview(&connection, PREVIEW);
        assert_eq!(mock.hits("FROM system.columns"), 1);
        assert!(
            mock.requests()
                .iter()
                .filter(|request| request.statement.contains("FROM system."))
                .all(|request| request.param("session_id").is_none())
        );
    }

    #[test]
    fn a_table_that_is_not_a_merge_tree_says_why_its_rows_are_read_only() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "Memory", "Memory");
        let result = preview(&connection, PREVIEW);
        assert_eq!(result.edit, None);
        assert_eq!(
            result.read_only_reason.as_deref(),
            Some("ClickHouse edits rows of MergeTree tables only.")
        );
    }

    #[test]
    fn only_a_select_star_of_a_qualified_table_is_described() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement("SELECT", [Response::rows(&[("id", "UInt32")], &[&["1"]])]);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        for sql in ["SELECT id FROM `d`.`t`", "SELECT * FROM `t`"] {
            let result = preview(&connection, sql);
            assert_eq!(
                (result.edit, result.read_only_reason),
                (None, None),
                "{sql}"
            );
        }
        assert_eq!(mock.hits("FROM system.tables"), 0);
    }

    #[test]
    fn a_statement_that_may_change_a_table_drops_what_was_described() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement("ALTER", [Response::empty()]);
        mock.on_statement("SELECT 7", [Response::rows(&[("7", "UInt8")], &[&["7"]])]);
        preview(&connection, PREVIEW);
        run(&connection, "SELECT 7").unwrap();
        preview(&connection, PREVIEW);
        assert_eq!(mock.hits("FROM system.tables"), 1);

        run(
            &connection,
            "ALTER TABLE d.t MODIFY SETTING enable_block_number_column = 1",
        )
        .unwrap();
        preview(&connection, PREVIEW);
        assert_eq!(mock.hits("FROM system.tables"), 2);
    }

    const MODIFY: &str = "ALTER TABLE d.t MODIFY SETTING enable_block_number_column = 1";

    #[test]
    fn a_describe_while_a_definition_changes_is_asked_again_once_the_change_ends() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement("ALTER", [Response::empty().held("alter")]);
        std::thread::scope(|scope| {
            let altering = scope.spawn(|| run(&connection, MODIFY));
            until("the ALTER to arrive", || mock.hits("ALTER") == 1);
            preview(&connection, PREVIEW);
            assert_eq!(mock.hits("FROM system.tables"), 1);
            mock.release("alter");
            altering.join().unwrap().unwrap();
        });
        preview(&connection, PREVIEW);
        assert_eq!(mock.hits("FROM system.tables"), 2);
    }

    #[test]
    fn a_describe_begun_before_a_change_ended_keeps_nothing_it_read() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement(
            "FROM system.merge_tree_settings",
            [Response::rows(
                &[("name", "String"), ("value", "String")],
                &[&["server.enable_lightweight_update", "1"]],
            )
            .held("settings")],
        );
        mock.on_statement("ALTER", [Response::empty()]);
        std::thread::scope(|scope| {
            let previewing = scope.spawn(|| preview(&connection, PREVIEW));
            until("the describe to read the settings", || {
                mock.hits("FROM system.merge_tree_settings") == 1
            });
            run(&connection, MODIFY).unwrap();
            mock.release("settings");
            previewing.join().unwrap();
        });
        preview(&connection, PREVIEW);
        assert_eq!(mock.hits("FROM system.tables"), 2);
    }

    const QUERIED: &str = "SELECT label, id FROM d.t";

    /// `d.t` as a query tab reads it, its columns the other way round.
    fn queried(mock: &Mock) {
        mock.on_statement(
            QUERIED,
            [Response::rows(
                &[("label", "String"), ("id", "UInt32")],
                &[&["a", "1"]],
            )],
        );
    }

    fn system_requests(mock: &Mock) -> Vec<Request> {
        mock.requests()
            .into_iter()
            .filter(|request| request.statement.contains("FROM system.columns"))
            .collect()
    }

    #[test]
    fn a_query_tabs_select_of_a_merge_tree_is_described_and_carries_an_edit_target() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        queried(&mock);
        let result = run(&connection, QUERIED).unwrap();
        assert_eq!(result.read_only_reason, None);
        let target = result.edit.expect("a MergeTree table is editable");
        assert_eq!((target.schema.as_str(), target.table.as_str()), ("d", "t"));
        assert_eq!(
            target.columns,
            [Some("label".to_string()), Some("id".to_string())]
        );
        assert_eq!(
            target.row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![1] }
        );
        assert_eq!(target.update, UpdateSpelling::Update);
        let [describe] = system_requests(&mock).try_into().unwrap();
        assert_eq!(describe.param("session_id"), None);
    }

    #[test]
    fn a_query_tabs_edit_is_counted_and_written_without_the_session_by_the_columns_it_read() {
        use crate::result_grid::{NewValue, ResultGrid};
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        queried(&mock);
        let read = run(&connection, QUERIED).unwrap();
        let mut grid =
            ResultGrid::new(read, crate::sql::Mode::ReadWrite).with_engine(Engine::ClickHouse);
        assert!(grid.set_pending(0, 0, NewValue::Value("b".into())));
        let batch = crate::sql::update_batch(Engine::ClickHouse, &grid.pending_updates()).unwrap();
        assert!(crate::sql::is_generated_write_on(
            Engine::ClickHouse,
            &batch
        ));
        let predicate = "WHERE `label` = 'a' AND `id` = '1'";
        assert!(batch.contains(predicate), "{batch}");
        mock.on_statement(
            "SELECT COUNT(*) FROM `d`.`t`",
            [Response::rows(&[("COUNT()", "UInt64")], &[&["1"]])],
        );
        mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
        write(&connection, &batch).unwrap();
        let sent: Vec<Request> = mock
            .requests()
            .into_iter()
            .filter(|request| request.statement.contains(predicate))
            .collect();
        let statements: Vec<&str> = sent
            .iter()
            .map(|request| request.statement.as_str())
            .collect();
        assert_eq!(
            statements,
            [
                format!("SELECT COUNT(*) FROM `d`.`t` {predicate}"),
                format!("UPDATE `d`.`t` SET `label` = 'b' {predicate}"),
            ]
        );
        assert!(
            sent.iter()
                .all(|request| request.param("session_id").is_none())
        );
    }

    /// Where the session reads an unqualified name: `database`, and whether a
    /// temporary table of its own goes by it.
    fn resolves(mock: &Mock, database: &str, temporary: &str) {
        mock.on_statement(
            "currentDatabase() AS database",
            [Response::rows(
                &[("database", "String"), ("temporary", "UInt64")],
                &[&[database, temporary]],
            )],
        );
    }

    #[test]
    fn an_unqualified_table_is_the_one_in_the_database_the_session_was_moved_to() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        mock.on_statement("USE analytics", [Response::empty()]);
        mock.on_statement(
            "SELECT * FROM page_views",
            [Response::rows(
                &[("id", "UInt32"), ("label", "String")],
                &[&["1", "a"]],
            )],
        );
        resolves(&mock, "analytics", "0");
        run(&connection, "USE analytics").unwrap();
        let result = run(&connection, "SELECT * FROM page_views").unwrap();
        let target = result.edit.expect("an editable table");
        assert_eq!(
            (target.schema.as_str(), target.table.as_str()),
            ("analytics", "page_views")
        );
        let [asked] = sent(&mock, "currentDatabase()").try_into().unwrap();
        let [query] = sent(&mock, "SELECT * FROM page_views").try_into().unwrap();
        assert!(asked.param("session_id").is_some());
        assert_eq!(asked.param("session_id"), query.param("session_id"));
        let [describe] = system_requests(&mock).try_into().unwrap();
        assert!(
            describe
                .statement
                .contains("database = 'analytics' AND table = 'page_views'"),
            "{}",
            describe.statement
        );
    }

    #[test]
    fn a_temporary_table_of_the_sessions_has_no_edit_target() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        mock.on_statement(
            "SELECT * FROM t",
            [Response::rows(&[("id", "UInt32")], &[&["1"]])],
        );
        resolves(&mock, "d", "1");
        let result = run(&connection, "SELECT * FROM t").unwrap();
        assert_eq!(result.edit, None);
        assert_eq!(result.read_only_reason.as_deref(), Some(edit::TEMPORARY));
        assert!(system_requests(&mock).is_empty());
    }

    #[test]
    fn after_a_set_on_the_session_its_rows_are_not_matched_but_a_run_at_once_is() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        queried(&mock);
        mock.on_statement("SET session_timezone", [Response::empty()]);
        assert!(run(&connection, QUERIED).unwrap().edit.is_some());
        run(
            &connection,
            "/* zone */ SET session_timezone = 'Asia/Tokyo'",
        )
        .unwrap();
        let result = run(&connection, QUERIED).unwrap();
        assert_eq!(result.edit, None);
        assert_eq!(result.read_only_reason.as_deref(), Some(edit::SESSION_SET));
        // A run at once goes without the session, as the guard does.
        let alongside = connection
            .query_alongside(QUERIED, &CancelToken::alongside(), Fetch::default(), false)
            .unwrap();
        assert!(alongside.edit.is_some());
        // A session of its own has run nothing.
        assert!(run(&connection.apart(), QUERIED).unwrap().edit.is_some());
    }

    #[test]
    fn a_query_whose_rows_are_not_one_tables_says_why_and_describes_nothing() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        mock.on_statement("FINAL", [Response::rows(&[("id", "UInt32")], &[&["1"]])]);
        numbers(&mock);
        let result = run(&connection, "SELECT * FROM d.t FINAL").unwrap();
        assert_eq!(
            result.read_only_reason.as_deref(),
            Some("FINAL merges rows as they are read, so these rows cannot be edited.")
        );
        let result = run(&connection, "SELECT n").unwrap();
        assert_eq!(
            result.read_only_reason.as_deref(),
            Some("These rows are not read from a table, so they cannot be edited.")
        );
        assert_eq!(result.edit, None);
        assert!(system_requests(&mock).is_empty());
        assert!(sent(&mock, "currentDatabase()").is_empty());
    }

    #[test]
    fn a_set_or_an_execute_as_is_what_changes_the_sessions_settings() {
        for statement in [
            "SET session_timezone = 'UTC'",
            "  set date_time_output_format = 'iso'",
            "-- zone\nSET session_timezone = 'UTC'",
            "/* role */ SET ROLE admin",
            "EXECUTE AS other",
            "/* unterminated",
        ] {
            assert!(sets_session(statement), "{statement}");
        }
        for statement in [
            "SELECT 1 SETTINGS session_timezone = 'UTC'",
            "USE analytics",
            "/* SET */ SELECT 1",
            "INSERT INTO t VALUES ('SET')",
            "SETTINGS",
        ] {
            assert!(!sets_session(statement), "{statement}");
        }
    }

    /// `` `d`.`t` `` described as a MergeTree that takes lightweight updates.
    fn editable(mock: &Mock) {
        described(
            mock,
            "MergeTree",
            "MergeTree ORDER BY id SETTINGS enable_block_number_column = 1, \
             enable_block_offset_column = 1",
        );
    }

    fn row(id: &str) -> String {
        format!("WHERE `id` = '{id}' AND `label` = 'a'")
    }

    fn update(id: &str) -> String {
        format!("UPDATE `d`.`t` SET `label` = 'b' {}", row(id))
    }

    fn guard(id: &str) -> String {
        format!("SELECT COUNT(*) FROM `d`.`t` {}", row(id))
    }

    /// The guard of the row `id` names counts `rows`.
    fn counts(mock: &Mock, id: &str, rows: &str) {
        mock.on_statement(
            &guard(id),
            [Response::rows(&[("COUNT()", "UInt64")], &[&[rows]])],
        );
    }

    fn write(connection: &Connection, sql: &str) -> Result<QueryResult, DbError> {
        connection.generated(sql, &CancelToken::default(), Fetch::default())
    }

    #[test]
    fn a_write_is_sent_only_after_its_guard_counts_one_row_with_its_own_predicate() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        counts(&mock, "1", "1");
        mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
        write(&connection, &format!("{};", update("1"))).unwrap();

        let sent: Vec<Request> = mock
            .requests()
            .into_iter()
            .filter(|request| request.statement.contains(&row("1")))
            .collect();
        let statements: Vec<&str> = sent
            .iter()
            .map(|request| request.statement.as_str())
            .collect();
        assert_eq!(statements, [guard("1"), update("1")]);
        assert!(
            sent.iter()
                .all(|request| request.param("session_id").is_none())
        );
    }

    #[test]
    fn a_guard_that_counts_no_row_or_two_refuses_the_write_unsent() {
        for (rows, refusal) in [
            (
                "0",
                "No row has these values any more; it was changed or deleted since it was read.",
            ),
            (
                "2",
                "2 rows have exactly these values, so DBDelve will not change one of them.",
            ),
        ] {
            let mock = Mock::start();
            let connection = open(&mock);
            editable(&mock);
            counts(&mock, "1", rows);
            mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
            let error = write(&connection, &update("1")).unwrap_err();
            assert_eq!(error.message, refusal);
            assert_eq!(mock.hits(&guard("1")), 1);
            assert_eq!(mock.hits("UPDATE `d`.`t`"), 0);
        }
    }

    #[test]
    fn a_batch_stops_at_its_first_refusal_and_says_which_edits_stay() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        counts(&mock, "1", "1");
        counts(&mock, "2", "0");
        counts(&mock, "3", "1");
        mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
        let batch = format!("{};\n{};\n{};", update("1"), update("2"), update("3"));
        let error = write(&connection, &batch).unwrap_err();
        assert_eq!(
            error.message,
            "1 of 3 edits was applied and stays; ClickHouse has no transaction. The second was \
             refused, and the one after it was not run: No row has these values any more; it was \
             changed or deleted since it was read."
        );
        assert_eq!(mock.hits(&update("1")), 1);
        assert_eq!(mock.hits(&update("2")), 0);
        assert_eq!(mock.hits(&guard("3")), 0);
        assert_eq!(mock.hits(&update("3")), 0);
    }

    #[test]
    fn a_write_the_server_refuses_part_way_says_where_and_keeps_its_position() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        counts(&mock, "1", "1");
        counts(&mock, "2", "1");
        mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
        mock.on_statement(
            &update("2"),
            [Response::error(
                500,
                497,
                "ACCESS_DENIED",
                "Not enough privileges",
            )],
        );
        let batch = format!("{};\n{};", update("1"), update("2"));
        let error = write(&connection, &batch).unwrap_err();
        assert_eq!(
            error.message,
            "1 of 2 edits was applied and stays; ClickHouse has no transaction. The second \
             failed: Not enough privileges (ACCESS_DENIED)"
        );

        let error = write(&connection, &format!("{};", update("2"))).unwrap_err();
        assert_eq!(error.message, "Not enough privileges (ACCESS_DENIED)");
    }

    #[test]
    fn a_write_with_no_guard_to_count_it_runs_nothing() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        mock.on_statement("`d`.`t`", [Response::empty()]);
        let before = mock.requests().len();
        for statement in [
            "UPDATE `d`.`t` SET `label` = 'b' WHERE `id` > '1'",
            "UPDATE `d`.`t` SET `label` = 'b' WHERE `id` = '1' OR `id` = '2'",
            "UPDATE `d`.`t` SET `label` = 'b'",
            "DELETE FROM `d`.`t` WHERE `id` = '1' AND lower(`label`) = 'a'",
            "OPTIMIZE TABLE `d`.`t` FINAL",
            "ALTER TABLE `d`.`t` UPDATE `label` = 'b' WHERE `id` = '1'",
            "ALTER TABLE `d`.`t` DELETE WHERE `id` = '1' SETTINGS mutations_sync = 2",
            "TRUNCATE TABLE `d`.`t`",
        ] {
            let error = write(&connection, statement).unwrap_err();
            assert_eq!(error.message, edit::UNCOUNTED, "{statement}");
        }
        assert_eq!(mock.requests().len(), before);
    }

    #[test]
    fn a_write_the_table_does_not_take_runs_nothing() {
        let delete = format!("DELETE FROM `d`.`t` {}", row("1"));
        let alter = format!(
            "ALTER TABLE `d`.`t` UPDATE `label` = 'b' {} SETTINGS mutations_sync = 2",
            row("1")
        );
        let lightweight = update("1");
        let cases = [
            (
                "MergeTree ORDER BY id",
                "CREATE TABLE d.t (id UInt32, PROJECTION p (SELECT id GROUP BY id))",
                delete.as_str(),
                "This table has projections and its lightweight_mutation_projection_mode is \
                 throw, so rows cannot be deleted.",
            ),
            (
                "MergeTree ORDER BY id SETTINGS enable_block_number_column = 1, \
                 enable_block_offset_column = 1",
                "CREATE TABLE d.t (id UInt32)",
                alter.as_str(),
                "This table's settings changed since its rows were read, so the edit was not run.",
            ),
            (
                "MergeTree ORDER BY id",
                "CREATE TABLE d.t (id UInt32)",
                lightweight.as_str(),
                "This table's settings changed since its rows were read, so the edit was not run.",
            ),
        ];
        for (engine_full, create, statement, refusal) in cases {
            let mock = Mock::start();
            let connection = open(&mock);
            described(&mock, "MergeTree", engine_full);
            mock.on_statement(
                "FROM system.tables",
                [Response::rows(
                    &[
                        ("engine", "String"),
                        ("engine_full", "String"),
                        ("create_table_query", "String"),
                    ],
                    &[&["MergeTree", engine_full, create]],
                )],
            );
            mock.on_statement(
                "FROM system.merge_tree_settings",
                [Response::rows(
                    &[("name", "String"), ("value", "String")],
                    &[
                        &["server.enable_lightweight_update", "1"],
                        &["lightweight_mutation_projection_mode", "throw"],
                    ],
                )],
            );
            counts(&mock, "1", "1");
            let error = write(&connection, statement).unwrap_err();
            assert_eq!(error.message, refusal, "{statement}");
            assert_eq!(mock.hits("COUNT(*)"), 0, "{statement}");
        }

        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "Memory", "Memory");
        let error = write(&connection, &update("1")).unwrap_err();
        assert_eq!(
            error.message,
            "ClickHouse edits rows of MergeTree tables only."
        );
        assert_eq!(mock.hits("COUNT(*)"), 0);
    }

    #[test]
    fn a_mutation_and_a_delete_are_guarded_by_their_own_predicates() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        counts(&mock, "1", "1");
        mock.on_statement("ALTER TABLE `d`.`t`", [Response::empty()]);
        mock.on_statement("DELETE FROM `d`.`t`", [Response::empty()]);
        let alter = format!(
            "ALTER TABLE `d`.`t` UPDATE `label` = 'b' {} SETTINGS mutations_sync = 2",
            row("1")
        );
        let delete = format!("DELETE FROM `d`.`t` {}", row("1"));
        write(&connection, &alter).unwrap();
        write(&connection, &delete).unwrap();
        let statements: Vec<String> = mock
            .requests()
            .into_iter()
            .map(|request| request.statement)
            .filter(|statement| statement.contains(&row("1")))
            .collect();
        assert_eq!(statements, [guard("1"), alter, guard("1"), delete]);
    }

    fn alter(id: &str) -> String {
        format!(
            "ALTER TABLE `d`.`t` UPDATE `label` = 'b' {} SETTINGS mutations_sync = 2",
            row(id)
        )
    }

    /// Two mutations of `` `d`.`t` ``, cancelled once `stalls` arrives.
    fn cancelled_at(mock: &Mock, connection: &Connection, stalls: &str) -> DbError {
        let cancel = CancelToken::default();
        let batch = format!("{};\n{};", alter("1"), alter("2"));
        std::thread::scope(|scope| {
            let writing = scope.spawn(|| connection.generated(&batch, &cancel, Fetch::default()));
            until(stalls, || mock.hits(stalls) == 1);
            connection.cancel(&cancel).unwrap();
            writing.join().unwrap().unwrap_err()
        })
    }

    const APPLIED_ONE: &str =
        "1 of 2 edits was applied and stays; ClickHouse has no transaction. The second";

    #[test]
    fn a_cancel_while_a_mutations_guard_counts_says_the_mutation_was_never_sent() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement("ALTER TABLE `d`.`t`", [Response::empty()]);
        counts(&mock, "1", "1");
        mock.on_statement(&guard("2"), [sleeping()]);
        let error = cancelled_at(&mock, &connection, &guard("2"));
        assert_eq!(
            error.message,
            format!(
                "{APPLIED_ONE} failed before it was sent, while DBDelve counted its rows: \
                 Cancelled."
            )
        );
        assert_eq!(mock.hits(&alter("2")), 0);
    }

    #[test]
    fn a_cancel_that_lands_before_the_mutation_goes_out_says_it_was_never_sent() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement("ALTER TABLE `d`.`t`", [Response::empty()]);
        counts(&mock, "1", "1");
        mock.on_statement(
            &guard("2"),
            [Response::rows(&[("COUNT()", "UInt64")], &[&["1"]])
                .delayed(Duration::from_millis(500))],
        );
        let error = cancelled_at(&mock, &connection, &guard("2"));
        assert_eq!(
            error.message,
            format!("{APPLIED_ONE} failed before it was sent: Cancelled.")
        );
        assert_eq!(mock.hits(&alter("2")), 0);
    }

    #[test]
    fn a_cancel_while_the_mutation_runs_says_it_keeps_running() {
        let mock = Mock::start();
        let connection = open(&mock);
        described(&mock, "MergeTree", "MergeTree ORDER BY id");
        mock.on_statement("ALTER TABLE `d`.`t`", [Response::empty()]);
        counts(&mock, "1", "1");
        counts(&mock, "2", "1");
        mock.on_statement(&alter("2"), [sleeping()]);
        let error = cancelled_at(&mock, &connection, &alter("2"));
        assert_eq!(
            error.message,
            format!(
                "{APPLIED_ONE} failed: Cancelled. The mutation keeps running in the background; \
                 KILL MUTATION stops it."
            )
        );
    }

    #[test]
    fn a_describe_that_fails_part_way_says_the_edit_was_never_sent() {
        let mock = Mock::start();
        let connection = open(&mock);
        editable(&mock);
        counts(&mock, "1", "1");
        mock.on_statement("UPDATE `d`.`t`", [Response::empty()]);
        mock.on_statement(
            "AND name = 'u'",
            [Response::error(
                500,
                241,
                "MEMORY_LIMIT_EXCEEDED",
                "Memory limit exceeded",
            )],
        );
        let other = format!("UPDATE `d`.`u` SET `label` = 'b' {}", row("1"));
        let error = write(&connection, &format!("{};\n{other};", update("1"))).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "{APPLIED_ONE} failed before it was sent, while DBDelve read its table: Memory \
                 limit exceeded (MEMORY_LIMIT_EXCEEDED)"
            )
        );
        assert_eq!(error.position, None);
        assert_eq!(mock.hits("`d`.`u`"), 0);
    }

    #[test]
    fn an_insert_and_a_read_go_out_as_one_request_unguarded() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement("INSERT INTO `d`.`t`", [Response::empty()]);
        mock.on_statement(
            "SELECT 1 FROM `d`.`t`",
            [Response::rows(&[("1", "UInt8")], &[&["1"]])],
        );
        let insert = "INSERT INTO `d`.`t` (`id`) VALUES ('1');";
        write(&connection, insert).unwrap();
        write(&connection, "SELECT 1 FROM `d`.`t` WHERE `id` = '1'").unwrap();
        assert_eq!(sent(&mock, "INSERT").len(), 1);
        assert_eq!(sent(&mock, "INSERT")[0].statement, insert);
        assert_eq!(mock.hits("COUNT(*)"), 0);
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
    fn a_capped_first_set_keeps_its_totals_and_extremes() {
        for tagged in [true, false] {
            let mock = Mock::start();
            mock.tagged(tagged);
            let connection = open(&mock);
            mock.on_statement(
                "WITH TOTALS",
                [Response::rows(
                    &[("k", "String"), ("n", "UInt64"), ("m", "UInt64")],
                    &[
                        &["a", "1", "5"],
                        &["b", "2", "6"],
                        &["c", "3", "7"],
                        &["d", "4", "8"],
                        &[""],
                        &["", "10", "26"],
                        &[""],
                        &["", "1", "5"],
                        &["", "4", "8"],
                    ],
                )],
            );
            let result = connection
                .query(
                    "SELECT k, n, m WITH TOTALS",
                    &CancelToken::default(),
                    Fetch {
                        limit: Some(2),
                        ..Fetch::default()
                    },
                )
                .unwrap();
            assert_eq!(result.rows.len(), 2, "tagged: {tagged}");
            assert_eq!(result.capped_from, Some(4), "tagged: {tagged}");
            let [totals, extremes] = result.rest.try_into().unwrap();
            assert_eq!(totals.rows.len(), 1, "tagged: {tagged}");
            assert_eq!(totals.rows[0].get(1), Some("10"));
            assert_eq!(extremes.rows.len(), 2, "tagged: {tagged}");
            assert_eq!(extremes.capped_from, None);
            assert_eq!(extremes.rows[1].get(2), Some("8"));
        }
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

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "waited too long for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn handles(cancel: &CancelToken) -> Vec<String> {
        cancel.0.lock().unwrap().handles.clone()
    }

    fn kills(mock: &Mock) -> Vec<Request> {
        sent(mock, "KILL QUERY")
    }

    /// A response that holds its headers back until a `KILL` for it lands,
    /// then ends as a killed query's does.
    fn sleeping() -> Response {
        Response::body("")
            .delayed(Duration::from_secs(30))
            .killed_by(Aftermath::Cancelled)
    }

    fn three_rows() -> Response {
        Response::rows(&[("n", "UInt8")], &[&["1"], &["2"], &["3"]])
    }

    #[test]
    fn a_cancel_before_the_send_drops_the_statement_unsent() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        let cancel = CancelToken::default();
        connection.cancel(&cancel).unwrap();
        let error = connection
            .query("SELECT n", &cancel, Fetch::default())
            .unwrap_err();
        assert_eq!(error.message, CANCELLED);
        assert!(sent(&mock, "SELECT n").is_empty());
        assert!(kills(&mock).is_empty());
    }

    #[test]
    fn a_kill_sent_before_the_server_registered_the_query_is_sent_again_until_it_lands() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.registration(Registration::OnKill(2));
        mock.on_statement("SELECT sleep", [sleeping()]);
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running =
                scope.spawn(|| connection.query("SELECT sleep(30)", &cancel, Fetch::default()));
            // A Cancel that comes before the send drops the statement unsent,
            // so it waits for the POST; what it races is the registration.
            until("the POST", || mock.hits("SELECT sleep") == 1);
            let id = handles(&cancel)[0].clone();
            connection.cancel(&cancel).unwrap();
            let error = running.join().unwrap().unwrap_err();
            assert_eq!(error.message, CANCELLED);
            assert!(is_cancel(Engine::ClickHouse, &error));
            assert_eq!(mock.hits("SELECT sleep"), 1);
            assert_eq!(mock.killed(), [id]);
        });
        assert!(kills(&mock).len() >= 2);
        for kill in kills(&mock) {
            assert_eq!(kill.param("session_id"), None);
            assert!(kill.statement.ends_with("' SYNC"), "{}", kill.statement);
        }
    }

    #[test]
    fn a_cancel_while_the_session_enters_its_database_never_sends_the_statement() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.registration(Registration::Signal);
        mock.on_statement("USE `", [sleeping()]);
        mock.on_statement("INSERT", [Response::empty()]);
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running = scope
                .spawn(|| connection.query("INSERT INTO t VALUES (1)", &cancel, Fetch::default()));
            until("the USE", || mock.hits("USE `") == 1);
            let started = Instant::now();
            assert_eq!(connection.cancel(&cancel).unwrap_err().message, UNREACHED);
            assert_eq!(running.join().unwrap().unwrap_err().message, CANCELLED);
            assert!(started.elapsed() < KILL_WITHIN + Duration::from_secs(1));
        });
        assert_eq!(mock.hits("INSERT"), 0);
        let entering = sent(&mock, "USE `")[0].query_id().unwrap().to_string();
        assert!(kills(&mock).len() > 2);
        for kill in kills(&mock) {
            assert!(kill.statement.contains(&entering), "{}", kill.statement);
        }
        assert!(!lock(&connection.session).opened);
    }

    #[test]
    fn a_query_the_server_never_registers_is_let_go_after_the_deadline() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.registration(Registration::Signal);
        mock.on_statement("SELECT sleep", [sleeping()]);
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running =
                scope.spawn(|| connection.query("SELECT sleep(30)", &cancel, Fetch::default()));
            until("the POST", || mock.hits("SELECT sleep") == 1);
            let started = Instant::now();
            let error = connection.cancel(&cancel).unwrap_err();
            assert_eq!(error.message, UNREACHED);
            let run = running.join().unwrap().unwrap_err();
            let took = started.elapsed();
            assert_eq!(run.message, CANCELLED);
            assert!(took >= KILL_WITHIN - KILL_RETRY * 2, "{took:?}");
            assert!(took < KILL_WITHIN + Duration::from_secs(1), "{took:?}");
        });
        assert!(kills(&mock).len() > 2);
    }

    #[test]
    fn a_query_the_server_cannot_cancel_is_let_go_at_once() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.kill_status("cant_cancel");
        mock.on_statement("SELECT sleep", [sleeping()]);
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running =
                scope.spawn(|| connection.query("SELECT sleep(30)", &cancel, Fetch::default()));
            until("the request", || mock.hits("SELECT sleep") == 1);
            let started = Instant::now();
            assert_eq!(connection.cancel(&cancel).unwrap_err().message, UNREACHED);
            assert_eq!(running.join().unwrap().unwrap_err().message, CANCELLED);
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        assert_eq!(kills(&mock).len(), 1);
    }

    #[test]
    fn an_upload_the_server_stopped_reading_is_ended_by_cancel_and_frees_the_session() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        mock.swallow_bodies_over(1024 * 1024);
        let insert = format!("INSERT INTO t VALUES {}", "(1),".repeat(16 * 1024 * 1024));
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| connection.query(&insert, &cancel, Fetch::default()));
            until("the server to stop reading", || mock.swallowed() == 1);
            let started = Instant::now();
            assert_eq!(connection.cancel(&cancel).unwrap_err().message, UNREACHED);
            assert_eq!(running.join().unwrap().unwrap_err().message, CANCELLED);
            assert!(started.elapsed() < KILL_WITHIN + Duration::from_secs(1));
        });
        assert!(kills(&mock).len() > 2);
        run(&connection, "SELECT n").unwrap();
    }

    #[test]
    fn a_kill_whose_connect_stalls_gives_up_within_its_deadline() {
        // TEST-NET-1 (RFC 5737) is never routed: behind a default route its
        // SYN goes unanswered, and without one the connect fails at once.
        let server = ServerConfig {
            host: "192.0.2.1".into(),
            port: Some(DEFAULT_PORT),
            sslmode: SslMode::Disable,
            ..ServerConfig::default()
        };
        let agent = http::agent(&server, None, Scheme::Http, Bounds::internal()).unwrap();
        let connection = Connection::unchecked(&server, DEFAULT_PORT, agent, Scheme::Http);
        let started = Instant::now();
        assert!(
            connection
                .kill("dbdelve-0", Duration::from_millis(500))
                .is_err()
        );
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1500), "{took:?}");
    }

    #[test]
    fn a_connection_lost_after_the_request_went_out_is_killed() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement("SELECT n", [Response::dropped()]);
        let error = connection
            .generated("SELECT n", &CancelToken::default(), Fetch::default())
            .unwrap_err();
        let endpoint = format!("127.0.0.1:{} did not answer: ", mock.server().port.unwrap());
        assert!(error.message.starts_with(&endpoint), "{error}");
        let id = sent(&mock, "SELECT n")[0].query_id().unwrap().to_string();
        assert_eq!(mock.killed(), [id]);

        // Never found: it may have been lost on the way, or be on its way.
        mock.registration(Registration::Signal);
        let error = connection
            .generated("SELECT n", &CancelToken::default(), Fetch::default())
            .unwrap_err();
        assert!(
            error.message.ends_with(
                "It may still be running: stopping it failed too (the server did not \
                 confirm that it stopped)."
            ),
            "{error}"
        );
    }

    #[test]
    fn a_body_cut_short_is_an_error_and_its_query_is_killed() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement("SELECT n", [three_rows().truncated(11)]);
        let error = run(&connection, "SELECT n").unwrap_err();
        assert!(is_cut(&error), "{error}");
        let id = sent(&mock, "SELECT n")[0].query_id().unwrap().to_string();
        assert_eq!(mock.killed(), [id]);
    }

    #[test]
    fn a_body_cut_after_a_cancel_is_the_cancel() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [three_rows()
                .stalled(10, Duration::from_secs(30))
                .killed_by(Aftermath::Cut)],
        );
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| connection.query("SELECT n", &cancel, Fetch::default()));
            until("the request", || mock.hits("SELECT n") == 1);
            connection.cancel(&cancel).unwrap();
            assert_eq!(running.join().unwrap().unwrap_err().message, CANCELLED);
        });
        assert_eq!(kills(&mock).len(), 1);
    }

    #[test]
    fn a_stalled_body_without_a_bound_is_ended_by_cancel() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [three_rows()
                .stalled(10, Duration::from_secs(30))
                .killed_by(Aftermath::Cancelled)],
        );
        let cancel = CancelToken::default();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| connection.query("SELECT n", &cancel, Fetch::default()));
            until("the request", || mock.hits("SELECT n") == 1);
            let started = Instant::now();
            connection.cancel(&cancel).unwrap();
            let error = running.join().unwrap().unwrap_err();
            assert_eq!(error.message, CANCELLED);
            assert!(started.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn a_server_that_goes_quiet_past_the_bound_is_stopped_and_killed() {
        let mock = Mock::start();
        let server = ServerConfig {
            statement_timeout: 1,
            ..profile(&mock)
        };
        let connection = open_with(&mock, writer(), &server).unwrap();
        mock.on_statement(
            "SELECT n",
            [three_rows().stalled(10, Duration::from_secs(30))],
        );
        // Not even the headers.
        mock.on_statement(
            "SELECT m",
            [Response::body("").delayed(Duration::from_secs(30))],
        );
        for statement in ["SELECT n", "SELECT m"] {
            let started = Instant::now();
            // Sessionless: what is tested is the bound, not how soon the
            // server lets go of a session after the `KILL`.
            let error = connection
                .generated(statement, &CancelToken::default(), Fetch::default())
                .unwrap_err();
            assert_eq!(
                error.message, "The server sent nothing for 1 seconds; the statement was stopped.",
                "{statement}"
            );
            assert!(started.elapsed() < Duration::from_secs(4), "{statement}");
        }
        let ids: Vec<String> = ["SELECT n", "SELECT m"]
            .iter()
            .map(|statement| sent(&mock, statement)[0].query_id().unwrap().to_string())
            .collect();
        assert_eq!(mock.killed(), ids);
    }

    #[test]
    fn a_timeout_before_the_server_registered_the_query_kills_it_once_it_is_or_says_so() {
        let mock = Mock::start();
        let server = ServerConfig {
            statement_timeout: 1,
            ..profile(&mock)
        };
        let connection = open_with(&mock, writer(), &server).unwrap();
        mock.on_statement(
            "SELECT m",
            [Response::body("").delayed(Duration::from_secs(30))],
        );
        // Past the bound (1.5 s here), and within the KILLs' deadline after.
        mock.registration(Registration::After(Duration::from_millis(2500)));
        let error = connection
            .generated("SELECT m", &CancelToken::default(), Fetch::default())
            .unwrap_err();
        assert_eq!(
            error.message,
            "The server sent nothing for 1 seconds; the statement was stopped."
        );
        let id = sent(&mock, "SELECT m")[0].query_id().unwrap().to_string();
        assert_eq!(mock.killed(), [id]);
        assert!(kills(&mock).len() > 1);

        mock.registration(Registration::Signal);
        let error = connection
            .generated("SELECT m", &CancelToken::default(), Fetch::default())
            .unwrap_err();
        assert_eq!(
            error.message,
            "The server sent nothing for 1 seconds, and stopping the statement failed (the \
             server did not confirm that it stopped); it may still be running."
        );
    }

    #[test]
    fn a_read_stopped_at_the_limit_whose_kill_keeps_waiting_may_still_be_running() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.kill_status("waiting");
        mock.on_statement(
            "SELECT n",
            [Response::rows(&[("n", "UInt8")], &[&["1"], &["2"], &["3"]])],
        );
        let result = connection
            .query(
                "SELECT n",
                &CancelToken::default(),
                Fetch {
                    limit: Some(2),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        let notice = result.notice.unwrap();
        assert!(notice.contains("did not confirm"), "{notice}");
        assert!(
            notice.ends_with("it may still be running on the server."),
            "{notice}"
        );
        assert!(kills(&mock).len() > 2);
    }

    #[test]
    fn a_late_error_past_the_cap_fails_the_drain_tagged_or_not() {
        for tagged in [true, false] {
            let mock = Mock::start();
            mock.tagged(tagged);
            let connection = open(&mock);
            mock.on_statement(
                "SELECT n",
                [Response::rows(
                    &[("n", "UInt8"), ("m", "UInt8")],
                    &[&["1", "1"], &["2", "2"], &["3", "3"]],
                )
                .then_fails(
                    395,
                    "FUNCTION_THROW_IF_VALUE_IS_NON_ZERO",
                    "Value passed to 'throwIf' function is non-zero",
                )],
            );
            let error = connection
                .query(
                    "SELECT n, m",
                    &CancelToken::default(),
                    Fetch {
                        limit: Some(1),
                        ..Fetch::default()
                    },
                )
                .unwrap_err();
            assert_eq!(
                error.message,
                "Value passed to 'throwIf' function is non-zero \
                 (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)",
                "tagged: {tagged}"
            );
        }
    }

    #[test]
    fn a_read_stopped_at_the_limit_is_killed_once_and_its_own_cancel_ignored() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [
                Response::rows(&[("n", "UInt8")], &[&["1"], &["2"], &["3"], &["4"]]).then_fails(
                    394,
                    "QUERY_WAS_CANCELLED",
                    "Query was cancelled.",
                ),
            ],
        );
        let capped = Fetch {
            limit: Some(2),
            reads_only: true,
            ..Fetch::default()
        };
        let result = connection
            .query("SELECT n", &CancelToken::default(), capped)
            .unwrap();
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.capped_from, None);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.notice, None);
        let [kill] = kills(&mock).try_into().unwrap();
        assert_eq!(kill.param("session_id"), None);
        let id = sent(&mock, "SELECT n")[0].query_id().unwrap().to_string();
        assert_eq!(mock.killed(), [id]);

        mock.kill_status("cant_cancel");
        let result = connection
            .query("SELECT n", &CancelToken::default(), capped)
            .unwrap();
        assert_eq!(result.rows.len(), 2);
        let notice = result.notice.unwrap();
        assert!(
            notice.starts_with("Stopping the statement at the row limit failed"),
            "{notice}"
        );
    }

    #[test]
    fn an_at_once_runs_statements_are_all_killed_and_the_session_left_alone() {
        let mock = Mock::start();
        let connection = open(&mock);
        numbers(&mock);
        run(&connection, "SELECT n").unwrap();
        mock.on_statement("SELECT sleep", [sleeping()]);
        let cancel = CancelToken::alongside();
        std::thread::scope(|scope| {
            let runs: Vec<_> = ["SELECT sleep(1)", "SELECT sleep(2)"]
                .into_iter()
                .map(|sql| {
                    let (connection, cancel) = (&connection, &cancel);
                    scope.spawn(move || {
                        connection.query_alongside(sql, cancel, Fetch::default(), false)
                    })
                })
                .collect();
            until("both requests", || {
                handles(&cancel).len() == 2 && mock.hits("SELECT sleep") == 2
            });
            let mut ids = handles(&cancel);
            connection.cancel(&cancel).unwrap();
            for run in runs {
                assert_eq!(run.join().unwrap().unwrap_err().message, CANCELLED);
            }
            let mut killed = mock.killed();
            killed.sort();
            ids.sort();
            assert_eq!(killed, ids);
        });
        for kill in kills(&mock) {
            assert_eq!(kill.param("session_id"), None);
        }
        assert!(connection.running.lock().unwrap().is_empty());
        run(&connection, "SELECT n").unwrap();
        let [before, after] = sent(&mock, "SELECT n").try_into().unwrap();
        assert_eq!(after.param("session_id"), before.param("session_id"));
        assert_eq!(after.param("session_check"), Some("1"));
    }

    #[test]
    fn both_user_paths_hint_a_refused_typed_update_and_generated_ones_do_not() {
        let mock = Mock::start();
        let connection = open(&mock);
        let refusal = "Lightweight updates are not supported. Lightweight updates are supported \
                       only for tables with materialized _block_number column. Run 'MODIFY \
                       SETTING enable_block_number_column = 1' command to enable it.";
        mock.on_statement(
            "UPDATE t",
            [
                Response::error(500, 48, "NOT_IMPLEMENTED", refusal),
                Response::error(500, 48, "NOT_IMPLEMENTED", refusal),
                Response::error(500, 48, "NOT_IMPLEMENTED", refusal),
            ],
        );
        let sql = "UPDATE t SET v = 1 WHERE id = 1";
        let cancel = CancelToken::default();
        let alongside = connection
            .query_alongside(sql, &CancelToken::alongside(), Fetch::default(), false)
            .unwrap_err();
        let query = connection
            .query(sql, &cancel, Fetch::default())
            .unwrap_err();
        let generated = connection
            .generated(sql, &cancel, Fetch::default())
            .unwrap_err();
        for hinted in [alongside, query] {
            assert!(hinted.message.ends_with(edit::LIGHTWEIGHT_UPDATE_HINT));
        }
        assert!(!generated.message.contains(edit::LIGHTWEIGHT_UPDATE_HINT));
    }

    #[test]
    fn a_cancel_nobody_here_asked_for_keeps_the_servers_words() {
        let mock = Mock::start();
        let connection = open(&mock);
        mock.on_statement(
            "SELECT n",
            [Response::error(
                500,
                394,
                "QUERY_WAS_CANCELLED",
                "Query was cancelled.",
            )],
        );
        let error = run(&connection, "SELECT n").unwrap_err();
        assert_eq!(error.message, "Query was cancelled. (QUERY_WAS_CANCELLED)");
        assert!(!is_cancel(Engine::ClickHouse, &error));
    }
}

/// Against the dev container (`docker compose up -d --build clickhouse`), read
/// from `dbdelve_CLICKHOUSE_URL`.
#[cfg(test)]
mod live {
    use super::*;
    use crate::db::{Column, ConnectionConfig, Engine, ExplainMode, is_cancel};
    use crate::explorer::{drop_sql, select_top_sql, truncate_sql};
    use crate::export::{RowsAs, render_rows_as};
    use crate::filter::{Operator, filter_predicate};
    use crate::sql::{Mode, Stop, classify, gate};

    fn live_config() -> ServerConfig {
        let url =
            std::env::var("dbdelve_CLICKHOUSE_URL").expect("dbdelve_CLICKHOUSE_URL is required");
        crate::db::server_from_url(&url, "ClickHouse").expect("dbdelve_CLICKHOUSE_URL should parse")
    }

    fn live() -> Connection {
        Connection::open(&live_config()).expect("connection should open")
    }

    fn live_any(config: ServerConfig) -> crate::db::Connection {
        crate::db::Connection::open(ConnectionConfig::ClickHouse(config))
            .expect("connection should open")
    }

    fn run(connection: &Connection, sql: &str) -> Result<QueryResult, DbError> {
        connection.query(sql, &CancelToken::default(), Fetch::default())
    }

    fn ran(connection: &Connection, sql: &str) -> QueryResult {
        run(connection, sql).unwrap_or_else(|error| panic!("{sql}: {}", error.message))
    }

    fn cells(result: &QueryResult) -> Vec<Vec<Option<String>>> {
        result
            .rows
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|at| row.get(at).map(str::to_string))
                    .collect()
            })
            .collect()
    }

    fn text(result: &QueryResult) -> String {
        cells(result)
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What a request with no driver in the way gets back: the status and body.
    fn raw(sql: &str, query: &str) -> (u16, String) {
        let config = live_config();
        let url = format!(
            "http://{}:{}/?{query}",
            config.host,
            config.port.unwrap_or(DEFAULT_PORT)
        );
        let mut response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("X-ClickHouse-User", config.user.as_str())
            .header("X-ClickHouse-Key", config.password.as_str())
            .send(sql.as_bytes())
            .expect("the server should answer");
        let status = response.status().as_u16();
        (
            status,
            response.body_mut().read_to_string().unwrap_or_default(),
        )
    }

    /// A marker no other statement carries, to find one in `system.processes`.
    fn marker() -> String {
        random_id().unwrap().replace("dbdelve-", "marker")
    }

    fn processes_with(connection: &Connection, marker: &str) -> usize {
        let counted = ran(
            &connection.apart(),
            &format!(
                "SELECT count() FROM system.processes WHERE query LIKE '%{marker}%' \
                 AND query_id != queryID()"
            ),
        );
        text(&counted).parse().unwrap()
    }

    fn column_types(result: &QueryResult) -> Vec<Option<&str>> {
        result
            .columns
            .iter()
            .map(|c: &Column| c.data_type.as_deref())
            .collect()
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_connects_and_prefer_falls_back_to_plaintext() {
        assert_eq!(text(&ran(&live(), "SELECT 1 AS one")), "1");
        let prefer = ServerConfig {
            sslmode: SslMode::Prefer,
            ..live_config()
        };
        let connection = Connection::open(&prefer).expect("prefer should fall back");
        assert_eq!(text(&ran(&connection, "SELECT 2")), "2");
        let require = ServerConfig {
            sslmode: SslMode::Require,
            ..live_config()
        };
        let error = Connection::open(&require)
            .err()
            .expect("require should not fall back");
        eprintln!("require against a plaintext listener: {}", error.message);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_type_gallery_reads_as_it_was_recorded() {
        let live = ran(&live(), "SELECT * FROM type_gallery ORDER BY id");
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/dev/clickhouse/fixtures");
        let body = std::fs::read(format!("{dir}/type_gallery.tsv")).unwrap();
        let mut reader = Reader::open(std::io::Cursor::new(body), None).unwrap();
        let recorded = collect(&mut reader, Fetch::default(), Instant::now()).unwrap();
        assert_eq!(column_types(&live), column_types(&recorded));
        assert_eq!(live.columns, recorded.columns);
        assert_eq!(live.rows, recorded.rows);
        assert_eq!(live.rows.len(), 2);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_pages_of_an_unsorted_preview_share_no_row() {
        let connection = live();
        let key = connection
            .structure("dbdelve_dev", "events")
            .unwrap()
            .row_key();
        assert!(!key.is_empty());
        let mut seen = std::collections::HashSet::new();
        for page in 0..5 {
            let preview = format!(
                "SELECT * FROM `dbdelve_dev`.`events` LIMIT 1000 OFFSET {}",
                page * 1000
            );
            let paged = crate::sql::paged(Engine::ClickHouse, &preview, &key).unwrap();
            assert!(paged.contains("ORDER BY"), "{paged}");
            let result = ran(&connection, &paged);
            assert_eq!(result.rows.len(), 1000, "page {page}");
            for row in cells(&result) {
                assert!(
                    seen.insert(row[0].clone()),
                    "page {page} repeats {:?}",
                    row[0]
                );
            }
        }
        assert_eq!(seen.len(), 5000);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_explain_plans_a_copy_without_running_it() {
        let connection = live();
        let statement = "SELECT * FROM system.numbers LIMIT 3";
        let explained = Engine::ClickHouse
            .explain(ExplainMode::Plan, statement)
            .unwrap();
        assert_eq!(explained, format!("EXPLAIN {statement}"));
        assert_eq!(
            classify(Engine::ClickHouse, &explained).mode,
            Mode::ReadOnly
        );
        let plan = text(&ran(&connection, &explained));
        assert!(plan.contains("ReadFromSystemNumbers"), "{plan}");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_trailing_semicolon_runs() {
        let connection = live();
        for sql in ["SELECT 1;", "SELECT 1 ;\n", "SELECT 1; -- done\n"] {
            assert_eq!(text(&ran(&connection, sql)), "1", "{sql:?}");
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_cancel_leaves_nothing_in_system_processes() {
        let connection = live();
        let marker = marker();
        let sql = format!(
            "SELECT sleepEachRow(1) FROM numbers(60) SETTINGS max_block_size = 1 /* {marker} */"
        );
        let token = CancelToken::default();
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            let running = scope.spawn(|| connection.query(&sql, &token, Fetch::default()));
            while processes_with(&connection, &marker) == 0 {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "it never started"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            connection.cancel(&token).expect("the cancel should land");
            running
                .join()
                .unwrap()
                .expect_err("the run should have been cancelled")
        });
        assert!(is_cancel(Engine::ClickHouse, &error), "{}", error.message);
        assert_eq!(processes_with(&connection, &marker), 0);
        assert_eq!(text(&ran(&connection, "SELECT 1")), "1");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_session_is_usable_at_once_after_a_cancel() {
        let connection = live();
        for _ in 0..8 {
            let sql = "SELECT sleepEachRow(0.2) FROM numbers(100) SETTINGS max_block_size = 1";
            let token = CancelToken::default();
            let error = std::thread::scope(|scope| {
                let running = scope.spawn(|| connection.query(sql, &token, Fetch::default()));
                std::thread::sleep(Duration::from_millis(500));
                connection.cancel(&token).expect("the cancel should land");
                running.join().unwrap().expect_err("cancelled")
            });
            assert!(is_cancel(Engine::ClickHouse, &error), "{}", error.message);
            let next = run(&connection, "SELECT 1");
            assert!(
                next.is_ok(),
                "SESSION_IS_LOCKED after a KILL: {:?}",
                next.err()
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_stop_at_the_limit_leaves_nothing_in_system_processes() {
        let connection = live();
        let marker = marker();
        let sql = format!("SELECT number FROM system.numbers /* {marker} */");
        let started = Instant::now();
        let result = connection
            .query(
                &sql,
                &CancelToken::default(),
                Fetch {
                    limit: Some(3),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.notice, None);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(processes_with(&connection, &marker), 0);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_read_that_may_not_stop_is_drained_and_counted() {
        let result = live()
            .query(
                "SELECT number FROM numbers(1000)",
                &CancelToken::default(),
                Fetch {
                    limit: Some(10),
                    reads_only: false,
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(result.stopped, None);
        assert_eq!(result.rows.len(), 10);
        assert_eq!(result.capped_from, Some(1000));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_throwif_after_the_cap_fails_the_run() {
        let error = live()
            .query(
                "SELECT number, throwIf(number = 5000) FROM numbers(10000) SETTINGS max_block_size = 100",
                &CancelToken::default(),
                Fetch {
                    limit: Some(10),
                    reads_only: false,
                    ..Fetch::default()
                },
            )
            .expect_err("a late exception must not be counted as rows");
        assert!(error.message.contains("throwIf"), "{}", error.message);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_timeout_stops_sleepeachrow_at_n() {
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 2,
            ..live_config()
        })
        .expect("a writer may be given a timeout");
        let started = Instant::now();
        let error = run(
            &connection,
            "SELECT sleepEachRow(0.5) FROM numbers(60) SETTINGS max_block_size = 1",
        )
        .expect_err("the server should stop it");
        assert!(
            error.message.contains("TIMEOUT_EXCEEDED"),
            "{}",
            error.message
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(text(&ran(&connection, "SELECT 1")), "1");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_read_only_hold_refuses_a_write_and_any_lifting_of_itself() {
        let connection = live();
        connection.set_read_only(true).unwrap();
        let refused = |sql: &str| run(&connection, sql).expect_err(sql).message;
        assert!(refused("INSERT INTO scratch VALUES (9, 'x')").contains("readonly mode"));
        assert!(refused("SET readonly = 0").contains("Cannot modify 'readonly' setting"));
        assert!(
            refused("SELECT 1 SETTINGS readonly = 0").contains("Cannot modify 'readonly' setting")
        );
        assert!(
            refused("CREATE TABLE refused_by_readonly (x UInt8) ENGINE = Memory")
                .contains("readonly mode")
        );
        assert_eq!(text(&ran(&connection, "SET max_threads = 2")), "");
        assert_eq!(text(&ran(&connection, "SELECT 1")), "1");
        connection.set_read_only(false).unwrap();
        ran(&connection, "INSERT INTO scratch VALUES (9, 'x')");
        assert_eq!(
            text(&ran(&connection, "SELECT note FROM scratch WHERE id = 9")),
            "x"
        );
        ran(&connection, "TRUNCATE TABLE scratch");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_backup_and_insert_into_function_file_are_refused_in_read_only_before_sending() {
        for sql in [
            "BACKUP TABLE conformance TO Disk('backups', 'x.zip')",
            "INSERT INTO FUNCTION file('x.csv', 'CSV') SELECT 1",
        ] {
            let verdict = classify(Engine::ClickHouse, sql);
            assert_eq!(
                gate(&verdict, Mode::ReadOnly, &[]),
                Some(Stop::Upgrade(Mode::Full)),
                "{sql}"
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_reader_connects_and_runs_and_a_timeout_fails_its_connect() {
        let reader = ServerConfig {
            user: "dbdelve_reader".into(),
            ..live_config()
        };
        let connection = Connection::open(&reader).expect("a readonly user connects");
        assert_eq!(
            text(&ran(&connection, "SELECT count() FROM conformance")),
            "3"
        );
        assert!(run(&connection, "INSERT INTO scratch VALUES (9, 'x')").is_err());
        let refused = run(&connection, "SET max_threads = 2").unwrap_err();
        assert!(
            refused
                .message
                .contains("Cannot modify 'max_threads' setting in readonly mode"),
            "{}",
            refused.message
        );
        connection.set_read_only(true).unwrap();
        assert_eq!(
            text(&ran(&connection, "SELECT count() FROM conformance")),
            "3"
        );

        let error = Connection::open(&ServerConfig {
            statement_timeout: 5,
            ..reader
        })
        .err()
        .expect("a readonly user cannot be given a timeout");
        assert!(
            error.message.contains("max_execution_time") && error.message.contains("readonly"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_use_carries_and_a_per_request_database_would_override_it() {
        let connection = live();
        for _ in 0..3 {
            assert_eq!(
                text(&ran(&connection, "SELECT count() FROM conformance")),
                "3"
            );
        }
        assert_eq!(
            text(&ran(&connection, "SELECT currentDatabase()")),
            "dbdelve_dev"
        );
        ran(&connection, "USE analytics");
        assert_eq!(
            text(&ran(&connection, "SELECT currentDatabase()")),
            "analytics"
        );
        assert_eq!(
            text(&ran(&connection, "SELECT count() FROM page_views")),
            "100"
        );
        assert_eq!(
            text(&ran(&connection.apart(), "SELECT currentDatabase()")),
            "dbdelve_dev"
        );

        let id = lock(&connection.session).id.clone().unwrap();
        let (status, body) = raw(
            "SELECT currentDatabase()",
            &format!("session_id={id}&session_check=1&database=dbdelve_dev"),
        );
        assert_eq!((status, body.trim()), (200, "dbdelve_dev"));
        let (status, body) = raw(
            "SELECT currentDatabase()",
            &format!("session_id={id}&session_check=1"),
        );
        assert_eq!((status, body.trim()), (200, "analytics"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_an_expired_session_fails_one_run_and_the_next_opens_a_new_one() {
        let connection = live();
        ran(&connection, "SET max_threads = 3");
        let id = format!("dbdelve-expiry-{}", marker());
        let (status, _) = raw("SELECT 1", &format!("session_id={id}&session_timeout=1"));
        assert_eq!(status, 200);
        *lock(&connection.session) = Session {
            id: Some(id),
            opened: true,
            default_timeout: false,
            set: false,
        };
        std::thread::sleep(Duration::from_secs(4));
        let error = run(&connection, "SELECT 1").expect_err("the session should be gone");
        assert_eq!(error.message, SESSION_ENDED);
        assert_eq!(text(&ran(&connection, "SELECT 1")), "1");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_into_outfile_over_http_is_refused_by_the_server() {
        let error = run(&live(), "SELECT 1 INTO OUTFILE 'dbdelve-x.out'").unwrap_err();
        eprintln!("INTO OUTFILE over HTTP: {}", error.message);
        assert!(
            error.message.contains("INTO_OUTFILE_NOT_ALLOWED"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_user_set_null_spelling_does_not_change_null() {
        let connection = live();
        ran(&connection, "SET format_tsv_null_representation = 'NULL'");
        let result = ran(
            &connection,
            "SELECT CAST(NULL AS Nullable(String)) AS n, 'NULL' AS s, '\\\\N' AS escaped",
        );
        assert_eq!(
            cells(&result),
            [[None, Some("NULL".into()), Some("\\N".into())]]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_statement_with_a_format_of_its_own_is_one_text_column() {
        let result = ran(&live(), "SELECT 1 AS a, 'x' AS b FORMAT JSON");
        assert_eq!(result.columns.len(), 1);
        let shown = text(&result);
        eprintln!("FORMAT JSON as shown:\n{shown}");
        assert!(
            shown.contains("\"a\": 1") && shown.contains("\"data\""),
            "{shown}"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_column_named_with_a_backslash_comes_back_with_one() {
        let result = ran(&live(), "SELECT 1 AS `a\\\\b`, 2 AS `c\\td`");
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a\\b", "c\td"]);
    }

    fn matched(
        connection: &Connection,
        from: &str,
        column: &str,
        operator: Operator,
        value: &str,
    ) -> String {
        let predicate = filter_predicate(Engine::ClickHouse, column, None, operator, value)
            .unwrap_or_else(|| panic!("{operator:?} {value:?} wrote no predicate"));
        assert!(
            crate::sql::is_generated_select(
                Engine::ClickHouse,
                &format!("SELECT * FROM {from} WHERE {predicate}")
            ),
            "{predicate}"
        );
        let result = ran(
            connection,
            &format!("SELECT count() FROM {from} WHERE {predicate}"),
        );
        text(&result)
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_every_operator_filters_each_type_the_gallery_holds() {
        let connection = live();
        let gallery = "dbdelve_dev.type_gallery";
        // Row 1 is `typical`, row 2 `edges`; each case says how many of the
        // two the operator keeps.
        let cases: [(&str, Operator, &str, &str); 44] = [
            ("u64", Operator::Equals, "18446744073709551615", "1"),
            ("u64", Operator::Greater, "5", "1"),
            ("u64", Operator::Contains, "4467", "1"),
            ("u64", Operator::NotContains, "9", "1"),
            ("u64", Operator::StartsWith, "1844", "1"),
            ("u64", Operator::EndsWith, "615", "1"),
            ("u64", Operator::Regex, "^1\\d", "1"),
            ("maybe", Operator::Contains, "res", "1"),
            ("maybe", Operator::NotContains, "res", "0"),
            // A NULL stays NULL through `toString`, so no negation keeps it.
            ("maybe", Operator::NotContains, "zzz", "1"),
            ("maybe", Operator::StartsWith, "pre", "1"),
            ("maybe", Operator::EndsWith, "ent", "1"),
            ("maybe", Operator::Regex, "^pre", "1"),
            ("maybe", Operator::IsNull, "", "1"),
            ("maybe", Operator::IsEmpty, "", "0"),
            ("low", Operator::Contains, "ow", "1"),
            ("low", Operator::NotContains, "ow", "1"),
            ("low", Operator::StartsWith, "lo", "1"),
            ("low", Operator::EndsWith, "ow", "1"),
            ("low", Operator::Regex, "^l", "1"),
            ("low", Operator::IsEmpty, "", "1"),
            ("day", Operator::Equals, "2024-06-15", "1"),
            ("day", Operator::Less, "2000-01-01", "1"),
            ("day", Operator::Contains, "2024-06", "1"),
            ("day", Operator::StartsWith, "1970", "1"),
            ("day", Operator::EndsWith, "-15", "1"),
            ("day", Operator::Regex, "^2024-\\d\\d-15$", "1"),
            ("moment64", Operator::Greater, "2000-01-01 00:00:00", "1"),
            ("moment64", Operator::Contains, "12:34:56.789", "1"),
            ("moment64", Operator::EndsWith, ".789", "1"),
            ("moment64", Operator::Regex, "^2024.*\\.789$", "1"),
            ("mood", Operator::Equals, "happy", "1"),
            ("mood", Operator::Contains, "app", "1"),
            ("mood", Operator::NotContains, "app", "1"),
            ("mood", Operator::StartsWith, "sa", "1"),
            ("mood", Operator::EndsWith, "py", "1"),
            ("mood", Operator::Regex, "^h", "1"),
            (
                "uid",
                Operator::Equals,
                "00000000-0000-0000-0000-000000000000",
                "1",
            ),
            ("uid", Operator::Contains, "7c2a", "1"),
            ("uid", Operator::NotContains, "7c2a", "1"),
            ("uid", Operator::StartsWith, "018f", "1"),
            ("uid", Operator::EndsWith, "0001", "1"),
            ("uid", Operator::Regex, "^0{8}-", "1"),
            ("u64", Operator::InList, "0, 18446744073709551615", "2"),
        ];
        for (column, operator, value, expected) in cases {
            assert_eq!(
                matched(&connection, gallery, column, operator, value),
                expected,
                "{column} {operator:?} {value:?}"
            );
        }
        assert_eq!(
            matched(&connection, gallery, "id", Operator::Between, "1..2"),
            "2"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_percent_underscore_and_backslash_in_a_value_match_themselves() {
        let connection = live();
        let from = "dbdelve_dev.dbdelve_filter_probe";
        ran(&connection, &format!("DROP TABLE IF EXISTS {from}"));
        ran(
            &connection,
            &format!("CREATE TABLE {from} (v String) ENGINE = Memory"),
        );
        ran(
            &connection,
            &format!(
                "INSERT INTO {from} VALUES ('100%'), ('100x'), ('a_b'), ('axb'), ('50%off'), \
                 ('p\\\\q'), ('pxq'), ('a1'), ('ab'), ('1a'), ('a9z')"
            ),
        );
        let cases: [(Operator, &str, &str); 8] = [
            (Operator::Contains, "%", "2"),
            (Operator::StartsWith, "100%", "1"),
            (Operator::EndsWith, "%", "1"),
            (Operator::Contains, "_", "1"),
            (Operator::StartsWith, "a_", "1"),
            (Operator::NotContains, "_", "10"),
            (Operator::Contains, "\\", "1"),
            (Operator::EndsWith, "\\q", "1"),
        ];
        for (operator, value, expected) in cases {
            assert_eq!(
                matched(&connection, from, "v", operator, value),
                expected,
                "{operator:?} {value:?}"
            );
        }
        let regex = matched(&connection, from, "v", Operator::Regex, "^a\\d");
        ran(&connection, &format!("DROP TABLE {from}"));
        assert_eq!(regex, "2");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_system_settings_has_the_readonly_column() {
        let result = ran(
            &live(),
            "SELECT name, readonly FROM system.settings WHERE name = 'readonly'",
        );
        assert_eq!(
            cells(&result),
            [[Some("readonly".into()), Some("0".into())]]
        );
    }

    #[test]
    #[ignore = "requires the dev bastions and ClickHouse configured through dbdelve_SSH_CONFIG and dbdelve_CLICKHOUSE_URL"]
    fn live_ssh_a_query_runs_through_the_bastion() {
        // The compose server as the bastion sees it, by service name and the
        // port inside the network.
        let connection = Connection::open(&ServerConfig {
            host: "clickhouse".into(),
            port: Some(DEFAULT_PORT),
            ssh: crate::db::ssh::live_bastion("dbdelve-bastion"),
            ..live_config()
        })
        .expect("connection should open");
        assert_eq!(
            text(&ran(&connection, "SELECT count() FROM conformance")),
            "3"
        );
        let token = CancelToken::default();
        let error = std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                connection.query(
                    "SELECT sleepEachRow(1) FROM numbers(30) SETTINGS max_block_size = 1",
                    &token,
                    Fetch::default(),
                )
            });
            std::thread::sleep(Duration::from_millis(1500));
            connection
                .cancel(&token)
                .expect("the kill should reach the server");
            running.join().unwrap().unwrap_err()
        });
        assert!(is_cancel(Engine::ClickHouse, &error), "{}", error.message);
    }

    const FORMAT_AND_DATABASE: &str = "default_format=TSV&database=dbdelve_dev";

    fn answer(sql: &str, extra: &str) -> (u16, String) {
        let (status, body) = raw(sql, &format!("{FORMAT_AND_DATABASE}{extra}"));
        (status, body.trim_end_matches('\n').to_string())
    }

    fn says(sql: &str) -> String {
        match answer(sql, "") {
            (200, body) => body,
            (status, body) => panic!("{sql:?} answered {status}: {body}"),
        }
    }

    fn refusal(sql: &str, extra: &str) -> String {
        match answer(sql, extra) {
            (200, body) => panic!("{sql:?} was not refused: {body}"),
            (_, body) => body,
        }
    }

    /// What `sql::misread` and the Read-only classifier take the server's
    /// lexer to do, checked against 26.8.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_servers_lexer_is_what_the_classifier_assumes() {
        assert_eq!(says("SELECT 1 AS `a\\`b` FORMAT TSVWithNames"), "a`b\n1");
        assert_eq!(
            says("SELECT 1 AS \"x\\\"y\" FORMAT TSVWithNames"),
            "x\"y\n1"
        );
        assert_eq!(says("SELECT 1 # note\n+ 1"), "2");
        assert_eq!(says("SELECT 1 #! note\n+ 1"), "2");
        assert!(refusal("SELECT 1 #note\n+ 1", "").contains("Unrecognized token"));
        assert!(refusal("SELECT 1 #\n+ 1", "").contains("Unrecognized token"));
        assert_eq!(says("SELECT 1 AS a$b"), "1");
        assert_eq!(says("SELECT 1 AS a$$ , 2 AS b$$"), "1\t2");
        assert_eq!(says("SELECT $$a'b;c$$"), "a\\'b;c");
        for space in [
            '\u{85}', '\u{a0}', '\u{2003}', '\u{200b}', '\u{2060}', '\u{3000}', '\u{feff}',
        ] {
            assert_eq!(
                says(&format!("SELECT{space}1")),
                "1",
                "U+{:04X}",
                space as u32
            );
        }
        assert!(refusal("SELECT\u{1680}1", "").contains("Unrecognized token"));
        assert_eq!(says("SELECT \u{2018}abc\u{2019}"), "abc");
        assert_eq!(says("SELECT 1 AS \u{201c}x\u{201d}"), "1");
        assert_eq!(says("SELECT 1 -- note\r+ 5"), "1");
        assert_eq!(says("SELECT 1 -- note\n+ 5"), "6");
        assert_eq!(says("SELECT 1 /* a /* b */ + 7 */ + 2"), "3");
        assert!(refusal("SELECT 1 /* a /* b */ + 7", "").contains("not closed"));
    }

    /// What the Read-only hold (`readonly=2`) and the floor reads of the
    /// classifier take the server to do.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_readonly_2_is_what_the_classifier_takes_it_to_be() {
        let held = "&readonly=2";
        for sql in [
            "SET readonly = 0",
            "SET readonly = 1",
            "SELECT 1 SETTINGS readonly = 0",
            "INSERT INTO scratch SETTINGS readonly = 0 VALUES (9, 'x')",
        ] {
            assert!(
                refusal(sql, held).contains("Cannot modify 'readonly' setting"),
                "{sql}"
            );
        }
        assert!(refusal("SET max_threads = 2 PARALLEL WITH CREATE TABLE refused_pw (x Int8) ENGINE = Memory", held)
            .contains("readonly mode"));
        assert_eq!(
            says("SELECT count() FROM system.tables WHERE name = 'refused_pw'"),
            "0"
        );
        // A left side with output is refused by the server whatever the hold.
        assert!(refusal("SELECT 1 PARALLEL WITH SELECT 2", held).contains("PARALLEL WITH"));
        assert!(
            refusal("SET max_threads = 2 PARALLEL WITH SELECT 2", held).contains("PARALLEL WITH")
        );
        assert!(refusal("SELECT 1 INTO OUTFILE 'x'", held).contains("INTO_OUTFILE_NOT_ALLOWED"));

        for kind in ["AST", "SYNTAX"] {
            let sql = format!("EXPLAIN {kind} INSERT INTO scratch VALUES (7, 'x')");
            assert_eq!(answer(&sql, held).0, 200, "{sql}");
        }
        // Plans the insert without running it, and a hold refuses the plan.
        let pipeline = "EXPLAIN PIPELINE INSERT INTO scratch VALUES (7, 'x')";
        assert!(refusal(pipeline, held).contains("readonly mode"));
        assert_eq!(answer(pipeline, "").0, 200);
        assert!(
            refusal("EXPLAIN ESTIMATE INSERT INTO scratch VALUES (7, 'x')", held)
                .contains("Only SELECT")
        );
        assert!(
            refusal("EXPLAIN INSERT INTO scratch VALUES (7, 'x')", held).contains("Only SELECT")
        );
        assert_eq!(says("SELECT count() FROM scratch"), "0");

        assert_eq!(answer("CHECK TABLE conformance", held).0, 200);
        let watch = refusal("WATCH active_accounts", held);
        assert!(
            watch.contains("NOT_IMPLEMENTED"),
            "WATCH is a read, refused for its storage: {watch}"
        );

        assert_eq!(
            answer(
                "ALTER TABLE edit_heavy UPDATE qty = qty WHERE id = 1 SETTINGS mutations_sync = 2",
                ""
            )
            .0,
            200
        );
        assert!(
            refusal(
                "ALTER TABLE edit_heavy UPDATE qty = qty WHERE id = 1 SETTINGS mutations_sync = 2",
                held
            )
            .contains("readonly mode")
        );
    }

    const SLOW: &str = "SELECT sleep(1), 1";
    const RUNAWAY: &str = "SELECT sleepEachRow(1) FROM numbers(60) SETTINGS max_block_size = 1";

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_reads_run_at_once_take_the_time_of_one() {
        crate::db::at_once::reads_overlap(
            &live_any(live_config()),
            Engine::ClickHouse,
            SLOW,
            "INSERT INTO scratch VALUES (1, 'x')",
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_cancel_stops_every_read_run_at_once() {
        crate::db::at_once::a_cancel_stops_them_all(
            &live_any(live_config()),
            Engine::ClickHouse,
            RUNAWAY,
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_cancel_of_one_read_run_at_once_leaves_the_other_running() {
        crate::db::at_once::a_cancel_stops_only_its_own(
            &live_any(live_config()),
            Engine::ClickHouse,
            RUNAWAY,
            "SELECT sleepEachRow(1) FROM numbers(6) SETTINGS max_block_size = 1",
        );
    }

    fn kinds_of(catalog: &Catalog, schema: &str) -> Vec<(String, RelationKind)> {
        catalog
            .schemas
            .iter()
            .find(|found| found.name == schema)
            .map(|found| {
                found
                    .relations
                    .iter()
                    .map(|relation| (relation.name.clone(), relation.kind))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_key_column_named_with_a_space_or_a_backtick_still_orders_paging() {
        let connection = live();
        ran(
            &connection,
            "CREATE OR REPLACE TABLE dbdelve_dev.fix2_keys (`user id` UInt64, `we\\`ird` UInt64, n UInt64) \
             ENGINE = MergeTree ORDER BY (`user id`, `we\\`ird`)",
        );
        let structure = live_any(live_config())
            .structure("dbdelve_dev", "fix2_keys")
            .unwrap();
        ran(&connection, "DROP TABLE dbdelve_dev.fix2_keys");
        assert_eq!(structure.row_key(), ["user id", "we`ird"]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_every_seeded_kind_lands_right_and_the_system_databases_stay_out() {
        let catalog = live_any(live_config()).catalog().unwrap();
        let seeded = kinds_of(&catalog, "dbdelve_dev");
        let kind = |name: &str| {
            seeded
                .iter()
                .find(|(found, _)| found == name)
                .unwrap_or_else(|| panic!("{name} is not listed: {seeded:?}"))
                .1
        };
        assert_eq!(kind("accounts"), RelationKind::Table);
        assert_eq!(kind("events"), RelationKind::Table);
        assert_eq!(kind("scratch"), RelationKind::Table);
        assert_eq!(kind("plan_seats"), RelationKind::Dictionary);
        assert_eq!(kind("events_per_kind"), RelationKind::Table);
        assert_eq!(kind("active_accounts"), RelationKind::View);
        assert_eq!(kind("events_per_kind_mv"), RelationKind::MaterializedView);
        assert_eq!(
            kinds_of(&catalog, "analytics"),
            [("page_views".to_string(), RelationKind::Table)]
        );
        assert!(
            catalog
                .schemas
                .iter()
                .all(|schema| !Engine::ClickHouse.is_system_database(&schema.name)),
            "{:?}",
            catalog.schemas.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_servers_own_row_and_byte_counts_ride_on_the_catalog() {
        let connection = live_any(live_config());
        let catalog = connection.catalog().unwrap();
        let relation = |name: &str| {
            catalog
                .schemas
                .iter()
                .find(|schema| schema.name == "dbdelve_dev")
                .and_then(|schema| schema.relations.iter().find(|r| r.name == name))
                .unwrap()
        };
        assert_eq!(relation("events").rows, Some(1_000_000));
        assert!(relation("events").size.is_some_and(|bytes| bytes > 0));
        assert_eq!(relation("accounts").rows, Some(50));
        assert_eq!(relation("active_accounts").rows, None);
        assert_eq!(relation("active_accounts").size, None);
        assert!(connection.sizes().unwrap().is_empty());
        assert!(connection.routines().unwrap().schemas.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_materialized_view_without_a_target_hides_its_inner_table() {
        let connection = live();
        let view = format!("dbdelve_tmp_{}", marker());
        ran(
            &connection,
            &format!(
                "CREATE MATERIALIZED VIEW `{view}` ENGINE = MergeTree ORDER BY kind \
                 AS SELECT kind FROM events"
            ),
        );
        let inner = text(&ran(
            &connection,
            "SELECT count() FROM system.tables WHERE startsWith(name, '.inner')",
        ));
        let catalog = live_any(live_config()).catalog();
        ran(&connection, &format!("DROP VIEW `{view}`"));
        let listed = kinds_of(&catalog.unwrap(), "dbdelve_dev");
        assert!(inner.parse::<u32>().unwrap() > 0, "no .inner table to hide");
        assert!(
            listed
                .iter()
                .any(|(name, kind)| *name == view && *kind == RelationKind::MaterializedView),
            "{listed:?}"
        );
        assert!(
            listed.iter().all(|(name, _)| !name.starts_with(".inner")),
            "{listed:?}"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_databases_list_flags_the_one_connected_to() {
        let databases = live_any(live_config())
            .databases(Engine::ClickHouse)
            .unwrap();
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
        assert!(databases.names.iter().any(|name| name == "analytics"));
        assert!(databases.names.iter().any(|name| name == "dbdelve_dev"));
        assert_eq!(databases.names.last().map(String::as_str), Some("system"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_ddl_of_a_table_and_a_view_is_the_servers_own() {
        let connection = live_any(live_config());
        let table = connection
            .ddl("dbdelve_dev", "accounts", RelationKind::Table)
            .unwrap();
        let view = connection
            .ddl("dbdelve_dev", "active_accounts", RelationKind::View)
            .unwrap();
        assert!(
            table.starts_with("CREATE TABLE dbdelve_dev.accounts"),
            "{table}"
        );
        assert!(table.contains('\n') && table.ends_with(';'), "{table}");
        assert!(view.contains("VIEW dbdelve_dev.active_accounts"), "{view}");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_the_ddl_of_a_materialized_view_and_a_dictionary_is_the_servers_own() {
        let connection = live_any(live_config());
        let view = connection
            .ddl(
                "dbdelve_dev",
                "events_per_kind_mv",
                RelationKind::MaterializedView,
            )
            .unwrap();
        let dictionary = connection
            .ddl("dbdelve_dev", "plan_seats", RelationKind::Table)
            .unwrap();
        assert!(
            view.starts_with("CREATE MATERIALIZED VIEW dbdelve_dev.events_per_kind_mv"),
            "{view}"
        );
        assert!(view.ends_with(';'), "{view}");
        assert!(
            dictionary.starts_with("CREATE DICTIONARY dbdelve_dev.plan_seats"),
            "{dictionary}"
        );
        assert!(
            dictionary.contains('\n') && dictionary.ends_with(';'),
            "{dictionary}"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_events_shows_its_keys_and_accounts_keys_its_rows() {
        let connection = live_any(live_config());
        let events = connection.structure("dbdelve_dev", "events").unwrap();
        let named = |name: &str| {
            events
                .indexes
                .iter()
                .find(|index| index.name == name)
                .map(|index| index.definition.as_str())
        };
        assert_eq!(
            named("sorting key"),
            Some("ORDER BY (account_id, occurred_at)")
        );
        assert_eq!(
            named("partition key"),
            Some("PARTITION BY toYYYYMM(occurred_at)")
        );
        assert_eq!(named("sampling key"), None);
        assert_eq!(events.row_key(), ["account_id", "occurred_at"]);
        let kind = events.columns.iter().find(|c| c.name == "kind").unwrap();
        assert_eq!(kind.data_type, "LowCardinality(String)");
        assert!(!kind.nullable);

        let accounts = connection.structure("dbdelve_dev", "accounts").unwrap();
        assert_eq!(accounts.row_key(), ["id"]);
        let email = accounts.columns.iter().find(|c| c.name == "email").unwrap();
        assert!(email.nullable);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_computed_column_a_skip_index_and_a_sample_key_are_shown() {
        let connection = live_any(live_config());
        let table = "dbdelve_structure_probe";
        let create = format!(
            "CREATE TABLE dbdelve_dev.{table} (
                id UInt64,
                ts DateTime,
                kept String MATERIALIZED toString(id),
                spelled String ALIAS concat('x', toString(id)),
                plain UInt8 DEFAULT 3,
                fed UInt8 EPHEMERAL 5,
                maybe LowCardinality(Nullable(String)),
                INDEX by_ts ts TYPE minmax GRANULARITY 2
            ) ENGINE = MergeTree ORDER BY (id, toDate(ts)) SAMPLE BY id"
        );
        let drop = format!("DROP TABLE IF EXISTS dbdelve_dev.{table}");
        let cancel = CancelToken::default();
        connection.query(&drop, &cancel, Fetch::default()).unwrap();
        connection
            .query(&create, &cancel, Fetch::default())
            .unwrap();
        let structure = connection.structure("dbdelve_dev", table);
        connection.query(&drop, &cancel, Fetch::default()).unwrap();
        let structure = structure.unwrap();

        let default = |name: &str| {
            structure
                .columns
                .iter()
                .find(|column| column.name == name)
                .unwrap()
                .clone()
        };
        assert_eq!(
            default("kept").default.as_deref(),
            Some("MATERIALIZED toString(id)")
        );
        assert_eq!(
            default("spelled").default.as_deref(),
            Some("ALIAS concat('x', toString(id))")
        );
        assert_eq!(default("plain").default.as_deref(), Some("3"));
        assert_eq!(default("fed").default.as_deref(), Some("EPHEMERAL 5"));
        assert_eq!(default("id").default, None);
        assert!(default("maybe").nullable);
        assert!(!default("ts").nullable);
        assert!(Engine::ClickHouse.generates(&default("kept")));
        assert!(Engine::ClickHouse.generates(&default("spelled")));
        assert!(!Engine::ClickHouse.generates(&default("plain")));
        assert!(!Engine::ClickHouse.generates(&default("id")));

        let definitions: Vec<(&str, &str)> = structure
            .indexes
            .iter()
            .map(|index| (index.name.as_str(), index.definition.as_str()))
            .collect();
        assert_eq!(
            definitions,
            [
                ("sorting key", "ORDER BY (id, toDate(ts))"),
                ("sampling key", "SAMPLE BY id"),
                ("by_ts", "ts TYPE minmax GRANULARITY 2"),
            ]
        );
        assert!(structure.row_key().is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_view_and_a_dictionary_have_columns_and_no_keys() {
        let connection = live_any(live_config());
        for relation in ["active_accounts", "plan_seats", "events_per_kind_mv"] {
            let structure = connection.structure("dbdelve_dev", relation).unwrap();
            assert!(!structure.columns.is_empty(), "{relation}");
            assert!(structure.indexes.is_empty(), "{relation}");
            assert!(structure.constraints.is_empty(), "{relation}");
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_copied_drop_and_truncate_run_on_throwaway_objects() {
        let connection = live();
        let schema = "dbdelve_dev";
        ran(
            &connection,
            "CREATE OR REPLACE TABLE dbdelve_dev.copy_drop_source (n UInt64) ENGINE = MergeTree ORDER BY n",
        );
        ran(
            &connection,
            "CREATE OR REPLACE TABLE dbdelve_dev.copy_drop_table (n UInt64) ENGINE = MergeTree ORDER BY n",
        );
        ran(
            &connection,
            "CREATE OR REPLACE TABLE dbdelve_dev.copy_drop_foreign (n UInt64) ENGINE = MergeTree ORDER BY n",
        );
        ran(
            &connection,
            "CREATE OR REPLACE VIEW dbdelve_dev.copy_drop_view AS SELECT n FROM dbdelve_dev.copy_drop_source",
        );
        ran(
            &connection,
            "CREATE OR REPLACE MATERIALIZED VIEW dbdelve_dev.copy_drop_mv ENGINE = MergeTree ORDER BY n AS SELECT n FROM dbdelve_dev.copy_drop_source",
        );
        ran(
            &connection,
            "CREATE OR REPLACE DICTIONARY dbdelve_dev.copy_drop_dictionary (n UInt64) PRIMARY KEY n \
             SOURCE(CLICKHOUSE(DB 'dbdelve_dev' TABLE 'copy_drop_source' USER 'dbdelve' PASSWORD 'dbdelve')) \
             LAYOUT(FLAT()) LIFETIME(0)",
        );
        ran(
            &connection,
            "INSERT INTO dbdelve_dev.copy_drop_table VALUES (1), (2)",
        );

        ran(
            &connection,
            &truncate_sql(Engine::ClickHouse, schema, "copy_drop_table"),
        );
        let rows = cells(&ran(
            &connection,
            "SELECT count() FROM dbdelve_dev.copy_drop_table",
        ));
        assert_eq!(rows[0][0].as_deref(), Some("0"));

        for (name, kind) in [
            ("copy_drop_table", RelationKind::Table),
            ("copy_drop_foreign", RelationKind::ForeignTable),
            ("copy_drop_view", RelationKind::View),
            ("copy_drop_mv", RelationKind::MaterializedView),
            ("copy_drop_dictionary", RelationKind::Dictionary),
        ] {
            let statement = drop_sql(Engine::ClickHouse, schema, name, kind).unwrap();
            ran(&connection, &statement);
            let left = cells(&ran(
                &connection,
                &format!(
                    "SELECT count() FROM system.tables WHERE database = 'dbdelve_dev' AND name = '{name}'"
                ),
            ));
            assert_eq!(left[0][0].as_deref(), Some("0"), "{statement}");
        }

        let source = drop_sql(
            Engine::ClickHouse,
            schema,
            "copy_drop_source",
            RelationKind::Table,
        )
        .unwrap();
        ran(&connection, &source);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_capped_preview_exports_to_csv_with_its_header() {
        let connection = live();
        let result = ran(
            &connection,
            &select_top_sql(Engine::ClickHouse, "dbdelve_dev", "conformance").unwrap(),
        );
        assert!(!result.rows.is_empty());
        let csv = render_rows_as(RowsAs::CsvWithHeader, Engine::ClickHouse, None, &result);
        let lines: Vec<&str> = csv.lines().collect();
        let header: Vec<&str> = result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(lines.len(), result.rows.len() + 1);
        assert_eq!(lines[0], header.join(","));
    }

    #[test]
    fn a_structure_query_leaves_placeholder_text_inside_a_name_alone() {
        assert_eq!(
            structure_sql("{schema}.{relation}", "{relation}", "{schema}"),
            "'{relation}'.'{schema}'"
        );
    }

    #[test]
    fn a_key_list_is_read_into_the_columns_it_names() {
        let columns = |list: &str| key_columns(list);
        assert_eq!(columns("a, b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(columns("(a, b)"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(
            columns(r#"`user id`, `we\`ird`, "q""#),
            Some(vec!["user id".into(), "we`ird".into(), "q".into()])
        );
        assert_eq!(columns("toDate(ts)"), None);
        assert_eq!(columns("a, toDate(ts)"), None);
        assert_eq!(columns("a + b"), None);
        assert_eq!(columns("`open"), None);
    }

    #[test]
    fn a_structure_query_quotes_a_name_holding_a_quote_and_a_backslash() {
        assert_eq!(
            structure_sql("{schema}.{relation}", "a'b", r"c\d"),
            r"'a''b'.'c\\d'"
        );
    }

    fn conformance_subject() -> crate::db::conformance::Subject {
        use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
        Subject {
            connection: live_any(live_config()),
            engine: Engine::ClickHouse,
            schema: "dbdelve_dev",
            relation: "conformance",
            key: "id",
            columns: &COLUMNS,
            read: sql_read(Engine::ClickHouse, "dbdelve_dev"),
            endless: "SELECT number FROM system.numbers",
            runaway: RUNAWAY,
            read_only_refusal: "readonly mode",
            fixture: Some(Fixture {
                types: &[
                    "Int32",
                    "String",
                    "Nullable(Int64)",
                    "Nullable(Decimal(10, 2))",
                    "Nullable(Float64)",
                    "Nullable(Bool)",
                    "Nullable(Date)",
                    "Nullable(DateTime('UTC'))",
                    "Nullable(String)",
                ],
                renders: &[
                    ("fraction", 0, "12.5"),
                    ("flag", 0, "true"),
                    ("flag", 1, "false"),
                ],
                find: Box::new(|key| sql_find(Engine::ClickHouse, "dbdelve_dev", key)),
            }),
            _guard: None,
        }
    }

    fn previewed(connection: &Connection, table: &str) -> QueryResult {
        connection
            .generated(
                &format!("SELECT * FROM `dbdelve_dev`.`{table}`"),
                &CancelToken::default(),
                Fetch::default(),
            )
            .unwrap()
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_each_seeded_table_is_described_as_the_design_says() {
        use crate::db::UpdateSpelling::{AlterUpdate, Update};
        let connection = live();
        let described = |table: &str| connection.describe("dbdelve_dev", table).unwrap().unwrap();
        assert_eq!(described("edit_lw").update, Update);
        assert_eq!(described("edit_heavy").update, AlterUpdate);
        assert_eq!(described("edit_sum").update, AlterUpdate);
        assert_eq!(described("edit_dupes").update, AlterUpdate);
        assert_eq!(described("edit_lw").delete_refusal, None);
        assert!(described("edit_proj").delete_refusal.is_some());
        assert!(
            connection
                .describe("dbdelve_dev", "no_such_table")
                .unwrap()
                .is_none()
        );

        let lw = previewed(&connection, "edit_lw");
        let target = lw.edit.expect("edit_lw is editable");
        assert_eq!(target.keys, vec![0, 1, 2]);
        assert_eq!(
            target.row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![0] }
        );
        for table in ["scratch", "active_accounts", "plan_seats"] {
            let result = previewed(&connection, table);
            assert_eq!(result.edit, None, "{table}");
            assert!(result.read_only_reason.is_some(), "{table}");
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_modified_block_setting_shows_in_the_next_describe() {
        use crate::db::UpdateSpelling::{AlterUpdate, Update};
        let connection = live();
        let table = format!("edit_modify_{}", marker());
        let name = format!("dbdelve_dev.{table}");
        ran(
            &connection,
            &format!("CREATE TABLE {name} (id UInt32) ENGINE = MergeTree ORDER BY id"),
        );
        let update = |connection: &Connection| {
            connection
                .describe("dbdelve_dev", &table)
                .unwrap()
                .unwrap()
                .update
        };
        assert_eq!(update(&connection), AlterUpdate);
        ran(
            &connection,
            &format!(
                "ALTER TABLE {name} MODIFY SETTING enable_block_number_column = 1, \
                 enable_block_offset_column = 1"
            ),
        );
        assert_eq!(update(&connection), Update);
        ran(&connection, &format!("DROP TABLE {name}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_typed_update_the_server_refuses_gains_the_alter_hint() {
        let connection = live();
        let name = format!("dbdelve_dev.typed_update_{}", marker());
        ran(
            &connection,
            &format!("CREATE TABLE {name} (id UInt32, v String) ENGINE = MergeTree ORDER BY id"),
        );
        ran(&connection, &format!("INSERT INTO {name} VALUES (1, 'a')"));
        let typed = format!("UPDATE {name} SET v = 'b' WHERE id = 1");
        let error = run(&connection, &typed).unwrap_err();
        assert!(
            error
                .message
                .contains("Lightweight updates are not supported"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("(NOT_IMPLEMENTED)"),
            "{}",
            error.message
        );
        assert!(
            error.message.ends_with(edit::LIGHTWEIGHT_UPDATE_HINT),
            "{}",
            error.message
        );
        assert_eq!(
            text(&ran(&connection, &format!("SELECT v FROM {name}"))),
            "a"
        );
        ran(&connection, &format!("DROP TABLE {name}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_typed_update_refused_for_the_engine_gains_no_alter_hint() {
        let connection = live();
        let name = format!("dbdelve_dev.summing_{}", marker());
        ran(
            &connection,
            &format!(
                "CREATE TABLE {name} (id UInt32, v UInt32) ENGINE = SummingMergeTree ORDER BY id \
                 SETTINGS enable_block_number_column = 1, enable_block_offset_column = 1"
            ),
        );
        ran(&connection, &format!("INSERT INTO {name} VALUES (1, 1)"));
        let error = run(
            &connection,
            &format!("UPDATE {name} SET v = 2 WHERE id = 1"),
        )
        .unwrap_err();
        assert!(
            !error.message.contains(edit::LIGHTWEIGHT_UPDATE_HINT),
            "{}",
            error.message
        );
        ran(&connection, &format!("DROP TABLE {name}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_every_value_read_is_found_again_by_its_match_term() {
        let connection = live();
        let table = format!("edit_match_{}", marker());
        let name = format!("dbdelve_dev.{table}");
        ran(
            &connection,
            &format!(
                "CREATE TABLE {name} (id UInt32, s String, ns Nullable(String), f Float64, \
                 nan Float64, ninf Float32, d DateTime64(6, 'UTC'), dt DateTime('UTC'), \
                 day Date, dec Decimal(10, 2), arr Array(String), m Map(String, UInt8), \
                 tup Tuple(String, Nullable(UInt8)), e Enum8('a' = 1, 'b' = 2), uid UUID, \
                 ip IPv6, lc LowCardinality(String), flag Bool, fs FixedString(4), \
                 big UInt256, pt Point) \
                 ENGINE = MergeTree ORDER BY id"
            ),
        );
        ran(
            &connection,
            &format!(
                "INSERT INTO {name} VALUES \
                 (1, 'it\\'s a \\\\ \\t tab', NULL, 1.1, nan, -inf, \
                  '2024-01-02 03:04:05.123456', '2024-01-02 03:04:05', '2024-01-02', 12.50, \
                  ['a\\'b', ''], {{'k': 1}}, ('x\\'', NULL), 'b', \
                  '61f0c404-5cb3-11e7-907b-a6006ad3dba0', '::1', 'lc', true, 'abcd', \
                  115792089237316195423570985008687907853269984665640564039457584007913129639935, \
                  (1.5, -2.25)), \
                 (2, '', 'present', -0.0, 1e300, inf, '1969-12-31 23:59:59.000001', \
                  '2024-01-02 03:04:06', '1970-01-01', -0.01, [], {{}}, ('', 7), 'a', \
                  '00000000-0000-0000-0000-000000000000', '2001:db8::ff00:42:8329', '', false, \
                  'wxyz', 0, (0, 0))"
            ),
        );
        let read = previewed(&connection, &table);
        let target = read.edit.clone().expect("a MergeTree table is editable");
        assert_eq!(read.rows.len(), 2);
        for (at, row) in read.rows.iter().enumerate() {
            let terms: Vec<String> = read
                .columns
                .iter()
                .enumerate()
                .map(|(column, described)| {
                    super::edit::match_value(
                        &described.name,
                        row.get(column),
                        described.data_type.as_deref().unwrap_or_default(),
                    )
                    .unwrap_or_else(|| panic!("row {at}: {} has no term", described.name))
                })
                .collect();
            let predicate = terms.join(" AND ");
            let delete = format!("DELETE FROM `dbdelve_dev`.`{table}` WHERE {predicate}");
            assert!(
                crate::sql::is_generated_write_on(Engine::ClickHouse, &delete),
                "{delete}"
            );
            let named: Vec<&str> = target
                .columns
                .iter()
                .flatten()
                .map(String::as_str)
                .collect();
            assert!(crate::sql::delete_matches_key_on(
                Engine::ClickHouse,
                &delete,
                &named
            ));
            let found = ran(
                &connection,
                &format!("SELECT count() FROM {name} WHERE {predicate}"),
            );
            assert_eq!(text(&found), "1", "row {at}: {predicate}");
        }
        ran(&connection, &format!("DROP TABLE {name}"));
    }

    const BLOCKS: &str = " SETTINGS enable_block_number_column = 1, enable_block_offset_column = 1";

    /// A throwaway `(id UInt32, label String)` MergeTree of its own name,
    /// holding `rows`.
    fn scratch(connection: &Connection, columns: &str, settings: &str, rows: &str) -> String {
        let table = format!("edit_guard_{}", marker());
        ran(
            connection,
            &format!(
                "CREATE TABLE dbdelve_dev.{table} ({columns}) ENGINE = MergeTree ORDER BY id\
                 {settings}"
            ),
        );
        ran(
            connection,
            &format!("INSERT INTO dbdelve_dev.{table} VALUES {rows}"),
        );
        table
    }

    /// The predicate naming the row of `read` whose id is `id`, by every value
    /// it was read with.
    fn named(read: &QueryResult, id: &str) -> String {
        let row = read
            .rows
            .iter()
            .find(|row| row.get(0) == Some(id))
            .unwrap_or_else(|| panic!("no row {id}"));
        read.columns
            .iter()
            .enumerate()
            .map(|(at, column)| {
                super::edit::match_value(
                    &column.name,
                    row.get(at),
                    column.data_type.as_deref().unwrap_or_default(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// The row `id` of `read` given `label`, spelled as `read`'s target says.
    fn relabel(table: &str, read: &QueryResult, id: &str, label: &str) -> String {
        let target = read.edit.as_ref().expect("an editable preview");
        let predicate = named(read, id);
        match target.update {
            crate::db::UpdateSpelling::Update => {
                format!("UPDATE `dbdelve_dev`.`{table}` SET `label` = '{label}' WHERE {predicate}")
            }
            crate::db::UpdateSpelling::AlterUpdate => format!(
                "ALTER TABLE `dbdelve_dev`.`{table}` UPDATE `label` = '{label}' WHERE \
                 {predicate} SETTINGS mutations_sync = 2"
            ),
        }
    }

    fn written(connection: &Connection, sql: &str) -> Result<QueryResult, DbError> {
        connection.generated(sql, &CancelToken::default(), Fetch::default())
    }

    fn labels(connection: &Connection, table: &str) -> String {
        text(&ran(
            connection,
            &format!("SELECT label FROM dbdelve_dev.{table} ORDER BY id, label"),
        ))
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_guarded_edit_lands_on_its_one_row_in_either_update_spelling() {
        use crate::db::UpdateSpelling::{AlterUpdate, Update};
        let connection = live();
        for (settings, spelling) in [(BLOCKS, Update), ("", AlterUpdate)] {
            let table = scratch(
                &connection,
                "id UInt32, label String, qty Int32",
                settings,
                "(1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
            );
            let read = previewed(&connection, &table);
            assert_eq!(read.edit.as_ref().unwrap().update, spelling);
            let update = relabel(&table, &read, "1", "uno");
            assert!(crate::sql::is_generated_write_on(
                Engine::ClickHouse,
                &update
            ));
            written(&connection, &update).unwrap_or_else(|error| panic!("{update}: {error}"));
            assert_eq!(
                labels(&connection, &table),
                "uno\ntwo\nthree",
                "{spelling:?}"
            );

            let delete = format!(
                "DELETE FROM `dbdelve_dev`.`{table}` WHERE {}",
                named(&read, "2")
            );
            written(&connection, &delete).unwrap_or_else(|error| panic!("{delete}: {error}"));
            assert_eq!(labels(&connection, &table), "uno\nthree", "{spelling:?}");
            ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_identical_rows_and_a_row_changed_behind_the_grid_are_refused_unwritten() {
        let connection = live();
        let table = scratch(
            &connection,
            "id UInt32, label String",
            BLOCKS,
            "(1, 'same'), (1, 'same'), (2, 'two')",
        );
        let read = previewed(&connection, &table);
        for statement in [
            relabel(&table, &read, "1", "changed"),
            format!(
                "DELETE FROM `dbdelve_dev`.`{table}` WHERE {}",
                named(&read, "1")
            ),
        ] {
            let error = written(&connection, &statement).unwrap_err();
            assert_eq!(
                error.message,
                "2 rows have exactly these values, so DBDelve will not change one of them."
            );
        }
        assert_eq!(labels(&connection, &table), "same\nsame\ntwo");

        ran(
            &connection,
            &format!("UPDATE dbdelve_dev.{table} SET label = 'behind' WHERE id = 2"),
        );
        let error = written(&connection, &relabel(&table, &read, "2", "mine")).unwrap_err();
        assert_eq!(
            error.message,
            "No row has these values any more; it was changed or deleted since it was read."
        );
        assert_eq!(labels(&connection, &table), "same\nsame\nbehind");
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_two_instants_shown_as_one_wall_clock_time_are_refused_unwritten() {
        let connection = live();
        // 05:30 and 06:30 UTC, either side of New York's clocks going back,
        // are both 01:30 there.
        let table = scratch(
            &connection,
            "id UInt32, label String, ts DateTime('America/New_York')",
            BLOCKS,
            "(1, 'same', 1730611800), (1, 'same', 1730615400)",
        );
        let read = previewed(&connection, &table);
        assert!(
            read.rows
                .iter()
                .all(|row| row.get(2) == Some("2024-11-03 01:30:00"))
        );
        for statement in [
            relabel(&table, &read, "1", "changed"),
            format!(
                "DELETE FROM `dbdelve_dev`.`{table}` WHERE {}",
                named(&read, "1")
            ),
        ] {
            let error = written(&connection, &statement).unwrap_err();
            assert_eq!(
                error.message,
                "2 rows have exactly these values, so DBDelve will not change one of them.",
                "{statement}"
            );
        }
        assert_eq!(labels(&connection, &table), "same\nsame");
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_batch_stops_at_its_first_refusal_and_what_ran_stays() {
        let connection = live();
        for settings in [BLOCKS, ""] {
            let table = scratch(
                &connection,
                "id UInt32, label String",
                settings,
                "(1, 'one'), (2, 'two'), (3, 'three')",
            );
            let read = previewed(&connection, &table);
            ran(
                &connection,
                &format!(
                    "ALTER TABLE dbdelve_dev.{table} UPDATE label = 'behind' WHERE id = 2 \
                     SETTINGS mutations_sync = 2"
                ),
            );
            let batch = ["1", "2", "3"]
                .map(|id| format!("{};", relabel(&table, &read, id, "edited")))
                .join("\n");
            let error = written(&connection, &batch).unwrap_err();
            assert_eq!(
                error.message,
                "1 of 3 edits was applied and stays; ClickHouse has no transaction. The second \
                 was refused, and the one after it was not run: No row has these values any \
                 more; it was changed or deleted since it was read."
            );
            assert_eq!(labels(&connection, &table), "edited\nbehind\nthree");
            ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_delete_a_projection_withholds_is_refused_before_its_guard() {
        let connection = live();
        let table = scratch(
            &connection,
            "id UInt32, label String, qty Int32, \
             PROJECTION by_label (SELECT label, sum(qty) GROUP BY label)",
            "",
            "(1, 'one', 10), (2, 'two', 20)",
        );
        let read = previewed(&connection, &table);
        let delete = format!(
            "DELETE FROM `dbdelve_dev`.`{table}` WHERE {}",
            named(&read, "1")
        );
        let error = written(&connection, &delete).unwrap_err();
        assert_eq!(
            error.message,
            "This table has projections and its lightweight_mutation_projection_mode is throw, \
             so rows cannot be deleted."
        );
        assert_eq!(labels(&connection, &table), "one\ntwo");
        written(&connection, &relabel(&table, &read, "1", "uno")).unwrap();
        assert_eq!(labels(&connection, &table), "uno\ntwo");
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_new_row_lands_each_gallery_type_through_a_quoted_literal() {
        let connection = live();
        let table = format!("edit_gallery_{}", marker());
        ran(
            &connection,
            &format!("CREATE TABLE dbdelve_dev.{table} AS dbdelve_dev.type_gallery"),
        );
        let seeded = ran(
            &connection,
            "SELECT * FROM dbdelve_dev.type_gallery WHERE id = 1",
        );
        let types: Vec<(String, String)> = connection
            .structure("dbdelve_dev", &table)
            .unwrap()
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.data_type.clone()))
            .collect();
        // Landing is checked by reading each column back as the grid shows it.
        let mut refused: Vec<String> = Vec::new();
        for (at, column) in seeded.columns.iter().enumerate().skip(1) {
            let value = seeded.rows[0].get(at);
            let insert = crate::sql::insert_row(
                Engine::ClickHouse,
                "dbdelve_dev",
                &table,
                &[("id", Some("1")), (column.name.as_str(), value)],
                &types,
            )
            .unwrap();
            if !crate::sql::is_generated_write_on(Engine::ClickHouse, &insert) {
                refused.push(column.name.clone());
                continue;
            }
            match written(&connection, &insert) {
                Err(_) => refused.push(column.name.clone()),
                Ok(_) => {
                    let back = ran(
                        &connection,
                        &format!(
                            "SELECT `{0}` FROM dbdelve_dev.{table} WHERE id = 1",
                            column.name
                        ),
                    );
                    if back.rows[0].get(0) != value {
                        refused.push(column.name.clone());
                    }
                }
            }
            ran(&connection, &format!("TRUNCATE TABLE dbdelve_dev.{table}"));
        }
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
        // A NUL the gate refuses, Nested's dotted subcolumns it cannot name,
        // and a state the server cannot parse from text.
        assert_eq!(refused, ["fixed", "nested.k", "nested.v", "state"]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_variant_and_json_text_with_backslashes_round_trips_through_an_insert() {
        let connection = live();
        let table = format!("edit_roundtrip_{}", marker());
        let name = format!("dbdelve_dev.{table}");
        ran(
            &connection,
            &format!(
                "CREATE TABLE {name} (id UInt32, v Variant(Array(String), Int32, String), \
                 dy Dynamic, doc JSON) ENGINE = MergeTree ORDER BY id"
            ),
        );
        ran(
            &connection,
            &format!(
                "INSERT INTO {name} VALUES (1, 'a\\\\b\\tc\\'d', 'p\\\\q\\n', \
                 '{{\"s\":\"x\\\\\\\\y\\\\tz\\\\\"\"}}'), \
                 (2, ['a\\\\b', 'c\\'d'], ['e\\\\f'], '{{}}')"
            ),
        );
        let types: Vec<(String, String)> = [
            ("id", "UInt32"),
            ("v", "Variant(Array(String), Int32, String)"),
            ("dy", "Dynamic"),
            ("doc", "JSON"),
        ]
        .map(|(column, data_type)| (column.to_string(), data_type.to_string()))
        .to_vec();
        let read = ran(&connection, &format!("SELECT * FROM {name} ORDER BY id"));
        assert_eq!(read.rows[0].get(1), Some("a\\b\tc'd"));
        assert_eq!(read.rows[0].get(3), Some(r#"{"s":"x\\y\tz\""}"#));
        assert_eq!(read.rows[1].get(1), Some(r"['a\\b','c\'d']"));
        for (at, row) in read.rows.iter().enumerate() {
            let id = (at + 3).to_string();
            let values: Vec<(&str, Option<&str>)> = std::iter::once(("id", Some(id.as_str())))
                .chain(
                    ["v", "dy", "doc"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, column)| (column, row.get(i + 1))),
                )
                .collect();
            // A compound variant is typed back as text, so it lands as a String;
            // only what a String variant, a Dynamic and a JSON hold must survive.
            let values: Vec<_> = values
                .into_iter()
                .filter(|(column, value)| {
                    !(*column == "v" || *column == "dy")
                        || !value.is_some_and(|v| v.starts_with('['))
                })
                .collect();
            let insert =
                crate::sql::insert_row(Engine::ClickHouse, "dbdelve_dev", &table, &values, &types)
                    .unwrap();
            assert!(crate::sql::is_generated_write_on(
                Engine::ClickHouse,
                &insert
            ));
            written(&connection, &insert).unwrap_or_else(|error| panic!("{insert}: {error}"));
        }
        let back = ran(&connection, &format!("SELECT * FROM {name} ORDER BY id"));
        assert_eq!(back.rows[2].get(1), read.rows[0].get(1));
        assert_eq!(back.rows[2].get(2), read.rows[0].get(2));
        assert_eq!(back.rows[2].get(3), read.rows[0].get(3));
        assert_eq!(back.rows[3].get(3), read.rows[1].get(3));
        ran(&connection, &format!("DROP TABLE {name}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_an_edited_cell_with_awkward_characters_reads_back_as_typed() {
        use crate::result_grid::PendingRow;
        let connection = live();
        for settings in [BLOCKS, ""] {
            let table = format!("edit_roundtrip_{}", marker());
            let name = format!("dbdelve_dev.{table}");
            ran(
                &connection,
                &format!(
                    "CREATE TABLE {name} (id UInt32, s String, arr Array(String), \
                     m Map(String, String)) ENGINE = MergeTree ORDER BY id{settings}"
                ),
            );
            ran(
                &connection,
                &format!(
                    "INSERT INTO {name} VALUES \
                     (1, 'a\\\\b \\'q\\' \\t tab \\n nl \\\\n', \
                      ['x\\\\y', 'it\\'s', 'a\\tb\\nc'], \
                      {{'k\\\\': 'v\\'\\t\\n\\\\'}}), \
                     (2, '', [], {{}})"
                ),
            );
            let read = previewed(&connection, &table);
            let target = read.edit.clone().expect("an editable preview");
            let types: Vec<(String, String)> = read
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone().unwrap_or_default()))
                .collect();
            assert_eq!(
                target.row_match,
                crate::db::RowMatch::EveryColumn { fixed: vec![0, 3] },
                "a Map is not assigned from text, and the key is not set"
            );
            for column in 1..3 {
                let typed = read.rows[0].get(column).unwrap().to_string();
                let current = previewed(&connection, &table);
                let row = current
                    .rows
                    .iter()
                    .find(|row| row.get(0) == Some("2"))
                    .unwrap();
                let name_of = types[column].0.clone();
                let pending = PendingRow {
                    schema: "dbdelve_dev".into(),
                    table: table.clone(),
                    sets: vec![(
                        name_of.clone(),
                        crate::result_grid::NewValue::Value(typed.clone().into()),
                    )],
                    keys: types
                        .iter()
                        .enumerate()
                        .filter_map(|(at, (n, _))| row.get(at).map(|v| (n.clone(), v.to_string())))
                        .collect(),
                    null_keys: Vec::new(),
                    update: target.update,
                    types: types.clone(),
                    held: Vec::new(),
                };
                let update = crate::sql::update_batch(Engine::ClickHouse, &[pending]).unwrap();
                written(&connection, &update).unwrap_or_else(|error| panic!("{update}: {error}"));
                let back = previewed(&connection, &table);
                let after = back
                    .rows
                    .iter()
                    .find(|row| row.get(0) == Some("2"))
                    .unwrap();
                assert_eq!(
                    after.get(column),
                    Some(typed.as_str()),
                    "{name_of} through {:?}",
                    target.update
                );
            }
            ran(&connection, &format!("DROP TABLE {name}"));
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_imported_rows_land_in_one_insert() {
        use crate::import_rows::Value;
        let connection = live();
        let table = scratch(
            &connection,
            "id UInt32, label String, flag Bool, day Date",
            "",
            "(0, 'zero', false, '2000-01-01')",
        );
        let (one, two, quoted, yes, no, day) = (
            Value::Number("1".into()),
            Value::Number("2".into()),
            Value::Text("it's \\ here".into()),
            Value::Bool(true),
            Value::Bool(false),
            Value::Text("2024-01-02".into()),
        );
        let columns = [
            ("id", "UInt32"),
            ("label", "String"),
            ("flag", "Bool"),
            ("day", "Date"),
        ];
        let insert = crate::sql::insert_rows(
            Engine::ClickHouse,
            "dbdelve_dev",
            &table,
            &columns,
            &[
                vec![Some(&one), Some(&quoted), Some(&yes), Some(&day)],
                vec![Some(&two), Some(&Value::Null), Some(&no), Some(&day)],
            ],
            false,
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(
            Engine::ClickHouse,
            &insert
        ));
        written(&connection, &insert).unwrap_or_else(|error| panic!("{insert}: {error}"));
        assert_eq!(
            cells(&ran(
                &connection,
                &format!("SELECT id, label, flag, day FROM dbdelve_dev.{table} ORDER BY id")
            )),
            [
                ["0", "zero", "false", "2000-01-01"].map(|cell| Some(cell.to_string())),
                ["1", "it's \\ here", "true", "2024-01-02"].map(|cell| Some(cell.to_string())),
                ["2", "", "false", "2024-01-02"].map(|cell| Some(cell.to_string())),
            ]
        );
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_row_with_every_awkward_value_is_edited_and_deleted_by_its_values() {
        let connection = live();
        for settings in [BLOCKS, ""] {
            let table = format!("edit_awkward_{}", marker());
            ran(
                &connection,
                &format!(
                    "CREATE TABLE dbdelve_dev.{table} (id UInt32, label String, \
                     ns Nullable(String), f Float64, d DateTime64(6, 'UTC'), \
                     arr Array(String), m Map(String, UInt8)) \
                     ENGINE = MergeTree ORDER BY id{settings}"
                ),
            );
            ran(
                &connection,
                &format!(
                    "INSERT INTO dbdelve_dev.{table} VALUES \
                     (1, 'one', NULL, nan, '2024-01-02 03:04:05.123456', ['a\\'b', ''], {{'k': 1}}), \
                     (2, 'two', 'x', 1.5, '2024-01-02 03:04:05.000001', [], {{}})"
                ),
            );
            let read = previewed(&connection, &table);
            let update = relabel(&table, &read, "1", "uno");
            written(&connection, &update).unwrap_or_else(|error| panic!("{update}: {error}"));
            assert_eq!(labels(&connection, &table), "uno\ntwo");
            let delete = format!(
                "DELETE FROM `dbdelve_dev`.`{table}` WHERE {}",
                named(&previewed(&connection, &table), "1")
            );
            written(&connection, &delete).unwrap_or_else(|error| panic!("{delete}: {error}"));
            assert_eq!(labels(&connection, &table), "two");
            ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_sorting_key_and_a_partition_key_column_cannot_be_edited() {
        let connection = live();
        let table = format!("edit_keys_{}", marker());
        ran(
            &connection,
            &format!(
                "CREATE TABLE dbdelve_dev.{table} (id UInt32, part UInt8, label String) \
                 ENGINE = MergeTree PARTITION BY part ORDER BY id"
            ),
        );
        ran(
            &connection,
            &format!("INSERT INTO dbdelve_dev.{table} VALUES (1, 1, 'one')"),
        );
        let target = previewed(&connection, &table).edit.unwrap();
        assert_eq!(
            target.row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![0, 1] }
        );
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_user_without_the_privilege_is_refused_after_the_guard_and_nothing_is_written() {
        let owner = live();
        for settings in [BLOCKS, ""] {
            let table = scratch(
                &owner,
                "id UInt32, label String",
                settings,
                "(1, 'one'), (2, 'two')",
            );
            // The reader holds no ALTER grant, and the mutation's own setting
            // is what `readonly` refuses first.
            for (user, refusal) in [
                ("dbdelve_noalter", "ACCESS_DENIED"),
                (
                    "dbdelve_reader",
                    if settings.is_empty() {
                        "READONLY"
                    } else {
                        "ACCESS_DENIED"
                    },
                ),
            ] {
                let connection = Connection::open(&ServerConfig {
                    user: user.into(),
                    ..live_config()
                })
                .unwrap();
                let read = previewed(&connection, &table);
                let update = relabel(&table, &read, "1", "denied");
                let error = written(&connection, &update).unwrap_err();
                assert!(
                    error.message.contains(refusal),
                    "{user} {settings:?}: {}",
                    error.message
                );
                if refusal == "READONLY" {
                    assert!(
                        error
                            .message
                            .starts_with("This ClickHouse user is read-only"),
                        "{}",
                        error.message
                    );
                }
            }
            assert_eq!(labels(&owner, &table), "one\ntwo");
            ran(&owner, &format!("DROP TABLE dbdelve_dev.{table}"));
        }
    }

    /// The batch the grid writes for `read` once the cell at `row`, `col` is
    /// set to `value`, built as a query tab's grid builds it.
    fn edited(read: &QueryResult, row: usize, col: usize, value: &str) -> String {
        use crate::result_grid::{NewValue, ResultGrid};
        let mut grid =
            ResultGrid::new(read.clone(), Mode::ReadWrite).with_engine(Engine::ClickHouse);
        assert!(grid.set_pending(row, col, NewValue::Value(value.into())));
        let batch = crate::sql::update_batch(Engine::ClickHouse, &grid.pending_updates()).unwrap();
        assert!(
            crate::sql::is_generated_write_on(Engine::ClickHouse, &batch),
            "{batch}"
        );
        batch
    }

    /// The `DELETE` the grid writes for `read`'s row `row`.
    fn deleted(read: &QueryResult, row: usize) -> String {
        use crate::result_grid::ResultGrid;
        let grid = ResultGrid::new(read.clone(), Mode::ReadWrite).with_engine(Engine::ClickHouse);
        let (schema, table, keys, nulls) = grid.row_key(row).unwrap();
        let keys: Vec<(&str, &str)> = keys.iter().map(|(c, v)| (c.as_str(), v.as_str())).collect();
        let nulls: Vec<&str> = nulls.iter().map(String::as_str).collect();
        crate::sql::delete_row_matching(
            Engine::ClickHouse,
            &schema,
            &table,
            &keys,
            &nulls,
            &grid.row_types(row),
        )
        .unwrap()
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_a_query_tabs_edit_lands_on_its_one_row_in_either_update_spelling() {
        use crate::db::UpdateSpelling::{AlterUpdate, Update};
        let connection = live();
        for (settings, spelling) in [(BLOCKS, Update), ("", AlterUpdate)] {
            let table = scratch(
                &connection,
                "id UInt32, label String, qty Int32",
                settings,
                "(1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
            );
            let read = ran(&connection, &format!("SELECT * FROM {table} ORDER BY id"));
            let target = read
                .edit
                .as_ref()
                .expect("a query tab's MergeTree rows are editable");
            assert_eq!(
                (target.schema.as_str(), target.table.as_str(), target.update),
                ("dbdelve_dev", table.as_str(), spelling)
            );
            let update = edited(&read, 0, 1, "uno");
            written(&connection, &update).unwrap_or_else(|error| panic!("{update}: {error}"));
            assert_eq!(
                labels(&connection, &table),
                "uno\ntwo\nthree",
                "{spelling:?}"
            );

            let delete = deleted(&read, 1);
            written(&connection, &delete).unwrap_or_else(|error| panic!("{delete}: {error}"));
            assert_eq!(labels(&connection, &table), "uno\nthree", "{spelling:?}");
            ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_some_columns_name_a_row_by_those_alone_and_rows_alike_in_them_are_refused() {
        let connection = live();
        let table = scratch(
            &connection,
            "id UInt32, label String, qty Int32",
            BLOCKS,
            "(1, 'one', 10), (2, 'two', 20), (3, 'two', 30), (4, 'same', 40), (4, 'same', 40)",
        );
        let alike = "2 rows have exactly these values, so DBDelve will not change one of them.";

        let read = ran(
            &connection,
            &format!("SELECT qty, label FROM {table} ORDER BY id, qty"),
        );
        let target = read.edit.as_ref().unwrap();
        assert_eq!(target.columns, [Some("qty".into()), Some("label".into())]);
        written(&connection, &edited(&read, 0, 1, "uno")).unwrap();
        assert_eq!(labels(&connection, &table), "uno\ntwo\ntwo\nsame\nsame");

        // `two` alone is two rows, whatever their quantities.
        let read = ran(
            &connection,
            &format!("SELECT label FROM {table} ORDER BY id"),
        );
        let error = written(&connection, &edited(&read, 1, 0, "dos")).unwrap_err();
        assert_eq!(error.message, alike);

        // Identical rows are two rows by every value too.
        let read = ran(&connection, &format!("SELECT * FROM {table} ORDER BY id"));
        for statement in [edited(&read, 3, 1, "changed"), deleted(&read, 3)] {
            let error = written(&connection, &statement).unwrap_err();
            assert_eq!(error.message, alike, "{statement}");
        }
        assert_eq!(labels(&connection, &table), "uno\ntwo\ntwo\nsame\nsame");
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_after_a_session_timezone_is_set_a_query_tabs_rows_are_not_matched() {
        let connection = live();
        let table = scratch(
            &connection,
            "id UInt32, label String, ts DateTime",
            BLOCKS,
            "(1, 'one', '2024-01-01 00:00:00')",
        );
        let select = format!("SELECT * FROM {table}");
        let before = ran(&connection, &select);
        assert!(before.edit.is_some());
        ran(&connection, "SET session_timezone = 'Asia/Tokyo'");
        let after = ran(&connection, &select);
        // The same instant, shown as another wall-clock time than the guard,
        // which runs without the session, would match it by.
        assert_ne!(cells(&before)[0][2], cells(&after)[0][2]);
        assert_eq!(after.edit, None);
        assert_eq!(after.read_only_reason.as_deref(), Some(edit::SESSION_SET));
        // A preview reads without the session, as its guard does.
        let preview = previewed(&connection, &table);
        assert_eq!(cells(&preview)[0][2], cells(&before)[0][2]);
        written(&connection, &edited(&preview, 0, 1, "uno")).unwrap();
        assert_eq!(labels(&connection, &table), "uno");
        ran(&connection, &format!("DROP TABLE dbdelve_dev.{table}"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_CLICKHOUSE_URL"]
    fn live_an_unqualified_name_is_read_where_use_moved_the_session_and_a_temporary_table_is_not() {
        let connection = live();
        ran(&connection, "USE analytics");
        let read = ran(&connection, "SELECT * FROM page_views LIMIT 1");
        let target = read.edit.expect("page_views is a MergeTree");
        assert_eq!(
            (target.schema.as_str(), target.table.as_str()),
            ("analytics", "page_views")
        );
        let temporary = format!("edit_tmp_{}", marker());
        ran(
            &connection,
            &format!("CREATE TEMPORARY TABLE {temporary} (id UInt32)"),
        );
        ran(&connection, &format!("INSERT INTO {temporary} VALUES (1)"));
        let read = ran(&connection, &format!("SELECT * FROM {temporary}"));
        assert_eq!(read.edit, None);
        assert_eq!(read.read_only_reason.as_deref(), Some(edit::TEMPORARY));
    }

    crate::db::conformance::suite!(
        live_conformance,
        "requires the repository development database configured through dbdelve_CLICKHOUSE_URL",
        conformance_subject:
        connects,
        query_round_trip,
        catalog_round_trip,
        structure_round_trip,
        types_render_as_expected,
        cancel_stops_a_long_query,
        row_limit_stops_an_eligible_read,
        row_limit_drains_an_ineligible_read,
        read_only_session_refuses_a_write,
        generated_writes_round_trip,
    );
}

//! The MySQL boundary.
//!
//! Closer to `postgres.rs` than to `sqlite.rs`, and in the one place that
//! matters most it is closer still: `query_iter` speaks the **text protocol**,
//! so the server formats every value before it leaves, exactly as Postgres's
//! simple query protocol does. Decimals keep their precision, dates arrive in
//! the server's own rendering, and a type nobody thought of formats itself.
//! Binary columns are the exception, and get the same hex treatment blobs get
//! everywhere else.
//!
//! It is also the engine that gives column provenance away free. Every row
//! description carries `org_table` and `org_name` — the table and the
//! pre-aliasing column each value was read from — so in-grid editing costs no
//! extra round trip, unlike the describe Postgres has to pay for.
//!
//! The catalog is `information_schema`, which is ordinary SQL, so the queries
//! below alias their columns to the names the shared assemblers in `mod.rs`
//! read and nothing else here has to know how a `Catalog` is built.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::mysql::consts::{ColumnFlags, ColumnType};
use ::mysql::prelude::Queryable;
use ::mysql::{Conn, OptsBuilder, SslOpts, Value};

use super::{
    Catalog, Cell, Column, DbError, EditTarget, Engine, QueryResult, ServerConfig, SslMode,
    Structure, assemble_catalog, assemble_foreign_keys, assemble_structure, non_utf8_error,
    percent_decoded, plain_error, required_cell,
};

/// Without this the driver waits out the OS SYN retry budget, so a host that
/// resolves but drops packets pins the UI in "Connecting…" for minutes with no
/// cancel. The Postgres side sets the same bound for the same reason.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// The port the server listens on when the profile does not say.
const DEFAULT_PORT: u16 = 3306;

/// Bounds the cancel connection's own handshake and its `KILL QUERY` round
/// trip, neither of which `CONNECT_TIMEOUT_SECONDS` reaches -- that one only
/// covers the TCP connect. A cancel that cannot be delivered in a few seconds
/// is not going to be, and without this bound a wedged or maxed-out server
/// hangs the background thread sending it forever, with nothing to show for
/// it.
const CANCEL_TIMEOUT_SECONDS: u64 = 5;

/// The collation id MySQL uses for "no character set at all". A value in such a
/// column is bytes, not text — but so is every number in the text protocol, so
/// this is never the whole test on its own.
const BINARY_COLLATION: u16 = 63;

/// The four schemas the server owns. Nobody wrote them and nobody wants them in
/// a sidebar listing what they wrote.
const SYSTEM_SCHEMAS: &str = "('information_schema', 'mysql', 'performance_schema', 'sys')";

const RELATIONS_SQL: &str = "
SELECT TABLE_SCHEMA AS schema_name,
       TABLE_NAME AS relation_name,
       CASE TABLE_TYPE
           WHEN 'BASE TABLE' THEN 'table'
           WHEN 'VIEW' THEN 'view'
       END AS relation_kind
FROM information_schema.TABLES
WHERE TABLE_SCHEMA NOT IN {system}
  AND TABLE_TYPE IN ('BASE TABLE', 'VIEW')
ORDER BY TABLE_SCHEMA, TABLE_NAME
";

// Every column is coalesced because the assembler refuses a null, and several of
// these are null for reasons that are not errors: `ROUTINE_DEFINITION` is null
// for a user without the privilege to read bodies, and `DTD_IDENTIFIER` is null
// for a procedure, which returns nothing by definition.
const ROUTINES_SQL: &str = "
SELECT routine.ROUTINE_SCHEMA AS schema_name,
       routine.ROUTINE_NAME AS routine_name,
       CASE routine.ROUTINE_TYPE
           WHEN 'FUNCTION' THEN 'function'
           WHEN 'PROCEDURE' THEN 'procedure'
       END AS routine_kind,
       COALESCE((
           SELECT GROUP_CONCAT(
                      CONCAT_WS(' ', parameter.PARAMETER_NAME, parameter.DTD_IDENTIFIER)
                      ORDER BY parameter.ORDINAL_POSITION
                      SEPARATOR ', '
                  )
           FROM information_schema.PARAMETERS AS parameter
           WHERE parameter.SPECIFIC_SCHEMA = routine.ROUTINE_SCHEMA
             AND parameter.SPECIFIC_NAME = routine.SPECIFIC_NAME
             -- Position zero is a function's return value, not an argument.
             AND parameter.ORDINAL_POSITION > 0
       ), '') AS identity_arguments,
       COALESCE(routine.DTD_IDENTIFIER, '') AS result_type,
       LOWER(COALESCE(routine.ROUTINE_BODY, '')) AS language,
       COALESCE(routine.ROUTINE_DEFINITION, '') AS definition
FROM information_schema.ROUTINES AS routine
WHERE routine.ROUTINE_SCHEMA NOT IN {system}
  AND routine.ROUTINE_TYPE IN ('FUNCTION', 'PROCEDURE')
ORDER BY routine.ROUTINE_SCHEMA, routine.ROUTINE_NAME
";

// The four structure queries name one relation, so they carry `{schema}` and
// `{relation}` placeholders substituted by `structure_sql`.
const STRUCTURE_COLUMNS_SQL: &str = "
SELECT COLUMN_NAME AS column_name,
       COLUMN_TYPE AS data_type,
       CASE IS_NULLABLE WHEN 'YES' THEN 'yes' ELSE 'no' END AS nullable,
       -- `COLUMN_DEFAULT` alone reports no default for the two cases where the
       -- server supplies the value itself, which would tell the user they must
       -- provide one.
       CASE
           WHEN EXTRA LIKE '%auto_increment%' THEN 'AUTO_INCREMENT'
           WHEN COALESCE(GENERATION_EXPRESSION, '') <> ''
               THEN CONCAT('GENERATED ALWAYS AS (', GENERATION_EXPRESSION, ')')
           ELSE COALESCE(COLUMN_DEFAULT, '')
       END AS column_default
FROM information_schema.COLUMNS
WHERE TABLE_SCHEMA = {schema}
  AND TABLE_NAME = {relation}
ORDER BY ORDINAL_POSITION
";

const STRUCTURE_INDEXES_SQL: &str = "
SELECT INDEX_NAME AS object_name,
       CONCAT(
           CASE WHEN NON_UNIQUE = 0 THEN 'UNIQUE INDEX' ELSE 'INDEX' END,
           ' (',
           GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ', '),
           ')'
       ) AS definition
FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = {schema}
  AND TABLE_NAME = {relation}
GROUP BY INDEX_NAME, NON_UNIQUE
ORDER BY INDEX_NAME
";

// `CHECK` is deliberately absent. Its clause lives in
// `information_schema.CHECK_CONSTRAINTS`, a table MySQL only grew in 8.0.16, and
// a Structure tab that fails wholesale against an older server is worse than one
// that shows the constraints every server has.
const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT table_constraint.CONSTRAINT_NAME AS object_name,
       CASE table_constraint.CONSTRAINT_TYPE
           WHEN 'FOREIGN KEY' THEN CONCAT(
               'FOREIGN KEY (', {columns}, ') REFERENCES ',
               COALESCE((
                   SELECT CONCAT(
                              key_column.REFERENCED_TABLE_NAME, ' (',
                              GROUP_CONCAT(
                                  key_column.REFERENCED_COLUMN_NAME
                                  ORDER BY key_column.ORDINAL_POSITION
                                  SEPARATOR ', '
                              ), ')'
                          )
                   FROM information_schema.KEY_COLUMN_USAGE AS key_column
                   WHERE key_column.CONSTRAINT_SCHEMA = table_constraint.CONSTRAINT_SCHEMA
                     AND key_column.CONSTRAINT_NAME = table_constraint.CONSTRAINT_NAME
                     AND key_column.TABLE_NAME = table_constraint.TABLE_NAME
                   -- Named beside an aggregate, so `only_full_group_by` -- on by
                   -- default since 5.7 -- requires it grouped. A foreign key
                   -- references one table, so this is one row either way.
                   GROUP BY key_column.REFERENCED_TABLE_NAME
               ), '')
           )
           ELSE CONCAT(table_constraint.CONSTRAINT_TYPE, ' (', {columns}, ')')
       END AS definition
FROM information_schema.TABLE_CONSTRAINTS AS table_constraint
WHERE table_constraint.TABLE_SCHEMA = {schema}
  AND table_constraint.TABLE_NAME = {relation}
  AND table_constraint.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')
ORDER BY table_constraint.CONSTRAINT_NAME
";

// A sibling of the constraint query rather than a second reading of its text:
// that one renders DDL for the Structure tab, this one wants the fields, and one
// query doing both would have to pick. `REFERENCED_TABLE_NAME IS NOT NULL` is
// what separates the foreign keys from every other key sharing the view, and
// `REFERENCED_TABLE_SCHEMA` is read rather than assumed because InnoDB takes a
// foreign key across databases.
const STRUCTURE_FOREIGN_KEYS_SQL: &str = "
SELECT COLUMN_NAME AS column_name,
       REFERENCED_TABLE_SCHEMA AS referenced_schema,
       REFERENCED_TABLE_NAME AS referenced_table,
       REFERENCED_COLUMN_NAME AS referenced_column
FROM information_schema.KEY_COLUMN_USAGE
WHERE TABLE_SCHEMA = {schema}
  AND TABLE_NAME = {relation}
  AND REFERENCED_TABLE_NAME IS NOT NULL
ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION
";

/// The columns a constraint covers, in key order. Spliced into the query above
/// twice, which is why it is a fragment rather than a second round trip.
const CONSTRAINT_COLUMNS_SQL: &str = "COALESCE((
    SELECT GROUP_CONCAT(
               key_column.COLUMN_NAME
               ORDER BY key_column.ORDINAL_POSITION
               SEPARATOR ', '
           )
    FROM information_schema.KEY_COLUMN_USAGE AS key_column
    WHERE key_column.CONSTRAINT_SCHEMA = table_constraint.CONSTRAINT_SCHEMA
      AND key_column.CONSTRAINT_NAME = table_constraint.CONSTRAINT_NAME
      AND key_column.TABLE_NAME = table_constraint.TABLE_NAME
), '')";

const PRIMARY_KEY_SQL: &str = "
SELECT COLUMN_NAME AS column_name
FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = {schema}
  AND TABLE_NAME = {relation}
  AND INDEX_NAME = 'PRIMARY'
ORDER BY SEQ_IN_INDEX
";

/// A `mysql://` URL, read by dbdelve rather than by the driver.
///
/// The driver has its own URL parser with a fixed key list, and it would refuse
/// the whole string over an `sslmode` it does not know — naming neither the
/// option nor the reason, which is the same trap the Postgres side documents.
/// dbdelve owns both TLS keys and builds the driver's options from fields.
pub fn config_from_url(url: &str) -> Result<ServerConfig, String> {
    let parsed =
        url::Url::parse(url).map_err(|error| format!("Connection URL is invalid: {error}"))?;

    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "Connection URL does not contain a host.".to_string())?
        .to_string();
    let database = parsed.path().trim_start_matches('/').to_string();
    if database.is_empty() {
        return Err("Connection URL does not contain a database.".into());
    }
    let user = percent_decoded(parsed.username())?;
    if user.is_empty() {
        return Err("Connection URL does not contain a username.".into());
    }

    let mut sslmode = SslMode::default();
    let mut root_certificate = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "sslmode" => sslmode = SslMode::parse(value.as_ref())?,
            "sslrootcert" => {
                root_certificate = Some(value.trim().to_string()).filter(|path| !path.is_empty());
            }
            // Refused rather than dropped. The driver's options are built from
            // fields here, so a parameter dbdelve does not carry has nowhere to
            // go, and silently ignoring one is how a connection ends up not
            // being the connection that was asked for.
            other => {
                return Err(format!(
                    "Connection URL parameter {other} is not one dbdelve can pass to MySQL."
                ));
            }
        }
    }

    Ok(ServerConfig {
        host,
        port: parsed.port(),
        database: percent_decoded(&database)?,
        user,
        password: parsed
            .password()
            .map(percent_decoded)
            .transpose()?
            .unwrap_or_default(),
        sslmode,
        root_certificate,
        // A URL has nowhere to say it; the form is where it is set.
        statement_timeout: 0,
    })
}

/// The fields every connection needs, query or cancel alike.
fn base_options(server: &ServerConfig) -> OptsBuilder {
    OptsBuilder::new()
        .ip_or_hostname(Some(server.host.clone()))
        .tcp_port(server.port.unwrap_or(DEFAULT_PORT))
        .db_name(Some(server.database.clone()))
        .user(Some(server.user.clone()))
        // Offered only when there is one. An empty password is not the
        // same as no password, and IAM auth relies on the latter.
        .pass((!server.password.is_empty()).then(|| server.password.clone()))
        .tcp_connect_timeout(Some(Duration::from_secs(CONNECT_TIMEOUT_SECONDS)))
}

/// Open a connection built from `options`, falling back to plaintext exactly
/// where `sslmode=prefer` allows it. Shared by the query connection and the
/// cancel connection so the fallback behaviour cannot drift between them.
fn open(server: &ServerConfig, options: impl Fn() -> OptsBuilder) -> Result<Conn, DbError> {
    match Conn::new(options().ssl_opts(ssl_options(server))) {
        Ok(connection) => Ok(connection),
        // `prefer` is the one rung where a weaker connection is reachable, and
        // reaching it is what the word means. The driver cannot say "encrypt if
        // you can" in one attempt, so this is two — which is what libpq's own
        // `prefer` amounts to. No other mode falls back.
        Err(_) if server.sslmode == SslMode::Prefer => {
            Conn::new(options().ssl_opts(None)).map_err(|error| connect_error(&error, server))
        }
        Err(error) => Err(connect_error(&error, server)),
    }
}

/// The connection a profile keeps.
fn connect(server: &ServerConfig) -> Result<Conn, DbError> {
    open(server, || match server.statement_timeout {
        0 => base_options(server),
        // Run once at connect as a session default, never spliced into the
        // user's own submission — see `ServerConfig::statement_timeout` for
        // what this does and does not bound. `max_execution_time` counts
        // milliseconds, and a server too old to know the variable fails the
        // connect here rather than the statement later.
        seconds => base_options(server).init(vec![format!(
            "SET SESSION max_execution_time = {}",
            u64::from(seconds) * 1_000
        )]),
    })
}

/// The throwaway connection a cancel opens: the same fields, but bounded end
/// to end and with no init statement it has no use for.
fn cancel_options(server: &ServerConfig) -> OptsBuilder {
    let timeout = Some(Duration::from_secs(CANCEL_TIMEOUT_SECONDS));
    base_options(server)
        .read_timeout(timeout)
        .write_timeout(timeout)
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise. A
/// profile runs one query at a time by design; revisit only if concurrent
/// statements per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    connection: Arc<Mutex<Conn>>,
    /// Read in `open`, off the connection, before it goes behind the mutex —
    /// see [`super::Connection::cancel`]. The crate offers no cancel API at
    /// all, so stopping a statement means telling the server to over a second
    /// socket, and the server knows this session by its number.
    connection_id: u32,
    /// The credentials that second socket is opened with, resolved, which the
    /// config a profile holds above is not. Keeping the password here changes
    /// nothing materially — the driver is holding the same one in the live
    /// connection's own options, in this process — and it is kept here rather
    /// than asked for from above because nothing above `src/db/` may learn that
    /// MySQL is the engine needing a second socket (AGENTS.md, hard rule 4).
    server: ServerConfig,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        let connection = connect(server)?;

        Ok(Self {
            connection_id: connection.connection_id(),
            server: server.clone(),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// `KILL QUERY` over a connection of its own, because the connection being
    /// stopped is busy holding the mutex. It ends the statement and not the
    /// session, so the user's transaction and temporary tables survive it.
    ///
    /// Opening a whole connection to send one statement is what a driver with
    /// no cancel API costs; there is no cheaper channel to the server. This one
    /// carries its own read/write timeout, unlike the query connection: bounding
    /// it abandons nothing, since the statement it failed to kill runs on
    /// regardless of whether this socket is still open. Leaving it unbounded
    /// only trades that for a background thread hung forever with no report.
    pub fn cancel(&self) -> Result<(), DbError> {
        let server = &self.server;
        let mut connection = open(server, || cancel_options(server))?;
        connection
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .map_err(|error| DbError {
                message: format!("Could not ask the server to cancel: {error}"),
                position: None,
            })
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. Row
    /// limits belong to the caller that *generated* a query, never to one the
    /// user typed.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, true)
    }

    /// dbdelve's own SQL. Its rows are never editable, so it does not pay for the
    /// round trip that reads a primary key.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, false)
    }

    fn run(&self, sql: &str, editable: bool) -> Result<QueryResult, DbError> {
        let mut connection = self.connection.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;

        let mut result = QueryResult::default();
        let mut probed = Vec::new();

        let mut submit = || -> Result<(), DbError> {
            // Timed from here, not from the call: one connection serialises a
            // profile's queries, and time spent waiting behind the catalog load
            // is not time the server spent on this statement.
            let started = Instant::now();
            let mut sets = connection
                .query_iter(sql)
                .map_err(|error| query_error(&error))?;

            while let Some(mut set) = sets.iter() {
                // The description borrows the set and iterating it needs the set
                // mutably, so everything it says is taken first.
                let described: Vec<ProbedColumn> = set
                    .columns()
                    .as_ref()
                    .iter()
                    .map(ProbedColumn::describe)
                    .collect();
                let columns: Vec<Column> = described
                    .iter()
                    .map(|column| Column {
                        name: column.name.clone(),
                        data_type: Some(column.type_name.clone()),
                    })
                    .collect();

                // No columns is a command rather than a query, and only a
                // command has rows to have affected.
                if columns.is_empty() {
                    for row in &mut set {
                        row.map_err(|error| query_error(&error))?;
                    }
                    result.rows_affected = Some(set.affected_rows());
                    continue;
                }

                // One selection can carry several statements, and the grid shows
                // one result set, so each new description starts the kept set
                // over and the last statement wins.
                let mut rows = Vec::new();
                let mut bytes = 0;
                for row in &mut set {
                    let row = row.map_err(|error| query_error(&error))?;
                    let cells: Vec<Cell> = (0..columns.len())
                        .map(|index| {
                            let value = row.as_ref(index).unwrap_or(&Value::NULL);
                            render(value, described[index].binary, &columns, index)
                        })
                        .collect::<Result<_, _>>()?;

                    bytes += cells
                        .iter()
                        .filter_map(|cell| cell.as_ref().map(String::len))
                        .sum::<usize>();
                    rows.push(cells);
                }

                // A query's count is the rows it returned, which is what
                // Postgres reports for a select too. `affected_rows` is zero for
                // one, and zero would read as a result set that came back empty.
                result.rows_affected = Some(rows.len() as u64);
                result.columns = columns;
                result.rows = rows;
                result.bytes = bytes;
                probed = described;
            }
            result.elapsed = started.elapsed();
            Ok(())
        };

        if let Err(error) = submit() {
            return Err(rolled_back(&mut connection, sql, error));
        }

        // The connection mutex is not reentrant and `edit_target` runs a catalog
        // query through it, so the lock has to go before the question is asked.
        drop(connection);
        if editable {
            result.edit = self.edit_target(&probed);
        }

        Ok(result)
    }

    /// Which table these columns can be written back to, if any.
    ///
    /// Every step is allowed to answer "no": a join, a computed column, a table
    /// without a primary key, a key the select omitted. A failure answers "no"
    /// too — this runs after the user's statement already succeeded, and a
    /// catalog query that was refused must not turn that success into an error.
    fn edit_target(&self, probed: &[ProbedColumn]) -> Option<EditTarget> {
        let (schema, table) = sole_table(probed)?;
        // ponytail: one catalog round trip per result set, no cache. A map from
        // table to key held on the connection is the upgrade path if the trip
        // shows up in query timings.
        let key = self
            .internal_query(&structure_sql(PRIMARY_KEY_SQL, &schema, &table))
            .ok()?;
        let key = column_of(&key, "column_name").ok()?;

        resolve_edit_target(probed, &schema, &table, &key)
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let relations = self.internal_query(&RELATIONS_SQL.replace("{system}", SYSTEM_SCHEMAS))?;
        assemble_catalog(relations, QueryResult::default())
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        let routines = self.internal_query(&ROUTINES_SQL.replace("{system}", SYSTEM_SCHEMAS))?;
        assemble_catalog(QueryResult::default(), routines)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let columns =
            self.internal_query(&structure_sql(STRUCTURE_COLUMNS_SQL, schema, relation))?;
        let indexes =
            self.internal_query(&structure_sql(STRUCTURE_INDEXES_SQL, schema, relation))?;
        let constraints = self.internal_query(&structure_sql(
            &STRUCTURE_CONSTRAINTS_SQL.replace("{columns}", CONSTRAINT_COLUMNS_SQL),
            schema,
            relation,
        ))?;
        let keys =
            self.internal_query(&structure_sql(STRUCTURE_FOREIGN_KEYS_SQL, schema, relation))?;

        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&keys)?;
        Ok(structure)
    }
}

/// One column as the server describes it: what it is called, what it is, and
/// where it came from.
///
/// The provenance is plumbing between the row description and
/// [`resolve_edit_target`], and stops in this module.
struct ProbedColumn {
    name: String,
    type_name: String,
    /// Absent for a computed column, which belongs to no table.
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    /// Whether the value is bytes rather than text.
    binary: bool,
}

impl ProbedColumn {
    fn describe(column: &::mysql::Column) -> Self {
        let named = |value: std::borrow::Cow<'_, str>| {
            Some(value.into_owned()).filter(|value| !value.is_empty())
        };

        Self {
            name: column.name_str().into_owned(),
            type_name: type_name(column),
            schema: named(column.schema_str()),
            table: named(column.org_table_str()),
            column: named(column.org_name_str()),
            binary: is_binary(column),
        }
    }
}

/// The server's own name for a column's type, as `SHOW CREATE TABLE` would
/// spell it.
///
/// The wire protocol carries a type code and a flag word rather than a name, and
/// the code alone cannot tell `text` from `blob` or `varchar` from `varbinary` —
/// those differ only by character set. So the name is rebuilt from all three.
fn type_name(column: &::mysql::Column) -> String {
    let binary = is_binary(column);
    let text_or_blob = |text: &str, blob: &str| match binary {
        true => blob.to_string(),
        false => text.to_string(),
    };

    let name = match column.column_type() {
        ColumnType::MYSQL_TYPE_TINY => "tinyint".into(),
        ColumnType::MYSQL_TYPE_SHORT => "smallint".into(),
        ColumnType::MYSQL_TYPE_INT24 => "mediumint".into(),
        ColumnType::MYSQL_TYPE_LONG => "int".into(),
        ColumnType::MYSQL_TYPE_LONGLONG => "bigint".into(),
        ColumnType::MYSQL_TYPE_FLOAT => "float".into(),
        ColumnType::MYSQL_TYPE_DOUBLE => "double".into(),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => "decimal".into(),
        ColumnType::MYSQL_TYPE_BIT => "bit".into(),
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date".into(),
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "time".into(),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "datetime".into(),
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "timestamp".into(),
        ColumnType::MYSQL_TYPE_YEAR => "year".into(),
        ColumnType::MYSQL_TYPE_JSON => "json".into(),
        ColumnType::MYSQL_TYPE_ENUM => "enum".into(),
        ColumnType::MYSQL_TYPE_SET => "set".into(),
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry".into(),
        ColumnType::MYSQL_TYPE_VECTOR => "vector".into(),
        ColumnType::MYSQL_TYPE_NULL => "null".into(),
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
            text_or_blob("varchar", "varbinary")
        }
        ColumnType::MYSQL_TYPE_STRING => text_or_blob("char", "binary"),
        ColumnType::MYSQL_TYPE_TINY_BLOB => text_or_blob("tinytext", "tinyblob"),
        ColumnType::MYSQL_TYPE_BLOB => text_or_blob("text", "blob"),
        ColumnType::MYSQL_TYPE_MEDIUM_BLOB => text_or_blob("mediumtext", "mediumblob"),
        ColumnType::MYSQL_TYPE_LONG_BLOB => text_or_blob("longtext", "longblob"),
        // Absent rather than invented. A code this build does not know is not a
        // type it can name, and a wrong name is worse than none.
        other => return format!("{other:?}").to_lowercase(),
    };

    match column.flags().contains(ColumnFlags::UNSIGNED_FLAG) && !binary {
        true => format!("{name} unsigned"),
        false => name,
    }
}

/// Whether a column's values are bytes rather than text.
///
/// The binary collation is not the whole test: in the text protocol every
/// number reports it too, and a number is not a blob. Only the string and blob
/// families can be either, so only they are asked.
fn is_binary(column: &::mysql::Column) -> bool {
    column.character_set() == BINARY_COLLATION
        && matches!(
            column.column_type(),
            ColumnType::MYSQL_TYPE_VARCHAR
                | ColumnType::MYSQL_TYPE_VAR_STRING
                | ColumnType::MYSQL_TYPE_STRING
                | ColumnType::MYSQL_TYPE_TINY_BLOB
                | ColumnType::MYSQL_TYPE_BLOB
                | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                | ColumnType::MYSQL_TYPE_LONG_BLOB
                | ColumnType::MYSQL_TYPE_BIT
                | ColumnType::MYSQL_TYPE_GEOMETRY
                | ColumnType::MYSQL_TYPE_VECTOR
        )
}

/// A value as text.
///
/// The text protocol has already done almost all of this: every non-null value
/// arrives as the bytes the server formatted, so the only decision left is
/// whether those bytes are text.
fn render(value: &Value, binary: bool, columns: &[Column], index: usize) -> Result<Cell, DbError> {
    Ok(match value {
        Value::NULL => None,
        // MySQL's own literal syntax for a blob, rather than bare hex, so a
        // value copied out of the grid is a value that can be pasted into a
        // statement.
        Value::Bytes(bytes) if binary => Some(format!("0x{}", hex::encode_upper(bytes))),
        Value::Bytes(bytes) => Some(
            // A column whose declared character set does not match what was
            // stored can return bytes that are not text at all. Postgres raises
            // the same error for the same reason, naming the column.
            std::str::from_utf8(bytes)
                .map_err(|_| non_utf8_error(columns, index))?
                .to_string(),
        ),
        // Unreachable through the text protocol, which sends everything as
        // bytes. `Value` is shared with the binary protocol, and a driver that
        // surprised us here should still render something true.
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    })
}

/// The one table every column that came from a table came from.
///
/// A result set spanning two of them is a join, and a row of it is not a row of
/// either table, so there is nothing to write back to. A result set spanning
/// none is entirely computed. Two tables of the same name in different schemas
/// are two tables, which is why the schema is half of the answer.
fn sole_table(probed: &[ProbedColumn]) -> Option<(String, String)> {
    let mut named = probed
        .iter()
        .filter_map(|column| Some((column.schema.clone()?, column.table.clone()?)));
    let first = named.next()?;
    named.all(|table| table == first).then_some(first)
}

/// The description decided against the table's key.
///
/// Refuses unless *every* primary key column is present in the result set: a
/// partial key matches more rows than the one the user is looking at, and an
/// empty one matches all of them.
fn resolve_edit_target(
    probed: &[ProbedColumn],
    schema: &str,
    table: &str,
    key: &[String],
) -> Option<EditTarget> {
    // The caller has already established that the columns carrying a table all
    // carry the same one, so carrying a table at all means carrying this one.
    let column_of = |column: &ProbedColumn| column.table.as_ref().and(column.column.clone());
    // Two result columns reading the same table column are the two sides of a
    // self-join, and no driver reports the alias that tells them apart. The key
    // below is located by position, so it would resolve to whichever side came
    // first and write the edit at the other row's key; the same shape also
    // generates one SET clause per side for the same column. Refusing the whole
    // result set is one answer to both.
    let mut origins = HashSet::new();
    if !probed
        .iter()
        .filter_map(column_of)
        .all(|column| origins.insert(column))
    {
        return None;
    }
    let keys = key
        .iter()
        .map(|name| {
            probed
                .iter()
                .position(|column| column_of(column).as_deref() == Some(name.as_str()))
        })
        .collect::<Option<Vec<usize>>>()?;
    if keys.is_empty() {
        return None;
    }

    Some(EditTarget {
        schema: schema.to_string(),
        table: table.to_string(),
        columns: probed.iter().map(column_of).collect(),
        keys,
    })
}

fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    template
        .replace("{schema}", &Engine::MySql.quote_literal(schema))
        .replace("{relation}", &Engine::MySql.quote_literal(relation))
}

/// Every row's value for one column, for the catalog queries that return a
/// single column and would otherwise each grow their own loop.
fn column_of(result: &QueryResult, column: &str) -> Result<Vec<String>, DbError> {
    result
        .rows
        .iter()
        .map(|row| required_cell(result, row, column).map(str::to_string))
        .collect()
}

/// dbdelve's five rungs as the driver's options.
///
/// The driver verifies certificates itself, so `tls::connector`'s verifiers are
/// not reusable here — see the multi-engine spec, section 4.3, for the trust
/// store that entails and why the divergence is acceptable. What matters for
/// hard rule 7 is that nothing below quietly accepts less than it was asked for:
/// each rung either gets its checks or fails saying so.
fn ssl_options(server: &ServerConfig) -> Option<SslOpts> {
    let with_root = || {
        SslOpts::default().with_root_cert_path(
            server
                .root_certificate
                .as_ref()
                .map(|path| PathBuf::from(path.as_str())),
        )
    };

    match server.sslmode {
        SslMode::Disable => None,
        // Encrypted, but checking nothing about who answered — which is exactly
        // what these two rungs promise and no more.
        SslMode::Prefer | SslMode::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        SslMode::VerifyCa => Some(with_root().with_danger_skip_domain_validation(true)),
        SslMode::VerifyFull => Some(with_root()),
    }
}

fn connect_error(error: &::mysql::Error, server: &ServerConfig) -> DbError {
    // A refused connection is the most common failure by a wide margin, and the
    // driver's own wording buries the endpoint. Say what happened, and nothing
    // about what the user should do -- we cannot see their machine.
    if let ::mysql::Error::IoError(io) = error
        && io.kind() == std::io::ErrorKind::ConnectionRefused
    {
        return plain_error(format!(
            "Connection refused: nothing is listening on {}",
            server.endpoint()
        ));
    }

    plain_error(describe(error))
}

fn query_error(error: &::mysql::Error) -> DbError {
    DbError {
        message: describe(error),
        // MySQL reports no offset into the statement, so there is nothing to
        // point the editor at. Absent rather than guessed.
        position: None,
    }
}

/// End the transaction a failed batch left open, and say so in the error.
///
/// MySQL stops a multi-statement submission at the failing statement, so the
/// `COMMIT` dbdelve wrote into the text never runs and the transaction stays open
/// on a connection that outlives it — the refresh that follows would read the
/// uncommitted rows back as though the apply had succeeded, and every statement
/// after it would join a transaction nobody closes.
///
/// Only a transaction *this* submission opened is rolled back, so one the user
/// began in an earlier run is theirs to finish. Unlike SQLite, MySQL cannot be
/// asked: the driver keeps the server's `SERVER_STATUS_IN_TRANS` flag private,
/// so the submitted text is what there is to read, and it is the text dbdelve
/// wrote or the user can see.
fn rolled_back(connection: &mut Conn, sql: &str, error: DbError) -> DbError {
    let Some(start) = Engine::MySql.transaction_start() else {
        return error;
    };
    if !sql
        .trim_start()
        .get(..start.len())
        .is_some_and(|word| word.eq_ignore_ascii_case(start))
    {
        return error;
    }

    let outcome = match connection.query_drop("ROLLBACK") {
        Ok(()) => "The transaction the batch opened was rolled back; nothing it wrote remains."
            .to_string(),
        Err(failure) => format!(
            "The transaction the batch opened is still open: the rollback failed too. {}",
            describe(&failure)
        ),
    };
    DbError {
        message: format!("{}\n\n{outcome}", error.message),
        position: error.position,
    }
}

/// Prefer the server's own message. It is written for humans and already says
/// the useful part; the driver's wrapper adds an error code and a SQLSTATE that
/// mostly repeat it.
fn describe(error: &::mysql::Error) -> String {
    match error {
        ::mysql::Error::MySqlError(server) => server.message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, ForeignKey, RelationKind, RoutineKind};

    /// The server the `live_` tests talk to, from `dbdelve_MYSQL_URL`.
    fn live() -> Connection {
        let url = std::env::var("dbdelve_MYSQL_URL").expect("dbdelve_MYSQL_URL is required");
        let config = config_from_url(&url).expect("dbdelve_MYSQL_URL should parse");
        Connection::open(&ServerConfig {
            // The compose database speaks no TLS, and these tests are the one
            // place a plaintext connection is the point.
            sslmode: SslMode::Disable,
            ..config
        })
        .expect("connection should open")
    }

    fn names(result: &QueryResult) -> Vec<&str> {
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect()
    }

    fn types(result: &QueryResult) -> Vec<Option<&str>> {
        result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref())
            .collect()
    }

    fn probed(columns: &[(Option<&str>, Option<&str>)]) -> Vec<ProbedColumn> {
        columns
            .iter()
            .map(|(table, column)| ProbedColumn {
                name: column.unwrap_or("?").to_string(),
                type_name: "int".to_string(),
                schema: table.map(|_| "dbdelve_dev".to_string()),
                table: table.map(str::to_string),
                column: column.map(str::to_string),
                binary: false,
            })
            .collect()
    }

    #[test]
    fn a_url_fills_the_fields_without_inventing_a_port() {
        let config =
            config_from_url("mysql://person%40example.com:pa%20ss@db.example.test/dbdelve_test")
                .unwrap();

        assert_eq!(
            config,
            ServerConfig {
                host: "db.example.test".into(),
                port: None,
                database: "dbdelve_test".into(),
                // Cloud IAM usernames are email addresses, and a password may
                // contain anything a URL can encode.
                user: "person@example.com".into(),
                password: "pa ss".into(),
                sslmode: SslMode::default(),
                root_certificate: None,
                statement_timeout: 0,
            }
        );
        assert_eq!(
            config_from_url("mysql://someone@db.example.test:3307/dbdelve_test")
                .unwrap()
                .port,
            Some(3307)
        );
    }

    #[test]
    fn a_url_carries_the_two_keys_dbdelve_owns() {
        let config = config_from_url(
            "mysql://someone@db.example.test/dbdelve_test\
             ?sslmode=verify-full&sslrootcert=/tmp/rds.pem",
        )
        .unwrap();

        assert_eq!(config.sslmode, SslMode::VerifyFull);
        assert_eq!(config.root_certificate.as_deref(), Some("/tmp/rds.pem"));
    }

    #[test]
    fn a_url_missing_a_part_or_carrying_an_unknown_one_says_which() {
        for (url, expected) in [
            ("mysql://db.example.test/dbdelve_test", "username"),
            ("mysql://someone@db.example.test/", "database"),
            (
                "mysql://someone@db.example.test/dbdelve_test?charset=utf8",
                "charset",
            ),
            (
                "mysql://someone@db.example.test/dbdelve_test?sslmode=allow",
                "allow",
            ),
        ] {
            let error = config_from_url(url).unwrap_err();
            assert!(error.contains(expected), "{url} said: {error}");
        }
    }

    #[test]
    fn every_rung_gets_the_checks_it_asked_for() {
        // Hard rule 7 in code. `disable` builds nothing, the two middle rungs
        // encrypt without checking who answered, and the two verifying rungs
        // switch the checks back on -- `verify-ca` keeping only the hostname
        // check off, which is the whole difference between it and `verify-full`.
        let server = |sslmode| ServerConfig {
            sslmode,
            root_certificate: Some("/tmp/rds.pem".into()),
            ..ServerConfig::default()
        };

        assert!(ssl_options(&server(SslMode::Disable)).is_none());
        for mode in [SslMode::Prefer, SslMode::Require] {
            let options = ssl_options(&server(mode)).expect("a connector is built");
            assert!(options.accept_invalid_certs(), "{mode:?}");
            assert!(options.skip_domain_validation(), "{mode:?}");
        }

        let verify_ca = ssl_options(&server(SslMode::VerifyCa)).expect("a connector is built");
        assert!(!verify_ca.accept_invalid_certs());
        assert!(verify_ca.skip_domain_validation());

        let verify_full = ssl_options(&server(SslMode::VerifyFull)).expect("a connector is built");
        assert!(!verify_full.accept_invalid_certs());
        assert!(!verify_full.skip_domain_validation());
    }

    #[test]
    fn sole_table_needs_one_table_and_at_least_one() {
        assert_eq!(
            sole_table(&probed(&[(Some("accounts"), Some("id")), (None, None)])),
            Some(("dbdelve_dev".into(), "accounts".into()))
        );
        assert_eq!(
            sole_table(&probed(&[
                (Some("accounts"), Some("id")),
                (Some("locations"), Some("id")),
            ])),
            None
        );
        assert_eq!(sole_table(&probed(&[(None, None)])), None);
        assert_eq!(sole_table(&[]), None);
    }

    #[test]
    fn an_edit_target_names_real_columns_and_locates_the_whole_key() {
        // The grid holds the aliases the user typed; an UPDATE has to name the
        // columns the table actually has. A computed column has none.
        let target = resolve_edit_target(
            &probed(&[
                (Some("accounts"), Some("id")),
                (None, None),
                (Some("accounts"), Some("name")),
            ]),
            "dbdelve_dev",
            "accounts",
            &["id".to_string()],
        )
        .expect("one table with its key present is editable");

        assert_eq!(target.schema, "dbdelve_dev");
        assert_eq!(
            target.columns,
            vec![Some("id".to_string()), None, Some("name".to_string())]
        );
        assert_eq!(target.keys, vec![0]);
    }

    #[test]
    fn a_self_join_is_not_editable() {
        // `org_table` is the same on both sides and the alias is not reported,
        // so `sole_table` sees one table and the key resolves to the first
        // side -- an edit typed on b.name would be written at a's id.
        assert_eq!(
            resolve_edit_target(
                &probed(&[
                    (Some("accounts"), Some("id")),
                    (Some("accounts"), Some("name")),
                    (Some("accounts"), Some("name")),
                ]),
                "dbdelve_dev",
                "accounts",
                &["id".to_string()],
            ),
            None
        );
    }

    #[test]
    fn a_result_set_missing_part_of_the_key_is_not_editable() {
        // Half a composite key matches more rows than the one on screen, and no
        // key at all matches every row in the table.
        assert_eq!(
            resolve_edit_target(
                &probed(&[(Some("t"), Some("left_id")), (Some("t"), Some("label"))]),
                "dbdelve_dev",
                "t",
                &["left_id".to_string(), "right_id".to_string()],
            ),
            None
        );
        assert_eq!(
            resolve_edit_target(
                &probed(&[(Some("t"), Some("label"))]),
                "dbdelve_dev",
                "t",
                &[],
            ),
            None
        );
    }

    #[test]
    fn the_cancel_connection_is_bounded_and_carries_no_init_statement() {
        // The bug this guards: a cancel that reuses the query connection's
        // options has no read/write timeout and runs an init statement it has
        // no use for, so it can hang a thread forever instead of failing fast.
        let server = ServerConfig {
            statement_timeout: 5,
            ..ServerConfig::default()
        };
        let opts: ::mysql::Opts = cancel_options(&server).into();

        let timeout = Some(Duration::from_secs(CANCEL_TIMEOUT_SECONDS));
        assert_eq!(opts.get_read_timeout(), timeout.as_ref());
        assert_eq!(opts.get_write_timeout(), timeout.as_ref());
        assert!(opts.get_init().is_empty());
    }

    #[test]
    fn structure_sql_quotes_a_name_containing_a_quote() {
        assert_eq!(
            structure_sql(
                "WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {relation}",
                "dbdelve_dev",
                "odd'name",
            ),
            "WHERE TABLE_SCHEMA = 'dbdelve_dev' AND TABLE_NAME = 'odd''name'"
        );
    }

    /// A statement the server has to work through, rather than one it waits
    /// out.
    ///
    /// `SELECT SLEEP(n)` is the obvious probe and it is the wrong one:
    /// `SLEEP()` is documented to return **1 when it is interrupted** rather
    /// than raising, so a cancelled or timed-out sleep comes back as an
    /// ordinary one-row result. Both tests below asserted on an error and
    /// failed against MySQL 8.4 for that reason alone -- the interrupt was
    /// landing on time, and the probe was swallowing it.
    ///
    /// 25 million rows of `SHA2` is real work on real table data, so the server
    /// is genuinely mid-statement when the interrupt arrives and answers the
    /// way anything else would. A cartesian `COUNT(*)` is not a substitute:
    /// the optimizer answers that one from statistics without reading a row.
    const LIVE_SLOW_SELECT: &str =
        "SELECT MAX(SHA2(CONCAT(a.id, b.id), 512)) FROM measurements a JOIN measurements b";

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_cancel_stops_a_running_statement_without_closing_the_session() {
        // The connection id has to have been read in `open`: asking the live
        // connection for it here would want the mutex the sleeping statement is
        // holding, and this would hang rather than fail.
        let connection = live();
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the KILL QUERY should send");
        });

        let error = connection
            .query(LIVE_SLOW_SELECT)
            .expect_err("the statement should be cancelled");
        assert!(
            error.message.contains("interrupt"),
            "the server's own words: {}",
            error.message
        );
        // `KILL QUERY` ends the statement, not the session.
        assert!(connection.query("SELECT 1").is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_statement_timeout_bounds_a_select_and_nothing_else() {
        let url = std::env::var("dbdelve_MYSQL_URL").expect("dbdelve_MYSQL_URL is required");
        let config = config_from_url(&url).expect("dbdelve_MYSQL_URL should parse");
        let connection = Connection::open(&ServerConfig {
            sslmode: SslMode::Disable,
            statement_timeout: 1,
            ..config
        })
        .expect("connection should open");

        let error = connection
            .query(LIVE_SLOW_SELECT)
            .expect_err("a read-only SELECT should time out");
        assert!(error.message.contains("exceeded"), "{}", error.message);

        // The asymmetry `ServerConfig::statement_timeout` names: the same wait
        // inside a statement that writes is not bounded at all, and Cancel is
        // the only thing that reaches it.
        assert!(
            connection
                .query("DO SLEEP(2)")
                .expect("a non-SELECT is not bounded")
                .rows
                .is_empty()
        );
    }

    /// What each mode does against the compose server, which speaks TLS with a
    /// certificate signed by nobody.
    ///
    /// Hard rule 7 in code, and the mode the connection form actually defaults
    /// to. `prefer` and `require` promise encryption and no more, so a
    /// certificate they cannot check is not their business. The two verifying
    /// rungs refuse it and say TLS was the reason, rather than quietly
    /// connecting anyway.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_only_the_modes_that_tolerate_an_unchecked_certificate_connect() {
        let url = std::env::var("dbdelve_MYSQL_URL").expect("dbdelve_MYSQL_URL is required");
        let base = config_from_url(&url).expect("dbdelve_MYSQL_URL should parse");
        let connect = |sslmode| {
            Connection::open(&ServerConfig {
                sslmode,
                ..base.clone()
            })
        };

        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require] {
            assert!(connect(mode).is_ok(), "{mode:?} should connect");
        }

        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let Err(error) = connect(mode) else {
                panic!("{mode:?} must not accept a certificate it cannot verify");
            };
            assert!(
                error.message.to_lowercase().contains("tls")
                    || error.message.to_lowercase().contains("certificate"),
                "{mode:?} failed without saying why: {}",
                error.message
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_the_development_database_is_fully_seeded() {
        // The seed is applied by a tool outside this test suite, and a seed that
        // half-applied still leaves something to connect to -- the MySQL
        // container reports itself healthy either way. So the volume table is
        // checked by count rather than assumed.
        let result = live()
            .query("SELECT count(*) AS rows_seeded FROM measurements")
            .expect("query should succeed");

        assert_eq!(result.rows[0][0].as_deref(), Some("5000"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_query_round_trip() {
        let result = live()
            .query("SELECT 1 AS id, 'alpha' AS label UNION ALL SELECT 2, NULL")
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "label"]);
        assert_eq!(
            result.rows,
            vec![
                vec![Some("1".into()), Some("alpha".into())],
                vec![Some("2".into()), None],
            ]
        );
        assert_eq!(result.rows_affected, Some(2));
        assert_eq!(result.bytes, 7);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_multi_statement_selection_keeps_one_result_shape() {
        let connection = live();

        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d")
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["d"]);
        assert_eq!(result.rows, vec![vec![Some("4".into())]]);
        assert!(
            result
                .rows
                .iter()
                .all(|row| row.len() == result.columns.len()),
            "every row must match the column count"
        );

        // A valid empty result still has to carry its headers.
        let empty = connection
            .query("SELECT 1 AS id, 'x' AS label FROM DUAL WHERE false")
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id", "label"]);
        assert!(empty.rows.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_column_is_tagged_with_the_type_the_server_would_name() {
        let result = live()
            .query("SELECT id, name, email FROM accounts")
            .expect("query should succeed");

        // `bigint` and not `int`: the wire protocol carries a type code rather
        // than a name, and reading it as the narrower type would be a quiet lie
        // about what the column holds.
        assert_eq!(
            types(&result),
            vec![Some("bigint"), Some("text"), Some("text")]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_catalog_round_trip() {
        let catalog = live().catalog().expect("catalog should load");
        let schema = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "dbdelve_dev")
            .expect("dbdelve_dev should exist");

        assert!(
            schema
                .relations
                .iter()
                .any(|relation| relation.name == "accounts" && relation.kind == RelationKind::Table)
        );
        assert!(
            schema
                .relations
                .iter()
                .any(|relation| relation.name == "account_overview"
                    && relation.kind == RelationKind::View)
        );
        assert!(schema.routines.iter().any(
            |routine| routine.name == "account_label" && routine.kind == RoutineKind::Function
        ));
        // The four schemas the server owns are not the user's.
        assert!(
            !catalog
                .schemas
                .iter()
                .any(|schema| schema.name == "mysql" || schema.name == "sys")
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_structure_round_trip() {
        let structure = live()
            .structure("dbdelve_dev", "accounts")
            .expect("structure should load");

        assert!(structure.columns.contains(&ColumnDefinition {
            name: "id".into(),
            data_type: "bigint".into(),
            nullable: false,
            // Not the empty default `COLUMN_DEFAULT` reports: the server
            // supplies this one, and saying otherwise would tell the user they
            // have to.
            default: Some("AUTO_INCREMENT".into()),
        }));
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "email" && column.nullable)
        );
        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition.starts_with("PRIMARY KEY"))
        );
        assert!(!structure.indexes.is_empty(), "the primary key is an index");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_single_table_select_is_editable_by_its_primary_key() {
        let edit = live()
            .query("SELECT name, id FROM accounts")
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");

        assert_eq!(edit.schema, "dbdelve_dev");
        assert_eq!(edit.table, "accounts");
        assert_eq!(
            edit.columns,
            vec![Some("name".to_string()), Some("id".to_string())]
        );
        assert_eq!(edit.keys, vec![1]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_an_aliased_or_computed_column_reports_what_the_table_calls_it() {
        let edit = live()
            .query("SELECT id AS ident, upper(name) AS shouted, name FROM accounts")
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");

        assert_eq!(
            edit.columns,
            vec![Some("id".to_string()), None, Some("name".to_string())]
        );
        assert_eq!(edit.keys, vec![0]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_join_an_aggregate_or_a_missing_key_is_not_editable() {
        let connection = live();

        for sql in [
            "SELECT accounts.id, locations.name
             FROM accounts JOIN locations ON locations.id = accounts.id",
            "SELECT plan, count(*) FROM accounts GROUP BY plan",
            "SELECT 1 AS one",
            // Nothing in this result set identifies which account a row is.
            "SELECT name, email FROM accounts",
        ] {
            assert!(
                connection
                    .query(sql)
                    .expect("query should succeed")
                    .edit
                    .is_none(),
                "{sql}"
            );
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_the_foreign_key_fixtures_are_seeded() {
        // The foreign-key tests have nothing to read unless the reseed that
        // added `orders`, `order_items` and the `dbdelve_archive` database has
        // actually been applied.
        let connection = live();

        let items = connection
            .query("SELECT count(*) AS rows_seeded FROM order_items")
            .expect("query should succeed");
        assert_eq!(items.rows[0][0].as_deref(), Some("3"));

        let closed = connection
            .query("SELECT count(*) AS rows_seeded FROM dbdelve_archive.closed_accounts")
            .expect("query should succeed");
        assert_eq!(closed.rows[0][0].as_deref(), Some("2"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_composite_foreign_key_arrives_as_one_key_per_column_in_key_order() {
        let structure = live()
            .structure("dbdelve_dev", "order_items")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![
                ForeignKey {
                    column: "order_account_id".into(),
                    referenced_schema: "dbdelve_dev".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "account_id".into(),
                },
                ForeignKey {
                    column: "order_number".into(),
                    referenced_schema: "dbdelve_dev".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "number".into(),
                },
            ]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_foreign_key_across_databases_names_the_database_it_references() {
        let structure = live()
            .structure("dbdelve_archive", "closed_accounts")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![ForeignKey {
                column: "account_id".into(),
                referenced_schema: "dbdelve_dev".into(),
                referenced_table: "accounts".into(),
                referenced_column: "id".into(),
            }]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_table_referencing_nothing_reports_no_foreign_keys() {
        let structure = live()
            .structure("dbdelve_dev", "accounts")
            .expect("structure should load");

        assert!(structure.foreign_keys.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_the_rendered_foreign_key_ddl_survives_beside_the_structured_form() {
        let structure = live()
            .structure("dbdelve_dev", "order_items")
            .expect("structure should load");

        assert!(
            structure.constraints.iter().any(|constraint| {
                constraint.definition
                    == "FOREIGN KEY (order_account_id, order_number) \
                        REFERENCES orders (account_id, number)"
            }),
            "{:?}",
            structure.constraints
        );
    }
}

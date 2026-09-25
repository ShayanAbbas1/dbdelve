//! The SQL Server boundary.
//!
//! tiberius is an async client with no runtime of its own, so each connection
//! owns a tokio current-thread runtime and blocks on it from the background
//! thread every call already runs on -- the arrangement the `postgres` crate
//! keeps privately around tokio-postgres, made visible here. Nothing tokio-
//! shaped leaves this module.
//!
//! The wire carries values in binary, so unlike the text protocols of the other
//! two servers every value is rendered here, in the server's own formats.
//!
//! Column provenance is learned the way Postgres learns it: by describing the
//! statement on a round trip of its own, after it ran.
//! `sys.dm_exec_describe_first_result_set` names the database, schema, table and
//! column behind each result column, and the primary key comes from the
//! catalog.

use std::collections::HashSet;
use std::net::Shutdown;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use tiberius::{
    AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem,
    time::{DateTime2, Time},
};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{
    Catalog, Cell, Column, DbError, EditTarget, Engine, QueryResult, ServerConfig, SslMode,
    Structure, assemble_catalog, assemble_foreign_keys, assemble_structure, plain_error,
    required_cell,
};

const RELATIONS_SQL: &str = "
SELECT
    s.name AS schema_name,
    o.name AS relation_name,
    CASE o.type WHEN 'U' THEN 'table' WHEN 'V' THEN 'view' END AS relation_kind
FROM sys.objects AS o
JOIN sys.schemas AS s ON s.schema_id = o.schema_id
WHERE o.type IN ('U', 'V')
    AND o.is_ms_shipped = 0
ORDER BY s.name, o.name
";

// `COALESCE` throughout because the assembler refuses a null, and these are
// null for reasons that are not errors: a procedure returns nothing, and
// `OBJECT_DEFINITION` is null for an encrypted module or a user without
// `VIEW DEFINITION`.
const ROUTINES_SQL: &str = "
SELECT
    s.name AS schema_name,
    o.name AS routine_name,
    CASE WHEN o.type IN ('P', 'PC') THEN 'procedure' ELSE 'function' END AS routine_kind,
    COALESCE((
        SELECT STRING_AGG(CAST(p.name COLLATE DATABASE_DEFAULT + N' ' + {type} AS nvarchar(max)), N', ')
            WITHIN GROUP (ORDER BY p.parameter_id)
        FROM sys.parameters AS p
        WHERE p.object_id = o.object_id AND p.parameter_id > 0
    ), N'') AS identity_arguments,
    CASE
        WHEN o.type IN ('IF', 'TF', 'FT') THEN N'TABLE'
        ELSE COALESCE((
            SELECT {type}
            FROM sys.parameters AS p
            WHERE p.object_id = o.object_id AND p.parameter_id = 0
        ), N'')
    END AS result_type,
    CASE WHEN o.type IN ('FS', 'FT', 'PC') THEN 'external' ELSE 'sql' END AS language,
    COALESCE(OBJECT_DEFINITION(o.object_id), N'') AS definition
FROM sys.objects AS o
JOIN sys.schemas AS s ON s.schema_id = o.schema_id
WHERE o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'P', 'PC')
    AND o.is_ms_shipped = 0
ORDER BY s.name, o.name
";

/// A column's or parameter's type as `CREATE TABLE` would spell it. The catalog
/// keeps the length in bytes, which is twice the declared length for the
/// UTF-16 types. An alias type is its own name and carries no length.
///
/// Every catalog string concatenated in these queries is `COLLATE
/// DATABASE_DEFAULT`: the catalog's collation is the server's, a database's
/// can differ, and concatenating across the two is an error rather than a
/// choice.
const TYPE_SQL: &str = "(TYPE_NAME(p.user_type_id) COLLATE DATABASE_DEFAULT + CASE
    WHEN p.user_type_id <> p.system_type_id THEN N''
    WHEN TYPE_NAME(p.system_type_id) IN ('varchar', 'char', 'varbinary', 'binary')
        THEN N'(' + CASE WHEN p.max_length = -1 THEN N'max'
            ELSE CAST(p.max_length AS nvarchar(10)) END + N')'
    WHEN TYPE_NAME(p.system_type_id) IN ('nvarchar', 'nchar')
        THEN N'(' + CASE WHEN p.max_length = -1 THEN N'max'
            ELSE CAST(p.max_length / 2 AS nvarchar(10)) END + N')'
    WHEN TYPE_NAME(p.system_type_id) IN ('decimal', 'numeric')
        THEN N'(' + CAST(p.precision AS nvarchar(10)) + N',' + CAST(p.scale AS nvarchar(10)) + N')'
    WHEN TYPE_NAME(p.system_type_id) IN ('datetime2', 'time', 'datetimeoffset')
        THEN N'(' + CAST(p.scale AS nvarchar(10)) + N')'
    ELSE N''
END)";

// The structure queries name one relation by `{object}`, an `OBJECT_ID` over
// the quoted name, substituted by `structure_sql`.
const STRUCTURE_COLUMNS_SQL: &str = "
SELECT
    p.name AS column_name,
    {type} AS data_type,
    CASE WHEN p.is_nullable = 1 THEN 'yes' ELSE 'no' END AS nullable,
    -- An identity or computed column has no default constraint, so reading
    -- only the default would report that the user must supply a value the
    -- server generates.
    CASE
        WHEN p.is_identity = 1 THEN CONCAT(
            N'IDENTITY(',
            CAST(identity_column.seed_value AS nvarchar(40)), N',',
            CAST(identity_column.increment_value AS nvarchar(40)), N')'
        )
        WHEN p.is_computed = 1 THEN CONCAT(N'AS ', computed.definition COLLATE DATABASE_DEFAULT)
        ELSE COALESCE(default_constraint.definition COLLATE DATABASE_DEFAULT, N'')
    END AS column_default
FROM sys.columns AS p
LEFT JOIN sys.identity_columns AS identity_column
    ON identity_column.object_id = p.object_id AND identity_column.column_id = p.column_id
LEFT JOIN sys.computed_columns AS computed
    ON computed.object_id = p.object_id AND computed.column_id = p.column_id
LEFT JOIN sys.default_constraints AS default_constraint
    ON default_constraint.object_id = p.default_object_id
WHERE p.object_id = {object}
ORDER BY p.column_id
";

const STRUCTURE_INDEXES_SQL: &str = "
SELECT
    i.name AS object_name,
    CONCAT(
        CASE WHEN i.is_unique = 1 THEN N'UNIQUE ' ELSE N'' END,
        i.type_desc COLLATE DATABASE_DEFAULT, N' INDEX (',
        STRING_AGG(
            CAST(c.name COLLATE DATABASE_DEFAULT + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END
                AS nvarchar(max)),
            N', '
        ) WITHIN GROUP (ORDER BY ic.key_ordinal),
        N')'
    ) AS definition
FROM sys.indexes AS i
JOIN sys.index_columns AS ic
    ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0
JOIN sys.columns AS c
    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
WHERE i.object_id = {object}
GROUP BY i.name, i.is_unique, i.type_desc
ORDER BY i.name
";

const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT
    k.name AS object_name,
    CONCAT(
        CASE k.type WHEN 'PK' THEN N'PRIMARY KEY' ELSE N'UNIQUE' END, N' (',
        (
            SELECT STRING_AGG(CAST(c.name COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
                WITHIN GROUP (ORDER BY ic.key_ordinal)
            FROM sys.index_columns AS ic
            JOIN sys.columns AS c
                ON c.object_id = ic.object_id AND c.column_id = ic.column_id
            WHERE ic.object_id = k.parent_object_id
                AND ic.index_id = k.unique_index_id
                AND ic.is_included_column = 0
        ),
        N')'
    ) AS definition
FROM sys.key_constraints AS k
WHERE k.parent_object_id = {object}
UNION ALL
SELECT
    f.name,
    CONCAT(
        N'FOREIGN KEY (',
        (
            SELECT STRING_AGG(CAST(c.name COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
                WITHIN GROUP (ORDER BY fc.constraint_column_id)
            FROM sys.foreign_key_columns AS fc
            JOIN sys.columns AS c
                ON c.object_id = fc.parent_object_id AND c.column_id = fc.parent_column_id
            WHERE fc.constraint_object_id = f.object_id
        ),
        N') REFERENCES ', OBJECT_SCHEMA_NAME(f.referenced_object_id) COLLATE DATABASE_DEFAULT, N'.',
        OBJECT_NAME(f.referenced_object_id) COLLATE DATABASE_DEFAULT, N' (',
        (
            SELECT STRING_AGG(CAST(c.name COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
                WITHIN GROUP (ORDER BY fc.constraint_column_id)
            FROM sys.foreign_key_columns AS fc
            JOIN sys.columns AS c
                ON c.object_id = fc.referenced_object_id AND c.column_id = fc.referenced_column_id
            WHERE fc.constraint_object_id = f.object_id
        ),
        N')'
    )
FROM sys.foreign_keys AS f
WHERE f.parent_object_id = {object}
UNION ALL
SELECT c.name, CONCAT(N'CHECK ', c.definition COLLATE DATABASE_DEFAULT)
FROM sys.check_constraints AS c
WHERE c.parent_object_id = {object}
ORDER BY object_name
";

// The structured half of a foreign key, beside the rendered DDL above.
// `constraint_column_id` is what keeps a composite key's columns in key order.
const STRUCTURE_FOREIGN_KEYS_SQL: &str = "
SELECT
    source_column.name AS column_name,
    OBJECT_SCHEMA_NAME(fc.referenced_object_id) AS referenced_schema,
    OBJECT_NAME(fc.referenced_object_id) AS referenced_table,
    referenced_column.name AS referenced_column
FROM sys.foreign_key_columns AS fc
JOIN sys.foreign_keys AS f ON f.object_id = fc.constraint_object_id
JOIN sys.columns AS source_column
    ON source_column.object_id = fc.parent_object_id
    AND source_column.column_id = fc.parent_column_id
JOIN sys.columns AS referenced_column
    ON referenced_column.object_id = fc.referenced_object_id
    AND referenced_column.column_id = fc.referenced_column_id
WHERE fc.parent_object_id = {object}
ORDER BY f.name, fc.constraint_column_id
";

// An inner join on the primary index is what makes a table without one return
// nothing, which is the same answer as a table DBDelve cannot identify rows in.
const PRIMARY_KEY_SQL: &str = "
SELECT c.name AS column_name
FROM sys.indexes AS i
JOIN sys.index_columns AS ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
WHERE i.object_id = {object} AND i.is_primary_key = 1
ORDER BY ic.key_ordinal
";

// Mode 2 describes a view as the view, as a cursor would, rather than seeing
// through it to its base tables: a row read through a view is edited through
// the view's own key, which it has none of. `is_hidden` columns are keys the
// describe adds for a cursor and the statement never returned.
const DESCRIBE_SQL: &str = "
SELECT
    name AS column_name,
    source_server,
    source_database,
    source_schema,
    source_table,
    source_column,
    DB_NAME() AS current_database
FROM sys.dm_exec_describe_first_result_set({statement}, NULL, 2)
WHERE is_hidden = 0 AND error_number IS NULL
ORDER BY column_ordinal
";

/// Without this a host that resolves but drops packets pins the UI in
/// "Connecting…" for the OS SYN retry budget. It bounds the login too, which
/// the TCP connect alone would not.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// A `mssql://` or `sqlserver://` URL, read by dbdelve. tiberius parses only
/// ADO.NET and JDBC strings, neither of which is a URL.
pub fn config_from_url(url: &str) -> Result<ServerConfig, String> {
    super::server_from_url(url, "SQL Server")
}

type Tds = Client<Compat<TcpStream>>;

/// dbdelve's five rungs as tiberius's two settings.
///
/// tiberius builds its own rustls configuration and offers three trusts:
/// none at all, the platform store, or one named certificate. Neither of the
/// last two can skip the hostname alone, so `verify-ca` checks the name too
/// here -- stricter than asked, never weaker.
///
/// `Off` is not "no TLS": it encrypts the login packet, then drops to plaintext
/// for everything after. It is what `disable` gets, and never a fallback. Every
/// rung that promises encryption asks for `Required`, which fails rather than
/// negotiate down. (`On` is never sent: tiberius panics when a server answers
/// it with less.)
fn config(server: &ServerConfig, encryption: EncryptionLevel) -> Config {
    let mut config = Config::new();
    config.host(&server.host);
    if let Some(port) = server.port {
        config.port(port);
    }
    config.database(&server.database);
    config.application_name("DBDelve");
    // Sent as typed, blank included: cloud IAM issues a token or nothing.
    config.authentication(AuthMethod::sql_server(&server.user, &server.password));
    config.encryption(encryption);
    match (server.sslmode, &server.root_certificate) {
        (SslMode::VerifyCa | SslMode::VerifyFull, Some(path)) => config.trust_cert_ca(path),
        (SslMode::VerifyCa | SslMode::VerifyFull, None) => {}
        (SslMode::Disable | SslMode::Prefer | SslMode::Require, _) => config.trust_cert(),
    }
    config
}

fn encryption(mode: SslMode) -> EncryptionLevel {
    match mode {
        SslMode::Disable => EncryptionLevel::Off,
        SslMode::Prefer | SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            EncryptionLevel::Required
        }
    }
}

/// A runtime, and the client it drives.
struct Session {
    runtime: Runtime,
    client: Tds,
}

/// What Cancel reaches without the session mutex, which the running statement
/// holds.
#[derive(Default)]
struct InFlight {
    /// A second handle on the live socket. Shutting it down is what reaches
    /// the statement: tiberius has no way to send the protocol's attention
    /// signal, and `KILL` needs `ALTER ANY CONNECTION`, which an ordinary login
    /// does not have.
    socket: Option<std::net::TcpStream>,
    running: bool,
    stopped: bool,
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise. A
/// profile runs one query at a time by design; revisit only if concurrent
/// statements per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    /// `None` once a stopped statement has taken the session with it and
    /// reconnecting failed; the next run tries again.
    session: Arc<Mutex<Option<Session>>>,
    in_flight: Arc<Mutex<InFlight>>,
    /// Kept to reconnect after a stop, for the reason mysql.rs keeps its
    /// credentials: nothing above `src/db/` may learn that this engine needs
    /// them again.
    server: ServerConfig,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        let connection = Self {
            session: Arc::new(Mutex::new(None)),
            in_flight: Arc::new(Mutex::new(InFlight::default())),
            server: server.clone(),
        };
        let session = connection.connect()?;
        *connection.session.lock().expect("unshared until returned") = Some(session);
        Ok(connection)
    }

    /// `prefer` and `disable` are the rungs that may reach less than their
    /// first attempt, and reaching it is what the words mean: a second attempt
    /// that negotiates nothing, for a server that cannot encrypt at all.
    fn connect(&self) -> Result<Session, DbError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| plain_error(format!("Could not start the connection: {error}")))?;

        let (client, socket) = {
            let attempt = |encryption| {
                guarded(|| runtime.block_on(login(&self.server, encryption)))
                    .and_then(|result| result.map_err(|error| connect_error(&error, &self.server)))
            };
            match attempt(encryption(self.server.sslmode)) {
                Err(_) if matches!(self.server.sslmode, SslMode::Prefer | SslMode::Disable) => {
                    attempt(EncryptionLevel::NotSupported)?
                }
                other => other?,
            }
        };

        self.in_flight().socket = Some(socket);
        Ok(Session { runtime, client })
    }

    fn in_flight(&self) -> std::sync::MutexGuard<'_, InFlight> {
        // Nothing panics while holding it, and it holds plain flags.
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stops the running statement by closing its connection, which is the only
    /// channel to the server the driver leaves open. The server abandons a batch
    /// whose client has gone, rolling back what it had open -- so, unlike
    /// Postgres and MySQL, the session does not survive, and the run it stopped
    /// says so and reconnects.
    pub fn cancel(&self) -> Result<(), DbError> {
        let mut in_flight = self.in_flight();
        if !in_flight.running {
            return Ok(());
        }
        in_flight.stopped = true;
        if let Some(socket) = &in_flight.socket {
            socket
                .shutdown(Shutdown::Both)
                .map_err(|error| plain_error(format!("Could not cancel: {error}")))?;
        }
        Ok(())
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. Row
    /// limits belong to the caller that *generated* a query, never to one the
    /// user typed.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, true)
    }

    /// dbdelve's own SQL. Its rows are never editable, so it does not pay for
    /// the describe that would say where they came from.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, false)
    }

    fn run(&self, sql: &str, editable: bool) -> Result<QueryResult, DbError> {
        let mut guard = self.session.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }
        let session = guard.as_mut().expect("connected above");

        {
            let mut in_flight = self.in_flight();
            in_flight.running = true;
            in_flight.stopped = false;
        }
        let limit = self.server.statement_timeout;
        // Timed from here, not from the call: one connection serialises a
        // profile's queries, and time spent waiting behind the catalog load is
        // not time the server spent on this statement.
        let started = Instant::now();
        let outcome = guarded(|| {
            session.runtime.block_on(async {
                let collect = collect(&mut session.client, sql);
                match limit {
                    0 => Ok(collect.await),
                    seconds => {
                        tokio::time::timeout(Duration::from_secs(seconds.into()), collect).await
                    }
                }
            })
        });
        let elapsed = started.elapsed();
        let stopped = {
            let mut in_flight = self.in_flight();
            in_flight.running = false;
            std::mem::take(&mut in_flight.stopped)
        };

        let collected = match outcome {
            Ok(Ok(Ok(collected))) if !stopped => collected,
            Ok(Ok(Err(error))) if !stopped => {
                let error = query_error(&error, sql);
                return Err(rolled_back(session, sql, error));
            }
            // The server has no statement timeout, so the timer is dbdelve's,
            // and what it stops has to be stopped the way Cancel stops it.
            Ok(Err(_)) => {
                return Err(self.stop(
                    &mut guard,
                    format!(
                        "The statement ran past the {limit}-second statement timeout and was \
                         stopped by closing its connection."
                    ),
                ));
            }
            Ok(Ok(_)) => {
                return Err(self.stop(
                    &mut guard,
                    "Cancelled: the statement was stopped by closing its connection.".into(),
                ));
            }
            Err(error) => {
                return Err(self.stop(&mut guard, error.message));
            }
        };

        if editable && mentions_use(sql) {
            held_to_database(session, &self.server.database)?;
        }

        let mut result = collected.result;
        result.elapsed = elapsed;
        if editable && collected.sets == 1 && !result.columns.is_empty() {
            let probed = describe_columns(session, sql, &result.columns);
            drop(guard);
            result.edit = self.edit_target(&probed);
        }
        Ok(result)
    }

    /// Close what is left of a stopped session and open another, saying what
    /// that cost.
    fn stop(&self, session: &mut Option<Session>, what: String) -> DbError {
        if let Some(socket) = &self.in_flight().socket {
            // The runtime's own handle closes when the session drops, but this
            // second one would keep the connection open behind it.
            let _ = socket.shutdown(Shutdown::Both);
        }
        *session = None;
        let lost = "The server rolls back whatever a closed session had open, and its \
                    temporary tables and SET options go with it.";
        let reconnected = match self.connect() {
            Ok(fresh) => {
                *session = Some(fresh);
                "dbdelve reconnected.".to_string()
            }
            Err(error) => format!("Reconnecting failed: {}", error.message),
        };
        plain_error(format!("{what} {lost} {reconnected}"))
    }

    /// Which table these rows can be written back to, if any.
    ///
    /// Every step is allowed to answer "no": an undescribable statement, a
    /// join, a computed column, a table without a primary key, a key the select
    /// omitted. A failure answers "no" too -- this runs after the user's
    /// statement already succeeded, and must not turn that into an error.
    fn edit_target(&self, probed: &[ProbedColumn]) -> Option<EditTarget> {
        let (schema, table) = sole_table(probed)?;
        // ponytail: one catalog round trip per result set, no cache. A map from
        // table to key held on the connection is the upgrade path if the trip
        // shows up in query timings.
        let key = self
            .internal_query(&structure_sql(PRIMARY_KEY_SQL, &schema, &table))
            .ok()?;
        let key = key
            .rows
            .iter()
            .map(|row| required_cell(&key, row, "column_name").map(str::to_string))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        resolve_edit_target(probed, &schema, &table, &key)
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        assemble_catalog(self.internal_query(RELATIONS_SQL)?, QueryResult::default())
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        assemble_catalog(
            QueryResult::default(),
            self.internal_query(&ROUTINES_SQL.replace("{type}", TYPE_SQL))?,
        )
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let query = |template: &str| {
            self.internal_query(&structure_sql(
                &template.replace("{type}", TYPE_SQL),
                schema,
                relation,
            ))
        };
        let columns = query(STRUCTURE_COLUMNS_SQL)?;
        let indexes = query(STRUCTURE_INDEXES_SQL)?;
        let constraints = query(STRUCTURE_CONSTRAINTS_SQL)?;
        let keys = query(STRUCTURE_FOREIGN_KEYS_SQL)?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&keys)?;
        Ok(structure)
    }
}

/// A panic inside the driver would poison the session mutex and take the
/// background thread with it. tiberius has a few (`unimplemented` tokens such as
/// a `FOR BROWSE` result's, a trust store with no roots), and each is a failure
/// to report rather than a crash.
fn guarded<T>(call: impl FnOnce() -> T) -> Result<T, DbError> {
    std::panic::catch_unwind(AssertUnwindSafe(call)).map_err(|panic| {
        let detail = panic
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        plain_error(format!("The SQL Server driver failed: {detail}"))
    })
}

/// The TCP connect and the login, both inside one bound, plus a second handle
/// on the socket for Cancel.
async fn login(
    server: &ServerConfig,
    encryption: EncryptionLevel,
) -> Result<(Tds, std::net::TcpStream), tiberius::error::Error> {
    let mut server = server.clone();
    let bound = Duration::from_secs(CONNECT_TIMEOUT_SECONDS);
    tokio::time::timeout(bound, async {
        // Azure SQL answers a login from inside Azure with the address of the
        // node that holds the database, once.
        let mut redirected = false;
        loop {
            let config = config(&server, encryption);
            let tcp = TcpStream::connect(config.get_addr()).await?;
            tcp.set_nodelay(true)?;
            let tcp = tcp.into_std()?;
            let socket = tcp.try_clone()?;
            let tcp = TcpStream::from_std(tcp)?;
            let mut client = match Client::connect(config, tcp.compat_write()).await {
                Err(tiberius::error::Error::Routing { host, port }) if !redirected => {
                    redirected = true;
                    server.host = host;
                    server.port = Some(port);
                    continue;
                }
                other => other?,
            };
            // A session default, like the other engines' statement timeouts:
            // what makes a bracketed batch all-or-nothing (see `rolled_back`).
            client
                .simple_query("SET XACT_ABORT ON")
                .await?
                .into_results()
                .await?;
            return Ok((client, socket));
        }
    })
    .await
    .map_err(|_| tiberius::error::Error::Io {
        kind: std::io::ErrorKind::TimedOut,
        message: format!(
            "No answer from {} within {CONNECT_TIMEOUT_SECONDS} seconds.",
            server.endpoint()
        ),
    })?
}

/// What one submission returned: the last result set, which is the one the grid
/// shows, and how many there were.
struct Collected {
    result: QueryResult,
    sets: usize,
}

async fn collect(client: &mut Tds, sql: &str) -> Result<Collected, tiberius::error::Error> {
    let mut stream = client.simple_query(sql).await?;
    let mut result = QueryResult::default();
    let mut types = Vec::new();
    let mut sets = 0;

    while let Some(item) = stream.try_next().await? {
        match item {
            // Each new description starts the kept set over and the last
            // statement wins, as on every engine.
            QueryItem::Metadata(meta) => {
                types = meta.columns().iter().map(|c| c.column_type()).collect();
                result.columns = meta
                    .columns()
                    .iter()
                    .map(|column| Column {
                        name: column.name().to_string(),
                        data_type: Some(type_name(column.column_type()).to_string()),
                    })
                    .collect();
                result.rows.clear();
                result.bytes = 0;
                sets += 1;
            }
            QueryItem::Row(row) => {
                // The wire names `datetime` and `smalldatetime` alike; the
                // value says which.
                for (column, (_, value)) in result.columns.iter_mut().zip(row.cells()) {
                    if let ColumnData::SmallDateTime(Some(_)) = value {
                        column.data_type = Some("smalldatetime".into());
                    }
                }
                let cells: Vec<Cell> = row
                    .into_iter()
                    .zip(&types)
                    .map(|(value, kind)| render(&value, *kind))
                    .collect();
                result.bytes += cells.iter().flatten().map(String::len).sum::<usize>();
                result.rows.push(cells);
            }
        }
    }

    // A query's count is the rows it returned. A write's is in the protocol's
    // done tokens, which tiberius keeps to itself when a batch may return rows,
    // so a write reports none rather than a zero that would read as "nothing
    // changed".
    if sets > 0 {
        result.rows_affected = Some(result.rows.len() as u64);
    }
    Ok(Collected { result, sets })
}

/// The server's own name for a type, as far as the wire says it. Lengths and
/// precisions are the Structure tab's to show.
fn type_name(kind: ColumnType) -> &'static str {
    match kind {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// A value as the server would print it.
///
/// Money is the one value that arrives already lossy: tiberius decodes it to an
/// `f64`, which holds its four decimal places exactly below about 9 × 10¹¹.
fn render(value: &ColumnData<'_>, kind: ColumnType) -> Cell {
    Some(match value {
        ColumnData::U8(value) => value.as_ref()?.to_string(),
        ColumnData::I16(value) => value.as_ref()?.to_string(),
        ColumnData::I32(value) => value.as_ref()?.to_string(),
        ColumnData::I64(value) => value.as_ref()?.to_string(),
        ColumnData::F32(value) => value.as_ref()?.to_string(),
        ColumnData::F64(value) if matches!(kind, ColumnType::Money | ColumnType::Money4) => {
            format!("{:.4}", value.as_ref()?)
        }
        ColumnData::F64(value) => value.as_ref()?.to_string(),
        ColumnData::Bit(value) => u8::from(*value.as_ref()?).to_string(),
        ColumnData::String(value) => value.as_ref()?.to_string(),
        ColumnData::Guid(value) => value.as_ref()?.to_string().to_uppercase(),
        // T-SQL's own literal for bytes, so a value copied out of the grid can
        // be pasted into a statement.
        ColumnData::Binary(value) => format!("0x{}", hex::encode_upper(value.as_ref()?)),
        ColumnData::Numeric(value) => {
            let value = value.as_ref()?;
            decimal(value.value(), value.scale())
        }
        ColumnData::Xml(value) => value.as_ref()?.to_string(),
        ColumnData::DateTime(value) => {
            let value = value.as_ref()?;
            // 1/300 s ticks, shown to the millisecond the way the server rounds
            // them: .000, .003, .007.
            let milliseconds = (u64::from(value.seconds_fragments()) * 10 + 1) / 3;
            format!(
                "{} {}",
                date_from_1900(value.days().into()),
                fraction_of_day(milliseconds, 3)
            )
        }
        ColumnData::SmallDateTime(value) => {
            let value = value.as_ref()?;
            format!(
                "{} {}",
                date_from_1900(value.days().into()),
                fraction_of_day(u64::from(value.seconds_fragments()) * 60, 0)
            )
        }
        ColumnData::Time(value) => time(value.as_ref()?),
        ColumnData::Date(value) => date(value.as_ref()?.days().into()),
        ColumnData::DateTime2(value) => datetime2(value.as_ref()?),
        ColumnData::DateTimeOffset(value) => {
            let value = value.as_ref()?;
            let offset = i64::from(value.offset());
            // The wire carries the instant in UTC and the offset beside it;
            // the server prints the local time at that offset.
            let utc = value.datetime2();
            let scale = u32::from(utc.time().scale());
            let per_minute = 60 * 10i64.pow(scale);
            let per_day = 1_440 * per_minute;
            let increments = i64::from(utc.date().days()) * per_day
                + utc.time().increments() as i64
                + offset * per_minute;
            let (days, increments) = (
                increments.div_euclid(per_day),
                increments.rem_euclid(per_day),
            );
            let sign = if offset < 0 { '-' } else { '+' };
            format!(
                "{} {} {sign}{:02}:{:02}",
                date(days),
                fraction_of_day(increments as u64, scale),
                offset.abs() / 60,
                offset.abs() % 60
            )
        }
    })
}

/// Days from 0001-01-01, the epoch of every TDS 7.3 date.
fn date(days: i64) -> String {
    // Howard Hinnant's `civil_from_days`, shifted from 1970 to year 1.
    let z = days - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The legacy `datetime` and `smalldatetime` count from 1900-01-01.
fn date_from_1900(days: i64) -> String {
    date(days + 693_595)
}

fn time(value: &Time) -> String {
    fraction_of_day(value.increments(), value.scale().into())
}

fn datetime2(value: &DateTime2) -> String {
    format!(
        "{} {}",
        date(value.date().days().into()),
        time(&value.time())
    )
}

/// `HH:MM:SS` plus `scale` digits of fraction, from units of 10^-scale seconds.
fn fraction_of_day(increments: u64, scale: u32) -> String {
    let per_second = 10u64.pow(scale);
    let seconds = increments / per_second;
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    match scale {
        0 => clock,
        _ => format!(
            "{clock}.{:0width$}",
            increments % per_second,
            width = scale as usize
        ),
    }
}

/// An exact decimal from its unscaled integer, so `decimal(38,10)` keeps every
/// digit a float would drop.
fn decimal(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

/// One result column as the describe names it: where it was read from, if it
/// was read from anywhere. Plumbing between the describe and
/// [`resolve_edit_target`], and it stops in this module.
struct ProbedColumn {
    name: String,
    /// Database, schema and table. Absent for a computed column.
    table: Option<(String, String, String)>,
    column: Option<String>,
    /// Whether the table is in the database this connection is using. An
    /// edit target names a schema and a table and nothing above them.
    local: bool,
}

/// What each column the statement returned was read from, in order.
///
/// Asked after the statement ran, and only of a submission that returned one
/// result set: the describe answers for the *first* result set, which is the
/// one the grid shows only when there is one. It compiles the statement and
/// executes nothing. A column list that disagrees with what came back is
/// nothing known, since everything here is positional.
fn describe_columns(session: &mut Session, sql: &str, columns: &[Column]) -> Vec<ProbedColumn> {
    let describe = DESCRIBE_SQL.replace("{statement}", &Engine::SqlServer.quote_literal(sql));
    let Ok(Ok(collected)) = guarded(|| {
        session
            .runtime
            .block_on(collect(&mut session.client, &describe))
    }) else {
        return Vec::new();
    };
    let described = &collected.result;
    let cell = |row: &[Cell], name: &str| -> Option<String> {
        let index = described.columns.iter().position(|c| c.name == name)?;
        row.get(index)?.clone()
    };

    let probed: Vec<ProbedColumn> = described
        .rows
        .iter()
        .map(|row| {
            let table = match (
                cell(row, "source_database"),
                cell(row, "source_schema"),
                cell(row, "source_table"),
            ) {
                (Some(database), Some(schema), Some(table)) => Some((database, schema, table)),
                _ => None,
            };
            ProbedColumn {
                name: cell(row, "column_name").unwrap_or_default(),
                local: cell(row, "source_server").is_none()
                    && table.as_ref().map(|(database, ..)| database)
                        == cell(row, "current_database").as_ref(),
                column: table.as_ref().and(cell(row, "source_column")),
                table,
            }
        })
        .collect();

    let agrees = probed.len() == columns.len()
        && probed
            .iter()
            .zip(columns)
            .all(|(probed, column)| probed.name == column.name);
    if agrees { probed } else { Vec::new() }
}

/// The one table every column that came from a table came from, when it is in
/// this database. A join is two tables and a row of it is a row of neither.
fn sole_table(probed: &[ProbedColumn]) -> Option<(String, String)> {
    let mut tables = probed
        .iter()
        .filter_map(|column| Some((column.table.as_ref()?, column.local)));
    let (first, local) = tables.next()?;
    let (_, schema, table) = first;
    (local && tables.all(|(other, _)| other == first)).then(|| (schema.clone(), table.clone()))
}

/// The describe decided against the table's key.
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
    let column_of = |column: &ProbedColumn| column.table.as_ref().and(column.column.clone());
    // Two result columns reading the same table column are the two sides of a
    // self-join, and the key located by position would resolve to whichever
    // came first and write the edit at the other row's key.
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

/// A relation name is user data and can contain a quote. `QUOTENAME` brackets
/// it for `OBJECT_ID`, which reads brackets whatever `QUOTED_IDENTIFIER` says.
fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    let object = format!(
        "OBJECT_ID(QUOTENAME({}) + N'.' + QUOTENAME({}))",
        Engine::SqlServer.quote_literal(schema),
        Engine::SqlServer.quote_literal(relation)
    );
    template.replace("{object}", &object)
}

fn connect_error(error: &tiberius::error::Error, server: &ServerConfig) -> DbError {
    // A refused connection is the most common failure by a wide margin, and the
    // driver's own wording buries the endpoint. Say what happened, and nothing
    // about what the user should do -- we cannot see their machine.
    if let tiberius::error::Error::Io { kind, .. } = error
        && *kind == std::io::ErrorKind::ConnectionRefused
    {
        return plain_error(format!(
            "Connection refused: nothing is listening on {}",
            server.endpoint()
        ));
    }
    plain_error(describe(error))
}

fn query_error(error: &tiberius::error::Error, sql: &str) -> DbError {
    DbError {
        message: describe(error),
        // SQL Server names a line rather than a character, so the offset is
        // where that line starts: true, if less precise than Postgres's.
        position: match error {
            tiberius::error::Error::Server(token) => line_start(sql, token.line()),
            _ => None,
        },
    }
}

fn line_start(sql: &str, line: u32) -> Option<usize> {
    match line.checked_sub(1)? {
        0 => Some(0),
        skipped => sql
            .match_indices('\n')
            .nth(skipped as usize - 1)
            .map(|(offset, _)| offset + 1),
    }
}

/// Prefer the server's own message; the driver's wrapper adds the server name,
/// the line and three numbers that mostly repeat it.
fn describe(error: &tiberius::error::Error) -> String {
    match error {
        tiberius::error::Error::Server(token) => token.message().to_string(),
        other => other.to_string(),
    }
}

/// A profile is bound to one database, as on Postgres, and T-SQL is the one
/// dialect here where a statement can move the session to another: the explorer
/// and every statement dbdelve writes name objects by schema alone, so after a
/// `USE` a generated `DELETE` would find the same name in the wrong database.
/// So the session is moved back, and the run reports it.
///
/// ponytail: a word scan for `USE`, not a parse, so a comment or a literal
/// holding the word costs one `DB_NAME()` round trip and nothing else.
fn mentions_use(sql: &str) -> bool {
    sql.split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|word| word.eq_ignore_ascii_case("use"))
}

fn held_to_database(session: &mut Session, database: &str) -> Result<(), DbError> {
    let mut ask = |statement: &str| {
        guarded(|| {
            session
                .runtime
                .block_on(collect(&mut session.client, statement))
        })?
        .map_err(|error| plain_error(describe(&error)))
    };
    let current = ask("SELECT DB_NAME()")?
        .result
        .rows
        .first()
        .and_then(|row| row.first()?.clone())
        .unwrap_or_default();
    if current.eq_ignore_ascii_case(database) {
        return Ok(());
    }
    ask(&format!("USE [{}]", database.replace(']', "]]")))?;
    Err(plain_error(format!(
        "The statement moved the session to database {current}, and dbdelve moved it back to \
         {database}: this connection's explorer, and every statement dbdelve writes, name \
         {database}'s objects."
    )))
}

/// Say what state a failed generated batch left the data in.
///
/// `SET XACT_ABORT ON` is what makes the brackets atomic here at all: without
/// it a constraint violation ends only its own statement, the batch carries on,
/// and the `COMMIT` dbdelve wrote commits the rows before it. T-SQL has no
/// nested transactions, so the rollback it forces takes any transaction the
/// user already had open with it -- which `@@TRANCOUNT` reports.
fn rolled_back(session: &mut Session, sql: &str, error: DbError) -> DbError {
    let Some(start) = Engine::SqlServer.transaction_start() else {
        return error;
    };
    if !sql
        .trim_start()
        .get(..start.len())
        .is_some_and(|word| word.eq_ignore_ascii_case(start))
    {
        return error;
    }

    let mut ask = |statement: &str| {
        guarded(|| {
            session
                .runtime
                .block_on(collect(&mut session.client, statement))
        })
        .ok()
        .and_then(Result::ok)
    };
    let open = ask("SELECT @@TRANCOUNT")
        .and_then(|collected| collected.result.rows.first()?.first()?.clone())
        .is_some_and(|count| count != "0");
    let outcome = match open {
        false => "The transaction was rolled back; nothing the batch wrote remains.",
        true => match ask("ROLLBACK") {
            Some(_) => "The transaction was rolled back; nothing the batch wrote remains.",
            None => "The transaction the batch opened is still open: the rollback failed too.",
        },
    };
    DbError {
        message: format!("{}\n\n{outcome}", error.message),
        position: error.position,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, ForeignKey, RelationKind, RoutineKind};
    use crate::filter::{Operator, filter_predicate, relation_sql};
    use crate::result_grid::{NewValue, PendingRow};
    use crate::sql::{self, SortKey};
    use tiberius::numeric::Numeric;
    use tiberius::time::{Date, DateTime, DateTimeOffset, SmallDateTime};

    /// The server the `live_` tests talk to, from `dbdelve_MSSQL_URL`.
    fn live_config() -> ServerConfig {
        let url = std::env::var("dbdelve_MSSQL_URL").expect("dbdelve_MSSQL_URL is required");
        // The URL's own `sslmode`, `prefer` when it names none: the compose
        // server speaks TLS with a certificate signed by nobody, which `prefer`
        // encrypts to without checking.
        config_from_url(&url).expect("dbdelve_MSSQL_URL should parse")
    }

    fn live() -> Connection {
        Connection::open(&live_config()).expect("connection should open")
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

    fn first(result: &QueryResult) -> Vec<Option<&str>> {
        result.rows[0].iter().map(Option::as_deref).collect()
    }

    fn probed(columns: &[(Option<&str>, Option<&str>)]) -> Vec<ProbedColumn> {
        columns
            .iter()
            .map(|(table, column)| ProbedColumn {
                name: column.unwrap_or("?").to_string(),
                table: table.map(|table| ("dbdelve_dev".into(), "dbo".into(), table.into())),
                column: column.map(str::to_string),
                local: true,
            })
            .collect()
    }

    #[test]
    fn a_url_fills_the_fields_without_inventing_a_port() {
        let config =
            config_from_url("mssql://person%40example.com:pa%20ss@db.example.test/dbdelve_test")
                .unwrap();
        assert_eq!(
            config,
            ServerConfig {
                host: "db.example.test".into(),
                port: None,
                database: "dbdelve_test".into(),
                user: "person@example.com".into(),
                password: "pa ss".into(),
                sslmode: SslMode::default(),
                root_certificate: None,
                statement_timeout: 0,
            }
        );
        let error = config_from_url("mssql://someone@db.example.test/db?encrypt=true").unwrap_err();
        assert!(
            error.contains("encrypt") && error.contains("SQL Server"),
            "{error}"
        );
    }

    #[test]
    fn every_rung_gets_the_encryption_and_trust_it_asked_for() {
        // Hard rule 7 in code. tiberius keeps both settings private, so its
        // own `Debug` is what there is to read them from.
        let settings = |sslmode, root: Option<&str>| {
            let server = ServerConfig {
                sslmode,
                root_certificate: root.map(str::to_string),
                ..ServerConfig::default()
            };
            format!("{:?}", config(&server, encryption(sslmode)))
        };
        let disable = settings(SslMode::Disable, None);
        assert!(disable.contains("encryption: Off") && disable.contains("TrustAll"));
        for mode in [SslMode::Prefer, SslMode::Require] {
            let text = settings(mode, None);
            assert!(
                text.contains("encryption: Required") && text.contains("TrustAll"),
                "{text}"
            );
        }
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let platform = settings(mode, None);
            assert!(platform.contains("Required") && platform.contains("trust: Default"));
            let pinned = settings(mode, Some("/tmp/ca.pem"));
            assert!(pinned.contains("CaCertificateLocation"), "{pinned}");
            assert!(!pinned.contains("TrustAll"), "{pinned}");
        }
    }

    #[test]
    fn values_render_the_way_the_server_prints_them() {
        let render = |value: ColumnData<'static>| super::render(&value, ColumnType::Null);
        let text = |value: ColumnData<'static>| render(value).expect("not null");

        assert_eq!(render(ColumnData::I32(None)), None);
        assert_eq!(text(ColumnData::Date(Some(Date::new(0)))), "0001-01-01");
        assert_eq!(
            text(ColumnData::Date(Some(Date::new(738_944)))),
            "2024-02-29"
        );
        assert_eq!(
            text(ColumnData::Date(Some(Date::new(3_652_058)))),
            "9999-12-31"
        );
        assert_eq!(
            text(ColumnData::Time(Some(Time::new(452_961_234_567, 7)))),
            "12:34:56.1234567"
        );
        assert_eq!(
            text(ColumnData::Time(Some(Time::new(45_296, 0)))),
            "12:34:56"
        );
        assert_eq!(
            text(ColumnData::DateTime2(Some(DateTime2::new(
                Date::new(738_944),
                Time::new(45_296_123_456, 6)
            )))),
            "2024-02-29 12:34:56.123456"
        );
        // 1/300 s ticks, rounded the way the server prints them.
        let legacy = |days, seconds: u32, ticks| {
            text(ColumnData::DateTime(Some(DateTime::new(
                days,
                seconds * 300 + ticks,
            ))))
        };
        assert_eq!(legacy(45_290, 45_296, 236), "2024-01-01 12:34:56.787");
        assert_eq!(legacy(45_290, 0, 1), "2024-01-01 00:00:00.003");
        assert_eq!(legacy(45_290, 0, 2), "2024-01-01 00:00:00.007");
        assert_eq!(legacy(-53_690, 0, 0), "1753-01-01 00:00:00.000");
        assert_eq!(
            text(ColumnData::SmallDateTime(Some(SmallDateTime::new(
                45_290, 754
            )))),
            "2024-01-01 12:34:00"
        );
        // The instant travels in UTC; the server prints the local time.
        let offset = |seconds, minutes| {
            text(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(
                DateTime2::new(Date::new(739_037), Time::new(seconds, 0)),
                minutes,
            ))))
        };
        assert_eq!(offset(28_800, 120), "2024-06-01 10:00:00 +02:00");
        assert_eq!(offset(84_600, 60), "2024-06-02 00:30:00 +01:00");
        assert_eq!(offset(7_200, -330), "2024-05-31 20:30:00 -05:30");

        let numeric = |value, scale| {
            text(ColumnData::Numeric(Some(Numeric::new_with_scale(
                value, scale,
            ))))
        };
        assert_eq!(numeric(12_500_050, 2), "125000.50");
        assert_eq!(numeric(-1, 2), "-0.01");
        assert_eq!(numeric(0, 2), "0.00");
        assert_eq!(numeric(42, 0), "42");
        assert_eq!(
            super::render(&ColumnData::F64(Some(12.5)), ColumnType::Money).as_deref(),
            Some("12.5000")
        );
        assert_eq!(text(ColumnData::F64(Some(12.5))), "12.5");
        assert_eq!(text(ColumnData::Bit(Some(true))), "1");
        assert_eq!(
            text(ColumnData::Binary(Some(vec![0x00, 0xff].into()))),
            "0x00FF"
        );
        assert_eq!(
            text(ColumnData::Guid(Some(
                tiberius::Uuid::parse_str("018f1f6e-7c2a-7000-8000-00000000000a").unwrap()
            ))),
            "018F1F6E-7C2A-7000-8000-00000000000A"
        );
    }

    #[test]
    fn structure_sql_quotes_a_name_containing_a_quote() {
        assert_eq!(
            structure_sql("WHERE x = {object}", "dbo", "odd'name"),
            "WHERE x = OBJECT_ID(QUOTENAME(N'dbo') + N'.' + QUOTENAME(N'odd''name'))"
        );
    }

    #[test]
    fn a_server_line_number_points_at_the_start_of_that_line() {
        let sql = "SELECT 1;\nSELECT nope;\nSELECT 3";
        assert_eq!(line_start(sql, 1), Some(0));
        assert_eq!(line_start(sql, 2), Some(10));
        assert_eq!(&sql[line_start(sql, 3).unwrap()..], "SELECT 3");
        assert_eq!(line_start(sql, 0), None);
        assert_eq!(line_start(sql, 9), None);
    }

    #[test]
    fn only_a_use_as_a_word_asks_where_the_session_is() {
        assert!(mentions_use("USE master; SELECT 1"));
        assert!(mentions_use("select 1;\nuse [x]"));
        assert!(!mentions_use("SELECT used, user_id FROM reuse"));
    }

    #[test]
    fn sole_table_needs_one_local_table_and_at_least_one() {
        assert_eq!(
            sole_table(&probed(&[(Some("accounts"), Some("id")), (None, None)])),
            Some(("dbo".into(), "accounts".into()))
        );
        assert_eq!(
            sole_table(&probed(&[
                (Some("accounts"), Some("id")),
                (Some("locations"), Some("id")),
            ])),
            None
        );
        assert_eq!(sole_table(&probed(&[(None, None)])), None);
        // A three-part name reads another database, and an edit target has no
        // room to say which.
        let mut elsewhere = probed(&[(Some("accounts"), Some("id"))]);
        elsewhere[0].local = false;
        assert_eq!(sole_table(&elsewhere), None);
    }

    #[test]
    fn an_edit_target_names_real_columns_and_locates_the_whole_key() {
        let target = resolve_edit_target(
            &probed(&[
                (Some("orders"), Some("total")),
                (None, None),
                (Some("orders"), Some("number")),
                (Some("orders"), Some("account_id")),
            ]),
            "dbo",
            "orders",
            &["account_id".to_string(), "number".to_string()],
        )
        .expect("the whole key is present");
        assert_eq!(target.keys, vec![3, 2]);
        assert_eq!(target.columns[1], None);

        // Half a key, and a self-join, are both refused.
        assert!(
            resolve_edit_target(
                &probed(&[(Some("orders"), Some("number"))]),
                "dbo",
                "orders",
                &["account_id".to_string(), "number".to_string()],
            )
            .is_none()
        );
        assert!(
            resolve_edit_target(
                &probed(&[
                    (Some("accounts"), Some("id")),
                    (Some("accounts"), Some("name")),
                    (Some("accounts"), Some("name")),
                ]),
                "dbo",
                "accounts",
                &["id".to_string()],
            )
            .is_none()
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_the_development_database_is_fully_seeded() {
        let result = live()
            .query("SELECT count(*) AS rows_seeded FROM measurements")
            .expect("query should succeed");
        assert_eq!(first(&result), vec![Some("5000")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_query_round_trip() {
        let result = live()
            .query("SELECT 1 AS id, N'alpha' AS label UNION ALL SELECT 2, NULL")
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "label"]);
        assert_eq!(types(&result), vec![Some("int"), Some("nvarchar")]);
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
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_multi_statement_selection_keeps_one_result_shape() {
        let connection = live();
        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d")
            .expect("query should succeed");
        assert_eq!(names(&result), vec!["d"]);
        assert_eq!(result.rows, vec![vec![Some("4".into())]]);
        // Two result sets, so the describe cannot say which it described.
        assert!(result.edit.is_none());

        let empty = connection
            .query("SELECT 1 AS id, N'x' AS label WHERE 1 = 0")
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id", "label"]);
        assert!(empty.rows.is_empty());

        // A write returns no rows and, from tiberius, no count.
        let write = connection
            .query("DECLARE @t TABLE (id int); INSERT INTO @t VALUES (1)")
            .expect("query should succeed");
        assert!(write.columns.is_empty());
        assert_eq!(write.rows_affected, None);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_every_seeded_type_renders_as_the_server_prints_it() {
        let result = live()
            .query(
                "SELECT
                    CAST('2024-02-29' AS date) AS a_date,
                    CAST('12:34:56.1234567' AS time(7)) AS a_time,
                    CAST('2024-03-10 18:45:12.123456' AS datetime2(6)) AS a_datetime2,
                    CAST('2024-01-01 12:34:56.787' AS datetime) AS a_datetime,
                    CAST('2024-01-01 12:34:29' AS smalldatetime) AS a_smalldatetime,
                    CAST('2024-06-01 10:00:00 +02:00' AS datetimeoffset(0)) AS a_offset,
                    CAST(-0.01 AS decimal(14, 2)) AS a_decimal,
                    CAST(12.5 AS money) AS a_money,
                    CAST(1 AS bit) AS a_bit,
                    CAST('018f1f6e-7c2a-7000-8000-000000000001' AS uniqueidentifier) AS a_guid,
                    CONVERT(varbinary(6), '00010203feff', 2) AS a_binary,
                    CAST(N'<a>b</a>' AS xml) AS a_xml,
                    CAST(1.5 AS float) AS a_float,
                    CAST(2.5 AS real) AS a_real,
                    CAST(7 AS tinyint) AS a_tinyint,
                    CAST(9007199254740993 AS bigint) AS a_bigint,
                    N'李小龍 🐉' AS a_unicode",
            )
            .expect("query should succeed");

        assert_eq!(
            first(&result),
            vec![
                Some("2024-02-29"),
                Some("12:34:56.1234567"),
                Some("2024-03-10 18:45:12.123456"),
                Some("2024-01-01 12:34:56.787"),
                Some("2024-01-01 12:34:00"),
                Some("2024-06-01 10:00:00 +02:00"),
                Some("-0.01"),
                Some("12.5000"),
                Some("1"),
                Some("018F1F6E-7C2A-7000-8000-000000000001"),
                Some("0x00010203FEFF"),
                Some("<a>b</a>"),
                Some("1.5"),
                Some("2.5"),
                Some("7"),
                Some("9007199254740993"),
                Some("李小龍 🐉"),
            ]
        );
        assert_eq!(
            types(&result),
            [
                "date",
                "time",
                "datetime2",
                "datetime",
                "smalldatetime",
                "datetimeoffset",
                "decimal",
                "money",
                "bit",
                "uniqueidentifier",
                "varbinary",
                "xml",
                "float",
                "real",
                "tinyint",
                "bigint",
                "nvarchar",
            ]
            .map(Some)
        );

        // And seeded rows, read back as stored.
        let seeded = live()
            .query(
                "SELECT a.external_id, a.balance, a.created_at, o.placed_at
                 FROM accounts AS a JOIN orders AS o ON o.account_id = a.id
                 WHERE o.number = 2001",
            )
            .expect("query should succeed");
        assert_eq!(
            first(&seeded),
            vec![
                Some("018F1F6E-7C2A-7000-8000-000000000002"),
                Some("8192.00"),
                Some("2024-02-29 12:00:00.000000"),
                Some("2024-06-12 16:15:00 +00:00"),
            ]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_catalog_round_trip() {
        let connection = live();
        let mut catalog = connection.catalog().expect("catalog should load");
        catalog.merge(connection.routines().expect("routines should load"));
        let dbo = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "dbo")
            .expect("dbo should exist");

        assert!(dbo.relations.iter().any(|relation| {
            relation.name == "accounts" && relation.kind == RelationKind::Table
        }));
        assert!(dbo.relations.iter().any(|relation| {
            relation.name == "account_overview" && relation.kind == RelationKind::View
        }));
        let label = dbo
            .routines
            .iter()
            .find(|routine| routine.name == "account_label")
            .expect("the seeded function is listed");
        assert_eq!(label.kind, RoutineKind::Function);
        assert_eq!(label.identity_arguments, "@account_id bigint");
        assert_eq!(label.result_type, "nvarchar(400)");
        assert!(label.definition.contains("RETURNS nvarchar(400)"));
        assert!(dbo.routines.iter().any(|routine| {
            routine.name == "deactivate_account" && routine.kind == RoutineKind::Procedure
        }));
        assert!(
            catalog
                .schemas
                .iter()
                .any(|schema| schema.name == "archive")
        );
        // The server's own schemas are not the user's.
        assert!(
            !catalog
                .schemas
                .iter()
                .any(|schema| schema.name == "sys" || schema.name == "INFORMATION_SCHEMA")
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_structure_round_trip() {
        let structure = live()
            .structure("dbo", "accounts")
            .expect("structure should load");

        assert!(structure.columns.contains(&ColumnDefinition {
            name: "id".into(),
            data_type: "bigint".into(),
            nullable: false,
            // The server supplies it, and no default constraint says so.
            default: Some("IDENTITY(1,1)".into()),
        }));
        assert!(structure.columns.iter().any(|column| {
            column.name == "email" && column.nullable && column.data_type == "nvarchar(200)"
        }));
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "balance" && column.data_type == "decimal(14,2)")
        );
        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition == "PRIMARY KEY (id)")
        );
        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.name == "CK_accounts_plan"
                    && constraint.definition.starts_with("CHECK "))
        );
        assert!(!structure.indexes.is_empty(), "the primary key is an index");
        assert!(structure.foreign_keys.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_foreign_keys_arrive_one_column_per_key_position_across_schemas() {
        let connection = live();
        let items = connection
            .structure("dbo", "order_items")
            .expect("structure should load");
        assert_eq!(
            items.foreign_keys,
            vec![
                ForeignKey {
                    column: "order_account_id".into(),
                    referenced_schema: "dbo".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "account_id".into(),
                },
                ForeignKey {
                    column: "order_number".into(),
                    referenced_schema: "dbo".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "number".into(),
                },
            ]
        );
        assert!(
            items
                .constraints
                .iter()
                .any(|constraint| constraint.definition
                    == "FOREIGN KEY (order_account_id, order_number) \
                    REFERENCES dbo.orders (account_id, number)"),
            "{:?}",
            items.constraints
        );

        let closed = connection
            .structure("archive", "closed_accounts")
            .expect("structure should load");
        assert_eq!(
            closed.foreign_keys,
            vec![ForeignKey {
                column: "account_id".into(),
                referenced_schema: "dbo".into(),
                referenced_table: "accounts".into(),
                referenced_column: "id".into(),
            }]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_paged_sorted_filtered_preview_runs_as_generated() {
        let connection = live();
        let preview = |filter: &str, offset| {
            let sql = relation_sql(
                Engine::SqlServer,
                "dbo",
                "measurements",
                filter,
                &[SortKey::new("\"id\"", false)],
                10,
                offset,
            );
            assert!(sql::is_generated_select(&sql), "{sql}");
            let paged = sql::paged(Engine::SqlServer, &sql).expect("a preview has a page");
            connection.query(&paged).expect("the preview should run")
        };

        // Rows whose sensor is `sensor-03` are ids 3, 27, 51 … 4995; sorted
        // down, page three of ten starts twenty rows below the top.
        let equals =
            filter_predicate(Engine::SqlServer, "sensor", Operator::Equals, "sensor-03").unwrap();
        let page = preview(&equals, 20);
        assert_eq!(page.rows.len(), 10);
        assert_eq!(page.rows[0][0].as_deref(), Some("4515"));
        assert_eq!(page.rows[9][0].as_deref(), Some("4299"));
        // An object tab's rows are editable by their key, like any single
        // table's.
        assert_eq!(
            page.edit.as_ref().map(|edit| edit.keys.clone()),
            Some(vec![0])
        );

        // `_` matches itself, not any character: no sensor is `sensor_03`.
        let literal =
            filter_predicate(Engine::SqlServer, "sensor", Operator::Contains, "sensor_03").unwrap();
        assert!(preview(&literal, 0).rows.is_empty());
        let unsorted = relation_sql(Engine::SqlServer, "dbo", "orders", "", &[], 100, 0);
        let unsorted = sql::paged(Engine::SqlServer, &unsorted).unwrap();
        assert_eq!(connection.query(&unsorted).unwrap().rows.len(), 3);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_single_table_select_is_editable_by_its_primary_key() {
        let connection = live();
        let edit = connection
            .query("SELECT name, id FROM accounts")
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");
        assert_eq!(edit.schema, "dbo");
        assert_eq!(edit.table, "accounts");
        assert_eq!(edit.columns, vec![Some("name".into()), Some("id".into())]);
        assert_eq!(edit.keys, vec![1]);

        let aliased = connection
            .query("SELECT id AS ident, upper(name) AS shouted, name FROM accounts")
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");
        assert_eq!(
            aliased.columns,
            vec![Some("id".into()), None, Some("name".into())]
        );
        assert_eq!(aliased.keys, vec![0]);

        let composite = connection
            .query("SELECT total, number, account_id FROM orders")
            .expect("query should succeed")
            .edit
            .expect("both key columns are present");
        assert_eq!(composite.keys, vec![2, 1]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_join_an_aggregate_a_view_or_a_missing_key_is_not_editable() {
        let connection = live();
        for sql in [
            "SELECT accounts.id, locations.name
             FROM accounts JOIN locations ON locations.id = accounts.id",
            "SELECT [plan], count(*) FROM accounts GROUP BY [plan]",
            "SELECT 1 AS one",
            "SELECT name, email FROM accounts",
            "SELECT * FROM account_overview",
            // A join reaching into another database is still a join.
            "SELECT a.id, o.name FROM accounts AS a JOIN master.sys.objects AS o ON o.object_id = a.id",
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
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_describing_a_result_leaves_an_open_transaction_alone() {
        let connection = live();
        connection
            .query("BEGIN TRANSACTION")
            .expect("BEGIN TRANSACTION should succeed");
        assert!(
            connection
                .query("SELECT id FROM accounts")
                .expect("query should succeed")
                .edit
                .is_some()
        );
        let open = connection.query("SELECT @@TRANCOUNT").unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        let error = connection
            .query("SELECT * FROM no_such_relation")
            .unwrap_err();
        assert!(
            error.message.contains("no_such_relation"),
            "{}",
            error.message
        );
        // `XACT_ABORT` ends the transaction on that error, as it would a batch's.
        let open = connection.query("SELECT @@TRANCOUNT").unwrap();
        assert_eq!(first(&open), vec![Some("0")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_an_edit_round_trips_by_primary_key() {
        let connection = live();
        let run = |sql: &str| {
            assert!(sql::is_generated_write(sql), "the gate refused {sql}");
            connection.query(sql)
        };
        let name_of = |id: &str| {
            connection
                .query(&format!("SELECT name FROM locations WHERE id = {id}"))
                .unwrap()
                .rows
                .first()
                .and_then(|row| row[0].clone())
        };
        let row = |id: &str, name: &str| PendingRow {
            schema: "dbo".into(),
            table: "locations".into(),
            sets: vec![("name".into(), NewValue::Value(name.to_string().into()))],
            keys: vec![("id".into(), id.into())],
        };
        for id in ["901", "902"] {
            let _ = connection.query(&format!("DELETE FROM locations WHERE id = {id}"));
            run(&sql::insert_row(
                Engine::SqlServer,
                "dbo",
                "locations",
                &[("id", Some(id)), ("name", Some("placeholder"))],
            )
            .unwrap())
            .expect("the insert should run");
        }

        run(&sql::update_row(
            Engine::SqlServer,
            "dbo",
            "locations",
            &[("name", NewValue::Value("Zoë 李 🐉 'quoted'".into()))],
            &[("id", "901")],
        )
        .unwrap())
        .expect("the update should run");
        // Outside the code page and through the quote: the `N` prefix.
        assert_eq!(name_of("901").as_deref(), Some("Zoë 李 🐉 'quoted'"));

        let batch = sql::update_batch(
            Engine::SqlServer,
            &[row("901", "first"), row("902", "second")],
        )
        .unwrap();
        run(&batch).expect("the batch should run");
        assert_eq!(name_of("901").as_deref(), Some("first"));
        assert_eq!(name_of("902").as_deref(), Some("second"));

        // The second statement breaks `NOT NULL`. Without `XACT_ABORT` the
        // batch would carry on and commit the first.
        let failing = sql::update_batch(
            Engine::SqlServer,
            &[
                row("901", "changed"),
                PendingRow {
                    sets: vec![("name".into(), NewValue::Null)],
                    ..row("902", "")
                },
            ],
        )
        .unwrap();
        let error = run(&failing).expect_err("NULL into a NOT NULL column fails");
        assert!(error.message.contains("rolled back"), "{}", error.message);
        assert_eq!(name_of("901").as_deref(), Some("first"));
        assert_eq!(
            first(&connection.query("SELECT @@TRANCOUNT").unwrap()),
            vec![Some("0")]
        );

        for id in ["901", "902"] {
            let delete =
                sql::delete_row(Engine::SqlServer, "dbo", "locations", &[("id", id)]).unwrap();
            assert!(sql::delete_matches_key(&delete, &["id"]));
            run(&delete).expect("the delete should run");
            assert_eq!(name_of(id), None);
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_cancel_stops_the_statement_on_the_server_and_reconnects() {
        // The probe outlives the session it is checked from only as a global
        // temporary table, which lives as long as the session that made it.
        let observer = live();
        observer
            .query("CREATE TABLE ##dbdelve_cancel_probe (id int)")
            .expect("the probe should be created");

        let connection = live();
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the cancel should send");
        });
        let started = Instant::now();
        let error = connection
            .query("WAITFOR DELAY '00:00:03'; INSERT INTO ##dbdelve_cancel_probe VALUES (1)")
            .expect_err("the statement should be cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(error.message.starts_with("Cancelled"), "{}", error.message);
        assert!(error.message.contains("reconnected"), "{}", error.message);

        // Honest only if the server stopped too: past the delay, the insert
        // behind it never ran.
        std::thread::sleep(Duration::from_secs(4));
        let count = observer
            .query("SELECT count(*) FROM ##dbdelve_cancel_probe")
            .unwrap();
        assert_eq!(first(&count), vec![Some("0")]);
        assert!(connection.query("SELECT 1").is_ok());
        // A cancel with nothing running stops nothing.
        connection.cancel().unwrap();
        assert!(connection.query("SELECT 1").is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_statement_timeout_stops_a_statement_that_outlasts_it() {
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 1,
            ..live_config()
        })
        .expect("connection should open");
        let error = connection
            .query("WAITFOR DELAY '00:00:30'")
            .expect_err("the statement should time out");
        assert!(
            error.message.contains("1-second statement timeout"),
            "{}",
            error.message
        );
        assert!(connection.query("SELECT 1").is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_use_is_moved_back_and_said() {
        let connection = live();
        let error = connection
            .query("USE master; SELECT 1")
            .expect_err("leaving the profile's database is reported");
        assert!(error.message.contains("moved it back"), "{}", error.message);
        let here = connection.query("SELECT DB_NAME()").unwrap();
        assert_eq!(first(&here), vec![Some(live_config().database.as_str())]);
    }

    /// What each mode does against the compose server, whose certificate is
    /// signed by nobody. Hard rule 7: the rungs that promise only encryption
    /// connect, and the two verifying rungs refuse and say why.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_only_the_modes_that_tolerate_an_unchecked_certificate_connect() {
        let connect = |sslmode| {
            Connection::open(&ServerConfig {
                sslmode,
                ..live_config()
            })
        };
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require] {
            assert!(connect(mode).is_ok(), "{mode:?} should connect");
        }
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let Err(error) = connect(mode) else {
                panic!("{mode:?} must not accept a certificate it cannot verify");
            };
            let message = error.message.to_lowercase();
            assert!(
                message.contains("tls") || message.contains("certificate"),
                "{mode:?} failed without saying why: {}",
                error.message
            );
        }
    }
}

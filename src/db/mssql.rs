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
use std::net::{Shutdown, SocketAddr};
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

use super::ssh::{Tunnel, tunnelled};
use super::{
    Catalog, Cell, Column, DbError, EditTarget, Engine, Fetch, QueryResult, Reference,
    RelationKind, ServerConfig, SslMode, Stopped, Structure, assemble_catalog, assemble_databases,
    assemble_foreign_keys, assemble_references, assemble_structure, create_table, plain_error,
    required_cell, terminated,
};

const DATABASES_SQL: &str = "
SELECT name, CASE WHEN name = DB_NAME() THEN 1 ELSE 0 END AS is_current
FROM sys.databases
WHERE HAS_DBACCESS(name) = 1
ORDER BY name";

const RELATIONS_SQL: &str = "
SELECT
    s.name AS schema_name,
    o.name AS relation_name,
    CASE o.type WHEN 'U' THEN 'table' WHEN 'V' THEN 'view' END AS relation_kind,
    CASE WHEN o.type = 'U' THEN sizes.size_bytes END AS size_bytes,
    CASE WHEN o.type = 'U' THEN counts.row_estimate END AS row_estimate
FROM sys.objects AS o
JOIN sys.schemas AS s ON s.schema_id = o.schema_id
-- The catalog views rather than `sys.dm_db_partition_stats`, which needs
-- VIEW DATABASE STATE and would fail the whole catalog for a login without it.
LEFT JOIN (
    SELECT p.object_id, SUM(a.used_pages) * 8192 AS size_bytes
    FROM sys.partitions AS p
    JOIN sys.allocation_units AS a ON a.container_id = p.partition_id
    GROUP BY p.object_id
) AS sizes ON sizes.object_id = o.object_id
-- Apart from the sizes, whose join repeats a partition once per allocation
-- unit; and only the heap or clustered index, since every other index holds
-- the same rows again.
LEFT JOIN (
    SELECT p.object_id, SUM(p.rows) AS row_estimate
    FROM sys.partitions AS p
    WHERE p.index_id IN (0, 1)
    GROUP BY p.object_id
) AS counts ON counts.object_id = o.object_id
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
///
/// `TYPE_NAME(p.user_type_id)` is null for an alias type the login has no
/// permission on, so it falls back to the base system type: an approximation,
/// but never the null that used to fail the whole tab.
const TYPE_SQL: &str = "(COALESCE(TYPE_NAME(p.user_type_id), TYPE_NAME(p.system_type_id)) COLLATE DATABASE_DEFAULT + CASE
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
// the quoted name, substituted by `structure_sql`. `{column}` spells a column
// name and `{ddl}` is 1 when the text is for `Connection::ddl` rather than the
// Structure tab, whose key text `Structure::row_key` reads back as bare names
// with no clustering or `DESC` in it.
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
        -- `computed.definition` is null without `VIEW DEFINITION`, and `CONCAT`
        -- turns that into a silent `N'AS '`; say the definition is hidden instead.
        WHEN p.is_computed = 1 THEN CONCAT(
            N'AS ', COALESCE(computed.definition COLLATE DATABASE_DEFAULT, N'<hidden>'),
            CASE WHEN computed.is_persisted = 1 THEN N' PERSISTED' ELSE N'' END
        )
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

// A columnstore index's columns all carry key_ordinal 0 and is_included_column
// 1 -- there is no key -- so the key and include lists below are read through
// `OUTER APPLY` rather than one inner join, or a columnstore index would join
// to nothing and vanish, and a `GROUP BY` over both lists at once would cross
// every key column with every included one.
const STRUCTURE_INDEXES_SQL: &str = "
SELECT
    i.name AS object_name,
    CASE i.type
        -- A clustered columnstore index has no key: every column is stored,
        -- and none is worth naming.
        WHEN 5 THEN CONCAT(i.type_desc COLLATE DATABASE_DEFAULT, N' INDEX')
        -- A nonclustered columnstore index has no key either; what it has is
        -- the columns it stores, read the same way an INCLUDE list is.
        WHEN 6 THEN CONCAT(
            i.type_desc COLLATE DATABASE_DEFAULT, N' INDEX (', included.list, N')'
        )
        ELSE CONCAT(
            CASE WHEN i.is_unique = 1 THEN N'UNIQUE ' ELSE N'' END,
            i.type_desc COLLATE DATABASE_DEFAULT, N' INDEX (', keyed.list, N')',
            CASE WHEN included.list IS NULL THEN N''
                ELSE CONCAT(N' INCLUDE (', included.list, N')') END,
            CASE WHEN i.has_filter = 1
                THEN CONCAT(N' WHERE ', i.filter_definition COLLATE DATABASE_DEFAULT)
                ELSE N'' END
        )
    END AS definition
FROM sys.indexes AS i
OUTER APPLY (
    SELECT STRING_AGG(
        CAST({column} COLLATE DATABASE_DEFAULT + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END
            AS nvarchar(max)),
        N', '
    ) WITHIN GROUP (ORDER BY ic.key_ordinal) AS list
    FROM sys.index_columns AS ic
    JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
    WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0
) AS keyed
OUTER APPLY (
    SELECT STRING_AGG(CAST({column} COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
        WITHIN GROUP (ORDER BY ic.index_column_id) AS list
    FROM sys.index_columns AS ic
    JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
    WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 1
) AS included
WHERE i.object_id = {object} AND i.name IS NOT NULL
ORDER BY i.name
";

const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT
    k.name AS object_name,
    CONCAT(
        CASE k.type WHEN 'PK' THEN N'PRIMARY KEY' ELSE N'UNIQUE' END,
        CASE WHEN {ddl} = 1 THEN N' ' + key_index.type_desc COLLATE DATABASE_DEFAULT ELSE N'' END,
        N' (',
        (
            SELECT STRING_AGG(
                CAST({column} COLLATE DATABASE_DEFAULT
                    + CASE WHEN {ddl} = 1 AND ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END
                    AS nvarchar(max)),
                N', '
            ) WITHIN GROUP (ORDER BY ic.key_ordinal)
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
JOIN sys.indexes AS key_index
    ON key_index.object_id = k.parent_object_id AND key_index.index_id = k.unique_index_id
WHERE k.parent_object_id = {object}
UNION ALL
SELECT
    f.name,
    CONCAT(
        N'FOREIGN KEY (',
        (
            SELECT STRING_AGG(CAST({column} COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
                WITHIN GROUP (ORDER BY fc.constraint_column_id)
            FROM sys.foreign_key_columns AS fc
            JOIN sys.columns AS c
                ON c.object_id = fc.parent_object_id AND c.column_id = fc.parent_column_id
            WHERE fc.constraint_object_id = f.object_id
        ),
        -- Referenced identifiers are quoted: unlike the source column list
        -- above, they cross a schema boundary the reader has no other way to
        -- see the bounds of.
        N') REFERENCES ', QUOTENAME(OBJECT_SCHEMA_NAME(f.referenced_object_id), '\"') COLLATE DATABASE_DEFAULT,
        N'.', QUOTENAME(OBJECT_NAME(f.referenced_object_id), '\"') COLLATE DATABASE_DEFAULT, N' (',
        (
            SELECT STRING_AGG(CAST(QUOTENAME(c.name, '\"') COLLATE DATABASE_DEFAULT AS nvarchar(max)), N', ')
                WITHIN GROUP (ORDER BY fc.constraint_column_id)
            FROM sys.foreign_key_columns AS fc
            JOIN sys.columns AS c
                ON c.object_id = fc.referenced_object_id AND c.column_id = fc.referenced_column_id
            WHERE fc.constraint_object_id = f.object_id
        ),
        N')',
        CASE WHEN f.delete_referential_action_desc <> 'NO_ACTION'
            THEN CONCAT(N' ON DELETE ', REPLACE(f.delete_referential_action_desc, '_', ' ') COLLATE DATABASE_DEFAULT)
            ELSE N'' END,
        CASE WHEN f.update_referential_action_desc <> 'NO_ACTION'
            THEN CONCAT(N' ON UPDATE ', REPLACE(f.update_referential_action_desc, '_', ' ') COLLATE DATABASE_DEFAULT)
            ELSE N'' END,
        CASE WHEN {ddl} = 0 AND f.is_disabled = 1 THEN N' DISABLED' ELSE N'' END,
        CASE WHEN {ddl} = 0 AND f.is_not_trusted = 1 THEN N' NOT TRUSTED' ELSE N'' END
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

// The same catalog read from the referenced side: who points at this relation.
const REFERENCES_SQL: &str = "
SELECT
    OBJECT_SCHEMA_NAME(fc.parent_object_id) AS source_schema,
    OBJECT_NAME(fc.parent_object_id) AS source_table,
    source_column.name AS column_name,
    referenced_column.name AS referenced_column,
    f.name AS constraint_name
FROM sys.foreign_key_columns AS fc
JOIN sys.foreign_keys AS f ON f.object_id = fc.constraint_object_id
JOIN sys.columns AS source_column
    ON source_column.object_id = fc.parent_object_id
    AND source_column.column_id = fc.parent_column_id
JOIN sys.columns AS referenced_column
    ON referenced_column.object_id = fc.referenced_object_id
    AND referenced_column.column_id = fc.referenced_column_id
WHERE fc.referenced_object_id = {object}
ORDER BY source_schema, source_table, f.name, fc.constraint_column_id
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
    -- False for a computed, identity or rowversion column, which a positioned
    -- `UPDATE` refuses; the column still names its row's key when it is one.
    is_updateable,
    DB_NAME() AS current_database
FROM sys.dm_exec_describe_first_result_set({statement}, NULL, 2)
WHERE is_hidden = 0 AND error_number IS NULL
ORDER BY column_ordinal
";

/// Without this a host that resolves but drops packets pins the UI in
/// "Connecting…" for the OS SYN retry budget. It bounds the login too, which
/// the TCP connect alone would not.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 1433;

/// What every statement dbdelve writes assumes of the session, and what a
/// user's `SET` can change under it: `XACT_ABORT` is what makes a bracketed
/// batch all-or-nothing, the quoting reads differently without the next four,
/// and `ROWCOUNT` would cut a preview short. `DATEFORMAT` is belt and braces:
/// `Engine::quote_value` spells `datetime` in the ISO form no `DATEFORMAT` or
/// `SET LANGUAGE` reorders, since the same literal is appended to a query
/// tab's buffer and runs there under the user's. See [`scoped`] for why they do
/// not outlive the batch.
///
/// One line: it goes in front of the statement, whose line numbers the
/// server's errors are counted in.
const SESSION_OPTIONS: &str = "SET XACT_ABORT ON; SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; \
    SET ANSI_WARNINGS ON; SET IMPLICIT_TRANSACTIONS OFF; SET ROWCOUNT 0; SET DATEFORMAT ymd; ";

/// What the preflight and describe of a user's statement assert of the
/// session, and nothing more: they compile the statement, so they do it under
/// the user's quoting and the rest of the user's options, but must neither
/// open a transaction nor be cut short by the user's `ROWCOUNT`.
const USER_DESCRIBE_OPTIONS: &str = "SET IMPLICIT_TRANSACTIONS OFF; SET ROWCOUNT 0; ";

/// Asked before a statement runs: how many transactions are open, and what the
/// first result set will hold. The count is what says afterwards whether a
/// failure took a transaction with it. The columns are what stops a type
/// tiberius cannot decode (sql_variant, and CLR types such as geography, 240)
/// from panicking the driver part way through a result, which closes the
/// session and the transaction in it. The join keeps the count when the
/// describe returns nothing.
const PREFLIGHT_SQL: &str = "
SELECT
    @@TRANCOUNT AS open_transactions,
    d.name AS column_name,
    d.system_type_id,
    d.system_type_name
FROM (SELECT 1 AS one) AS t
LEFT JOIN sys.dm_exec_describe_first_result_set({statement}, NULL, 0) AS d
    ON d.error_number IS NULL AND d.is_hidden = 0
ORDER BY d.column_ordinal
";

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
    if !server.database.is_empty() {
        config.database(&server.database);
    }
    config.application_name("DBDelve");
    // Sent as typed, blank included: cloud IAM issues a token or nothing.
    config.authentication(AuthMethod::sql_server(&server.user, &server.password));
    config.encryption(encryption);
    match (server.sslmode, &server.root_certificate) {
        (SslMode::VerifyCa | SslMode::VerifyFull, Some(path)) => {
            config.trust_cert_ca(crate::store::home_expanded(path).display())
        }
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

/// How a statement Cancel stopped part way begins its error, which goes on to
/// say what the reset cost.
pub(crate) const CANCELLED: &str =
    "Cancelled: the statement was stopped by closing its connection.";

/// Why a session was closed after a capped read.
const UNCAPPED: &str = "dbdelve could not take back the SET ROWCOUNT the row limit sent ahead of \
     it, so the session was closed rather than left capping every statement after it.";

/// The largest row limit `SET ROWCOUNT` can carry one past: it takes an
/// `int`. A larger one drains instead.
const MAX_ROWCOUNT_LIMIT: usize = i32::MAX as usize - 1;

/// A runtime, and the client it drives.
struct Session {
    runtime: Runtime,
    client: Tds,
    /// The profile's statement timeout, which bounds every round trip.
    timeout: u32,
    /// Set when a round trip left the session unusable: a read abandoned part
    /// way, or a driver panic, leaves the stream where nothing can resume it.
    lost: Option<Lost>,
}

enum Lost {
    TimedOut,
    Panicked(String),
    /// The `SET ROWCOUNT` a capped read went out under could not be taken
    /// back, and every statement after it would be capped too.
    Capped,
}

impl Session {
    /// One round trip, under the statement timeout. `None` once a trip has
    /// lost the session, which `Connection::run` answers by reconnecting.
    fn trip(&mut self, sql: &str) -> Option<Result<Collected, tiberius::error::Error>> {
        self.fetching(sql, Fetch::default())
    }

    /// [`Self::trip`], keeping what `fetch` asks for of the rows.
    ///
    /// The timeout stops applying once a set's kept rows reach the limit.
    /// What follows is rows read only to be counted and dropped, and stopping
    /// them would close the connection and take the user's transaction with
    /// it for rows nobody sees. Cancel still reaches it.
    fn fetching(
        &mut self,
        sql: &str,
        fetch: Fetch,
    ) -> Option<Result<Collected, tiberius::error::Error>> {
        if self.lost.is_some() {
            return None;
        }
        let (runtime, client, seconds) = (&self.runtime, &mut self.client, self.timeout);
        let outcome = guarded(|| {
            runtime.block_on(async {
                let past_limit = std::cell::Cell::new(false);
                let collect = collect(client, sql, fetch, &past_limit);
                match seconds {
                    0 => Ok(collect.await),
                    seconds => {
                        tokio::pin!(collect);
                        tokio::select! {
                            collected = &mut collect => Ok(collected),
                            () = tokio::time::sleep(Duration::from_secs(seconds.into())) => {
                                match past_limit.get() {
                                    true => Ok(collect.await),
                                    false => Err(()),
                                }
                            }
                        }
                    }
                }
            })
        });
        match outcome {
            Ok(Ok(result)) => Some(result),
            Ok(Err(_)) => {
                self.lost = Some(Lost::TimedOut);
                None
            }
            Err(error) => {
                self.lost = Some(Lost::Panicked(error.message));
                None
            }
        }
    }
}

/// Who wrote a statement, which decides what runs around it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Typed by the user: run as typed, under whatever the user has `SET`.
    User,
    /// Written by dbdelve at the user's ask, a relation tab's preview or an
    /// edit: run under [`SESSION_OPTIONS`], and cancellable.
    Generated,
    /// dbdelve's catalog and structure queries: under [`SESSION_OPTIONS`],
    /// never editable, and not what Cancel is for.
    Internal,
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
    /// A statement Cancel is for holds the session. A catalog load does not
    /// count: Cancel stops the user's statement, never the explorer's.
    running: bool,
    stopped: bool,
    /// Statements Cancel is for, waiting for the session.
    queued: usize,
    /// A Cancel that arrived while one was waiting, carried out when it gets
    /// the session.
    cancel_queued: bool,
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
    /// What every login dials, reconnects included, and held so ssh runs as
    /// long as any clone does.
    tunnel: Option<Arc<Tunnel>>,
    /// Opened for one statement ([`Self::alongside`]): a stop closes it for
    /// good rather than reconnecting, and has no session of the user's to
    /// report lost.
    alongside: bool,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        tunnelled(server, DEFAULT_PORT, |tunnel| {
            let mut connection = Self {
                session: Arc::new(Mutex::new(None)),
                in_flight: Arc::new(Mutex::new(InFlight::default())),
                server: server.clone(),
                tunnel,
                alongside: false,
            };
            let mut session = connection.connect()?;
            if connection.server.database.is_empty() {
                // The login's default database, named so that `held_to_database`
                // and a reconnect return to it rather than to "".
                connection.server.database = current_database(&mut session).map_err(|error| {
                    match error.message.is_empty() {
                        true => plain_error(
                            "The connection closed while asking which database the login landed \
                             in."
                            .into(),
                        ),
                        false => error,
                    }
                })?;
            }
            *connection.session.lock().expect("unshared until returned") = Some(session);
            Ok(connection)
        })
    }

    /// `prefer` and `disable` are the rungs that may reach less than their
    /// first attempt, and reaching it is what the words mean: a second attempt
    /// that negotiates nothing, for a server that cannot encrypt at all.
    fn connect(&self) -> Result<Session, DbError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| plain_error(format!("Could not start the connection: {error}")))?;

        let dial = self.tunnel.as_deref().map(Tunnel::dial).transpose()?;
        let attempt =
            |encryption| guarded(|| runtime.block_on(login(&self.server, dial, encryption)));
        let (client, socket) = match attempt(encryption(self.server.sslmode))? {
            Err(error)
                if matches!(self.server.sslmode, SslMode::Prefer | SslMode::Disable)
                    && negotiation_failed(&error) =>
            {
                attempt(EncryptionLevel::NotSupported)?
            }
            other => other,
        }
        .map_err(|error| connect_error(&error, &self.server))?;

        self.in_flight().socket = Some(socket);
        Ok(Session {
            runtime,
            client,
            timeout: self.server.statement_timeout,
            lost: None,
        })
    }

    /// A second session, for a statement to run on beside this one's; see
    /// [`super::Connection::query_alongside`]. It logs in on its first run,
    /// as a reconnect does.
    pub fn alongside(&self) -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
            in_flight: Arc::default(),
            alongside: true,
            ..self.clone()
        }
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
            // Waiting behind a catalog load: stopping the load would let the
            // statement run anyway.
            in_flight.cancel_queued = in_flight.queued > 0;
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

    /// Run one statement verbatim, keeping what `fetch` asks for of its rows;
    /// see [`super::Connection::query`].
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. A
    /// lone read is stopped at the limit by `SET ROWCOUNT`, sent as a batch of
    /// its own before it and taken back after it. Anything else has the rows
    /// past the limit read and dropped: the only other stop the driver has is
    /// closing the connection, which takes the user's transaction and
    /// temporary tables with it.
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, Origin::User, fetch)
    }

    /// A statement dbdelve wrote at the user's ask, run under the session
    /// options it was written for.
    pub fn generated(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, Origin::Generated, fetch)
    }

    /// dbdelve's own SQL. Its rows are never editable, so it does not pay for
    /// the describe that would say where they came from.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, Origin::Internal, Fetch::default())
    }

    fn run(&self, sql: &str, origin: Origin, fetch: Fetch) -> Result<QueryResult, DbError> {
        let cancellable = origin != Origin::Internal;
        if cancellable {
            self.in_flight().queued += 1;
        }
        let mut locked = self.session.lock();
        // Still queued while it reconnects, which can take the whole connect
        // bound, so a Cancel meanwhile is kept for the check below rather than
        // finding nothing running to stop.
        let connected = match &mut locked {
            Ok(guard) if guard.is_none() => self.connect().map(|session| **guard = Some(session)),
            _ => Ok(()),
        };
        let cancelled = cancellable && {
            let mut in_flight = self.in_flight();
            in_flight.queued -= 1;
            std::mem::take(&mut in_flight.cancel_queued)
        };
        let mut guard = locked.map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        if cancelled {
            return Err(plain_error(
                "Cancelled before it started: nothing was sent to the server.".into(),
            ));
        }
        connected?;
        let session = guard.as_mut().expect("connected above");

        if cancellable {
            let mut in_flight = self.in_flight();
            in_flight.running = true;
            in_flight.stopped = false;
        }
        let ran = self.execute(session, sql, origin, fetch);
        let stopped = {
            let mut in_flight = self.in_flight();
            in_flight.running = false;
            std::mem::take(&mut in_flight.stopped)
        };

        let limit = self.server.statement_timeout;
        let lost = session.lost.take();
        if stopped || lost.is_some() {
            // The server has no statement timeout, so the timer is dbdelve's,
            // and what it stops has to be stopped the way Cancel stops it.
            let what = match (ran, lost) {
                (None, Some(Lost::TimedOut)) => format!(
                    "The statement ran past the {limit}-second statement timeout and was stopped \
                     by closing its connection."
                ),
                (None, Some(Lost::Panicked(message))) => message,
                (None, Some(Lost::Capped)) => UNCAPPED.into(),
                (None, None) => CANCELLED.into(),
                // What dbdelve asks after the statement is what was stopped,
                // and the statement's own work stands.
                (Some(ran), lost) => {
                    let after = match lost {
                        Some(Lost::TimedOut) => format!(
                            "a query dbdelve sends after it ran past the {limit}-second statement \
                             timeout."
                        ),
                        Some(Lost::Panicked(message)) => format!("then {message}"),
                        Some(Lost::Capped) => UNCAPPED.into(),
                        None => "Cancel arrived after that.".into(),
                    };
                    match ran.result {
                        // Whole, so it lands as a result rather than as an
                        // error or as rows a cancel cut short, and the reset
                        // is said beside it.
                        Ok(mut result) => {
                            let reset = self
                                .stop(&mut guard, format!("The statement finished, but {after}"));
                            result.notice = Some(reset.message);
                            return Ok(result);
                        }
                        Err(error) => {
                            format!("{}\n\nThe statement failed, but {after}", error.message)
                        }
                    }
                }
            };
            return Err(self.stop(&mut guard, what));
        }

        let Some(Ran { result, probed }) = ran else {
            unreachable!("a run that did not finish was stopped or lost the session")
        };
        let mut result = result?;
        if !probed.is_empty() {
            drop(guard);
            result.edit = self.edit_target(&probed);
        }
        Ok(result)
    }

    /// The statement and every round trip that belongs to it, all inside the
    /// one window Cancel and the statement timeout reach. `None` when the
    /// statement itself did not finish.
    fn execute(
        &self,
        session: &mut Session,
        sql: &str,
        origin: Origin,
        fetch: Fetch,
    ) -> Option<Ran> {
        let mut statement = sql.to_string();
        let mut before = None;
        if origin != Origin::Internal
            && let Some(Ok(preflight)) = session.trip(&describing(
                &PREFLIGHT_SQL.replace("{statement}", &Engine::SqlServer.quote_literal(sql)),
                origin,
            ))
        {
            let described = &preflight.result;
            before = described
                .rows
                .first()
                .and_then(|row| named(described, row, "open_transactions")?.parse().ok());
            let unreadable = unreadable_columns(described);
            if !unreadable.is_empty() {
                match readable_preview(sql, described).filter(|_| origin == Origin::Generated) {
                    Some(projected) => statement = projected,
                    None => {
                        return Some(Ran {
                            result: Err(plain_error(unreadable_error(&unreadable))),
                            probed: Vec::new(),
                        });
                    }
                }
            }
        }

        let submitted = match origin {
            Origin::User => statement.clone(),
            Origin::Generated | Origin::Internal => scoped(&statement),
        };
        // A lone read is stopped at the limit by the server: one row past it
        // says there were more, and nothing past that is sent. A batch of its
        // own rather than a prefix on the user's text, which would move every
        // line number the server reports. Never a write: `SET ROWCOUNT` caps
        // those too, and `sql::stoppable` admits none.
        let capped = fetch.limit.filter(|limit| {
            origin == Origin::User && fetch.reads_only && *limit <= MAX_ROWCOUNT_LIMIT
        });
        let capped = match capped {
            Some(limit) => match session.trip(&format!("SET ROWCOUNT {}", limit + 1))? {
                Ok(_) => Some(limit),
                // Nothing was set, so the rows past the limit are drained.
                Err(_) => None,
            },
            None => None,
        };
        // Timed from here, not from the call: one connection serialises a
        // profile's queries, and time spent waiting behind the catalog load is
        // not time the server spent on this statement.
        let started = Instant::now();
        let outcome = session.fetching(&submitted, fetch)?;
        let elapsed = started.elapsed();
        let mut result = match outcome {
            Ok(mut collected) => {
                // One submission, one duration: every set of a batch carries
                // the whole batch's time, because that is the only time the
                // protocol reports. The first set's is timed to its last kept
                // row when rows past the limit came after it: draining them
                // is not the wait for the rows on screen.
                collected.result.elapsed = match collected.filled {
                    Some(at) if collected.result.capped_from.is_some() => {
                        at.duration_since(started)
                    }
                    _ => elapsed,
                };
                for set in &mut collected.result.rest {
                    set.elapsed = elapsed;
                }
                Ok(collected)
            }
            Err(error @ tiberius::error::Error::Server(_)) => Err(query_error(&error, sql)),
            // The socket Cancel shut down, rather than anything the server said.
            Err(_) if self.in_flight().stopped => return None,
            Err(error) => Err(query_error(&error, sql)),
        };
        if capped.is_some() {
            // Taken back whatever the statement did, before anything else is
            // asked. One that cannot be is a session that would cap every
            // statement after it, so it is closed instead: a fresh one starts
            // uncapped.
            let reset = matches!(session.trip("SET ROWCOUNT 0"), Some(Ok(_)));
            if !reset && session.lost.is_none() && !self.in_flight().stopped {
                session.lost = Some(Lost::Capped);
            }
            if let Ok(collected) = &mut result
                && collected.result.capped_from.take().is_some()
            {
                collected.result.stopped = Some(Stopped::AtLimit);
            }
        }
        if origin == Origin::Internal {
            return Some(Ran {
                result: result.map(|collected| collected.result),
                probed: Vec::new(),
            });
        }

        if let Err(error) = result {
            result = Err(transaction_outcome(session, sql, origin, before, error));
        }
        // After a `SET ROWCOUNT`, `@@ROWCOUNT` is that `SET`'s.
        if let Ok(collected) = &mut result
            && collected.sets == 0
            && capped.is_none()
        {
            // tiberius keeps the done tokens to itself, so the count is asked
            // for. It is the last statement's, as Postgres reports it.
            collected.result.rows_affected = session
                .trip("SELECT @@ROWCOUNT")
                .and_then(Result::ok)
                .and_then(|count| count.result.rows.first()?.first()?.clone()?.parse().ok());
        }
        // After a failure too: a batch that moved the session and then failed
        // leaves it moved all the same.
        if mentions_use(sql)
            && let Err(moved) = held_to_database(session, &self.server.database)
            && session.lost.is_none()
        {
            result = Err(match result {
                Ok(_) => moved,
                Err(error) => DbError {
                    message: format!("{}\n\n{}", error.message, moved.message),
                    position: error.position,
                },
            });
        }

        let (result, probed) = match result {
            Ok(collected) => {
                let rows = collected.result;
                let probed = if collected.sets == 1 && !rows.columns.is_empty() {
                    describe_columns(session, &statement, &rows.columns, origin)
                } else {
                    Vec::new()
                };
                (Ok(rows), probed)
            }
            Err(error) => (Err(error), Vec::new()),
        };
        Some(Ran { result, probed })
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
        if self.alongside {
            return plain_error(what);
        }
        let lost = "The connection was reset: the server rolled back any transaction that was \
                    open, and temporary tables and SET options are gone with the session.";
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

    pub fn databases(&self) -> Result<super::Databases, DbError> {
        let listed = self.internal_query(DATABASES_SQL)?;
        assemble_databases(&listed)
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

    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        assemble_references(&self.internal_query(&structure_sql(
            REFERENCES_SQL,
            schema,
            relation,
        ))?)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        self.structure_for(schema, relation, false)
    }

    fn structure_for(&self, schema: &str, relation: &str, ddl: bool) -> Result<Structure, DbError> {
        let (column, ddl) = match ddl {
            true => ("QUOTENAME(c.name, '\"')", "1"),
            false => ("c.name", "0"),
        };
        let query = |template: &str| {
            self.internal_query(&structure_sql(
                &template
                    .replace("{type}", TYPE_SQL)
                    .replace("{column}", column)
                    .replace("{ddl}", ddl),
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

    /// A view's text is the server's, as it was written. A table's is written
    /// back out of its structure, whose index text stops short of a statement
    /// (`UNIQUE NONCLUSTERED INDEX (a) INCLUDE (b)`) and so has its name and
    /// table put in after `INDEX`.
    ///
    /// ponytail: a disabled or untrusted foreign key is written as an ordinary
    /// one, and an XML or spatial index's options are not written; a `NOCHECK`
    /// statement per such key and reading `sys.xml_indexes` /
    /// `sys.spatial_index_tessellations` are the upgrade paths.
    pub fn ddl(&self, schema: &str, relation: &str, kind: RelationKind) -> Result<String, DbError> {
        let name = Engine::SqlServer.qualified(schema, relation);
        if let RelationKind::View | RelationKind::MaterializedView = kind {
            let result = self.internal_query(&structure_sql(
                "SELECT OBJECT_DEFINITION({object}) AS definition",
                schema,
                relation,
            ))?;
            return result
                .rows
                .first()
                .and_then(|row| row.first()?.as_deref())
                .map(terminated)
                .ok_or_else(|| plain_error(format!("{name} has no definition to show.")));
        }

        let structure = self.structure_for(schema, relation, true)?;
        // Every SQL Server table has a column, so none means no such table.
        if structure.columns.is_empty() {
            return Err(plain_error(format!("{schema} has no relation {relation}.")));
        }
        Ok(create_table(
            Engine::SqlServer,
            &format!("CREATE TABLE {name}"),
            &structure,
            "",
            |index| match index.definition.split_once("INDEX") {
                Some((kind, rest)) => format!(
                    "CREATE {kind}INDEX {} ON {name}{rest}",
                    Engine::SqlServer.quote_identifier(&index.name)
                ),
                None => index.definition.clone(),
            },
        ))
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
/// on the socket for Cancel. `dial` is a tunnel's address, dialled in place of
/// the server's while TLS still verifies the server's name.
async fn login(
    server: &ServerConfig,
    dial: Option<SocketAddr>,
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
            let tcp = match dial {
                Some(dial) => TcpStream::connect(dial).await?,
                None => TcpStream::connect(config.get_addr()).await?,
            };
            tcp.set_nodelay(true)?;
            let tcp = tcp.into_std()?;
            let socket = tcp.try_clone()?;
            let tcp = TcpStream::from_std(tcp)?;
            let mut client = match Client::connect(config, tcp.compat_write()).await {
                // Not through a tunnel, which reaches only the server it was
                // opened to; the error names where the login was sent.
                Err(tiberius::error::Error::Routing { host, port })
                    if !redirected && dial.is_none() =>
                {
                    redirected = true;
                    server.host = host;
                    server.port = Some(port);
                    continue;
                }
                other => other?,
            };
            // A session default, like the other engines' statement timeouts,
            // for the user's statements; dbdelve's own assert it again, since
            // the user can `SET` it off (see `transaction_outcome`).
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

/// What one submission returned: its first result set, with any others behind
/// it in `result.rest`, and how many there were.
struct Collected {
    result: QueryResult,
    sets: usize,
    /// When the first set's kept rows reached the limit.
    filled: Option<Instant>,
}

/// A statement that finished, one way or the other, and where its columns
/// came from when that is worth asking the catalog about.
struct Ran {
    result: Result<QueryResult, DbError>,
    probed: Vec<ProbedColumn>,
}

/// A column of a result dbdelve asked for, by name, null or absent alike.
fn named<'a>(result: &QueryResult, row: &'a [Cell], name: &str) -> Option<&'a str> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)?;
    row.get(index)?.as_deref()
}

/// dbdelve's own SQL, run inside `sp_executesql` behind [`SESSION_OPTIONS`]: a
/// `SET` there lasts until the dynamic batch returns, so whatever the user set
/// is theirs again for their next statement.
fn scoped(sql: &str) -> String {
    scoped_under(SESSION_OPTIONS, sql)
}

fn scoped_under(options: &str, sql: &str) -> String {
    format!(
        "EXEC sp_executesql {}",
        Engine::SqlServer.quote_literal(&format!("{options}{sql}"))
    )
}

/// A question dbdelve asks about a statement, compiling it under the options
/// that statement runs under: a user's own, where `SET QUOTED_IDENTIFIER OFF`
/// makes `"position"` a string rather than a column.
fn describing(sql: &str, origin: Origin) -> String {
    match origin {
        Origin::User => scoped_under(USER_DESCRIBE_OPTIONS, sql),
        Origin::Generated | Origin::Internal => scoped(sql),
    }
}

/// The preflight's columns tiberius cannot decode: its `todo!()`s for
/// sql_variant (98) and every CLR type (240), geography, geometry and
/// hierarchyid among them.
fn unreadable_columns(described: &QueryResult) -> Vec<(usize, String, String)> {
    described
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(named(described, row, "system_type_id"), Some("98" | "240")))
        .map(|(index, row)| {
            (
                index,
                named(described, row, "column_name")
                    .unwrap_or_default()
                    .to_string(),
                named(described, row, "system_type_name")
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn unreadable_error(columns: &[(usize, String, String)]) -> String {
    let named = columns
        .iter()
        .map(|(index, name, kind)| match name.is_empty() {
            true => format!("column {} ({kind})", index + 1),
            false => format!("{name} ({kind})"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The statement was not run: its result would hold {named}, which the SQL Server driver \
         cannot read. A CLR type such as geography reads as text through .ToString(), and \
         sql_variant through CAST(… AS nvarchar(4000))."
    )
}

/// A relation tab's `SELECT *` with every column tiberius cannot decode read
/// as text, under its own name. The expressions have no source column, so the
/// describe leaves them uneditable.
///
/// ponytail: rewrites only the one shape `explorer::preview_sql` writes; a
/// user's own statement is refused instead, never rewritten (hard rule 1).
///
/// The rewrite is what is sent, so it passes `sql::is_generated_select` again
/// (read back through `sql::unpaged`, as the preview was read before paging)
/// rather than riding on the approval the preview it replaced was given (hard
/// rule 2); one that does not is `None`, and the preview is refused.
fn readable_preview(sql: &str, described: &QueryResult) -> Option<String> {
    let rest = sql.strip_prefix("SELECT * FROM ")?;
    let columns = described
        .rows
        .iter()
        .map(|row| {
            let name = Engine::SqlServer.quote_identifier(named(described, row, "column_name")?);
            Some(match named(described, row, "system_type_id")? {
                // Not `.ToString()` or `CAST(… AS nvarchar(max))`, which the
                // gate's grammar cannot read.
                "240" => format!("CONVERT(nvarchar(max), {name}) AS {name}"),
                "98" => format!("CAST({name} AS nvarchar(4000)) AS {name}"),
                _ => name,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("SELECT {} FROM {rest}", columns.join(", "))).filter(|projected| {
        crate::sql::is_generated_select(super::Engine::SqlServer, &crate::sql::unpaged(projected))
    })
}

/// Every set of a submission, each kept to the limit and counted past it. The
/// first set alone is fed, since it is the one the grid shows; the sets behind
/// it are results of their own, kept here until the batch is done.
///
/// `past_limit` is set once any set's kept rows reach the limit.
async fn collect(
    client: &mut Tds,
    sql: &str,
    fetch: Fetch<'_>,
    past_limit: &std::cell::Cell<bool>,
) -> Result<Collected, tiberius::error::Error> {
    let mut stream = client.simple_query(sql).await?;
    let mut collected: Vec<QueryResult> = Vec::new();
    let mut types = Vec::new();
    // The current set's rows, kept or not, and kept: a fed row is not in its
    // `rows` to be counted.
    let (mut returned, mut kept) = (0, 0);
    let mut filled = None;

    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(meta) => {
                cap(collected.last_mut(), returned, kept);
                (returned, kept) = (0, 0);
                types = meta.columns().iter().map(|c| c.column_type()).collect();
                let columns: Vec<Column> = meta
                    .columns()
                    .iter()
                    .map(|column| Column {
                        name: column.name().to_string(),
                        data_type: Some(type_name(column.column_type()).to_string()),
                    })
                    .collect();
                if collected.is_empty()
                    && let Some(feed) = fetch.feed
                {
                    feed.begin(columns.clone());
                }
                open_set(&mut collected, columns);
            }
            QueryItem::Row(row) => {
                let fed = collected.len() == 1;
                let Some(result) = collected.last_mut() else {
                    continue;
                };
                returned += 1;
                if fetch.limit.is_some_and(|limit| kept >= limit) {
                    continue;
                }
                kept += 1;
                if fetch.limit == Some(kept) {
                    past_limit.set(true);
                }
                if fed && fetch.limit == Some(kept) {
                    filled = Some(Instant::now());
                    if let Some(feed) = fetch.feed {
                        feed.fill();
                    }
                }
                // The wire names `datetime` and `smalldatetime` alike; the
                // value says which.
                let mut retyped = false;
                for (column, (_, value)) in result.columns.iter_mut().zip(row.cells()) {
                    if let ColumnData::SmallDateTime(Some(_)) = value
                        && column.data_type.as_deref() != Some("smalldatetime")
                    {
                        column.data_type = Some("smalldatetime".into());
                        retyped = true;
                    }
                }
                let cells: Vec<Cell> = row
                    .into_iter()
                    .zip(&types)
                    .map(|(value, kind)| render(&value, *kind))
                    .collect();
                result.bytes += cells.iter().flatten().map(String::len).sum::<usize>();
                match fetch.feed.filter(|_| fed) {
                    Some(feed) => {
                        if retyped {
                            feed.retype(result.columns.clone());
                        }
                        feed.push(cells);
                    }
                    None => result.rows.push(cells),
                }
            }
        }
    }
    cap(collected.last_mut(), returned, kept);

    let mut collected = collected_sets(collected);
    collected.filled = filled;
    Ok(collected)
}

/// A set's own count, when the limit kept fewer of its rows than it had.
fn cap(set: Option<&mut QueryResult>, returned: usize, kept: usize) {
    if let Some(set) = set {
        set.capped_from = (returned > kept).then_some(returned);
        set.rows_affected = Some(returned as u64);
    }
}

/// A description opens a result set of its own, where it once started the one
/// kept set over and the last statement of a batch won. Split out of `collect`,
/// with [`collected_sets`], because that is the decision worth a check and
/// `QueryItem` cannot be built outside tiberius.
fn open_set(sets: &mut Vec<QueryResult>, columns: Vec<Column>) {
    sets.push(QueryResult {
        columns,
        ..QueryResult::default()
    });
}

/// One submission's sets as the first of them carrying the rest.
fn collected_sets(mut sets: Vec<QueryResult>) -> Collected {
    // A query's count is the rows it returned, per set, which `cap` has
    // written where it saw the rows. A write's is in the protocol's done
    // tokens, which tiberius keeps to itself, so `execute` asks for it.
    for set in &mut sets {
        set.rows_affected.get_or_insert(set.rows.len() as u64);
    }
    let count = sets.len();
    let mut sets = sets.into_iter();
    let mut result = sets.next().unwrap_or_default();
    result.rest = sets.collect();
    Collected {
        result,
        sets: count,
        filled: None,
    }
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
        ColumnData::F32(value) => float(*value.as_ref()?),
        ColumnData::F64(value) if matches!(kind, ColumnType::Money | ColumnType::Money4) => {
            format!("{:.4}", value.as_ref()?)
        }
        ColumnData::F64(value) => float(*value.as_ref()?),
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

/// The shortest digits that read back as the same value, in exponent form at
/// the magnitudes where `Display` would spell out hundreds of zeros. `1E+308`,
/// the way the server prints it, is also a literal T-SQL reads.
fn float<T: Copy + Into<f64> + std::fmt::Display + std::fmt::UpperExp>(value: T) -> String {
    let magnitude = value.into().abs();
    if magnitude != 0.0 && !(1e-4..1e15).contains(&magnitude) {
        let text = format!("{value:E}");
        match text.contains("E-") {
            true => text,
            false => text.replacen('E', "E+", 1),
        }
    } else {
        value.to_string()
    }
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
    /// Whether the server would accept a positioned `UPDATE` of this column:
    /// false for a computed, identity or rowversion column. It still names its
    /// row's key when it is one -- see [`resolve_edit_target`].
    writable: bool,
}

/// What each column the statement returned was read from, in order.
///
/// Asked after the statement ran, and only of a submission that returned one
/// result set: the describe answers for the *first* result set, which is the
/// one the grid shows only when there is one. It compiles the statement and
/// executes nothing. A column list that disagrees with what came back is
/// nothing known, since everything here is positional.
fn describe_columns(
    session: &mut Session,
    sql: &str,
    columns: &[Column],
    origin: Origin,
) -> Vec<ProbedColumn> {
    let describe = DESCRIBE_SQL.replace("{statement}", &Engine::SqlServer.quote_literal(sql));
    let Some(Ok(collected)) = session.trip(&describing(&describe, origin)) else {
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
                writable: cell(row, "is_updateable").as_deref() == Some("1"),
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
        // A computed, identity or rowversion column is never a `SET` target --
        // the server refuses the `UPDATE` -- but one still names its row's key
        // above, so it is only cleared here, once `keys` no longer needs it.
        columns: probed
            .iter()
            .enumerate()
            .map(|(index, column)| {
                column_of(column).filter(|_| column.writable || keys.contains(&index))
            })
            .collect(),
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
        // Inside a procedure, trigger or function the line is the module's,
        // which says nothing about the batch; the message names the module.
        position: match error {
            tiberius::error::Error::Server(token) if token.procedure().is_empty() => {
                line_start(sql, token.line())
            }
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

fn ask(session: &mut Session, statement: &str) -> Result<Collected, DbError> {
    match session.trip(statement) {
        Some(result) => result.map_err(|error| plain_error(describe(&error))),
        // The session is lost, which `run` reports in place of this.
        None => Err(plain_error(String::new())),
    }
}

fn current_database(session: &mut Session) -> Result<String, DbError> {
    Ok(ask(session, "SELECT DB_NAME()")?
        .result
        .rows
        .first()
        .and_then(|row| row.first()?.clone())
        .unwrap_or_default())
}

fn held_to_database(session: &mut Session, database: &str) -> Result<(), DbError> {
    let current = current_database(session)?;
    if current.eq_ignore_ascii_case(database) {
        return Ok(());
    }
    ask(session, &format!("USE [{}]", database.replace(']', "]]")))?;
    Err(plain_error(format!(
        "The statement moved the session to database {current}, and dbdelve moved it back to \
         {database}: this connection's explorer, and every statement dbdelve writes, name \
         {database}'s objects."
    )))
}

/// Say what a failed statement did to the transactions open around it.
///
/// `SET XACT_ABORT ON` is what makes a generated batch's brackets atomic:
/// without it a constraint violation ends only its own statement, the batch
/// carries on, and the `COMMIT` dbdelve wrote commits the rows before it. It is
/// the session's default too, so a failing statement of the user's ends a
/// transaction they began earlier. T-SQL has no nested transactions, so either
/// takes every open level with it -- which the counts before and after tell.
fn transaction_outcome(
    session: &mut Session,
    sql: &str,
    origin: Origin,
    before: Option<u64>,
    error: DbError,
) -> DbError {
    let Some(before) = before else {
        return error;
    };
    let mut ask = |statement: &str| session.trip(statement).and_then(Result::ok);
    let Some(after) = ask("SELECT @@TRANCOUNT").and_then(|collected| {
        collected
            .result
            .rows
            .first()?
            .first()?
            .as_deref()?
            .parse::<u64>()
            .ok()
    }) else {
        return error;
    };
    // Only a batch dbdelve wrote is known to end at its own `COMMIT`.
    let bracketed = origin == Origin::Generated
        && Engine::SqlServer.transaction_start().is_some_and(|start| {
            sql.trim_start()
                .get(..start.len())
                .is_some_and(|word| word.eq_ignore_ascii_case(start))
        });
    let outcome = if after > before {
        // A `ROLLBACK` takes every level, so it is sent only when the batch
        // opened all of them.
        match bracketed && before == 0 {
            true => match ask("ROLLBACK") {
                Some(_) => "The transaction was rolled back: nothing the batch wrote remains.",
                None => "The transaction the batch opened is still open: the rollback failed too.",
            },
            false => "The transaction the batch began is still open.",
        }
    } else if after == 0 && before > 0 {
        "The transaction that was open before this statement was rolled back, and everything \
         written in it is gone."
    } else if bracketed {
        match before {
            0 => "The transaction did not commit: nothing the batch wrote remains.",
            _ => "Nothing the batch wrote remains, and the transaction open before it still is.",
        }
    } else {
        return error;
    };
    DbError {
        message: format!("{}\n\n{outcome}", error.message),
        position: error.position,
    }
}

/// Whether a failed connect may have failed at the encryption handshake, the
/// one failure a second attempt without encryption can get past. A refused
/// login or an unreachable host would fail the same way twice, and a second
/// failed login counts against a lockout policy.
fn negotiation_failed(error: &tiberius::error::Error) -> bool {
    match error {
        tiberius::error::Error::Tls(_) => true,
        tiberius::error::Error::Io { kind, .. } => !matches!(
            kind,
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, ForeignKey, RelationKind, RoutineKind};
    use crate::filter::{FilterBar, Operator, derived_filter, filter_predicate, relation_sql};
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

    /// The compose server as the bastion sees it, by service name and the port
    /// inside the network.
    fn live_tunnelled() -> ServerConfig {
        ServerConfig {
            host: "mssql".into(),
            port: Some(1433),
            ssh: crate::db::ssh::live_bastion("dbdelve-bastion"),
            ..live_config()
        }
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
                writable: true,
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
                ssh: None,
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
    fn a_float_far_from_one_renders_in_exponent_form() {
        let text = |value: ColumnData<'static>| super::render(&value, ColumnType::Floatn).unwrap();
        assert_eq!(text(ColumnData::F64(Some(1.5))), "1.5");
        assert_eq!(text(ColumnData::F64(Some(3.0))), "3");
        assert_eq!(text(ColumnData::F64(Some(0.0))), "0");
        assert_eq!(text(ColumnData::F64(Some(-0.25))), "-0.25");
        assert_eq!(text(ColumnData::F64(Some(123_456.789))), "123456.789");
        assert_eq!(text(ColumnData::F64(Some(1e308))), "1E+308");
        assert_eq!(text(ColumnData::F64(Some(-1.5e15))), "-1.5E+15");
        assert_eq!(text(ColumnData::F64(Some(1e-300))), "1E-300");
        assert_eq!(text(ColumnData::F64(Some(2.5e-5))), "2.5E-5");
        assert_eq!(text(ColumnData::F32(Some(3.4e38))), "3.4E+38");
        assert_eq!(text(ColumnData::F32(Some(0.1))), "0.1");
        for value in [1e308, -1.5e15, 1e-300, 2.5e-5, f64::MIN_POSITIVE, f64::MAX] {
            let shown = text(ColumnData::F64(Some(value)));
            assert_eq!(shown.parse::<f64>(), Ok(value), "{shown}");
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
    fn only_a_failed_handshake_is_retried_without_encryption() {
        use std::io::ErrorKind;
        use tiberius::error::Error;
        let io = |kind| Error::Io {
            kind,
            message: String::new(),
        };
        assert!(negotiation_failed(&Error::Tls("handshake".into())));
        assert!(negotiation_failed(&io(ErrorKind::UnexpectedEof)));
        assert!(!negotiation_failed(&io(ErrorKind::ConnectionRefused)));
        assert!(!negotiation_failed(&io(ErrorKind::TimedOut)));
        assert!(!negotiation_failed(&Error::Protocol("login".into())));
    }

    #[test]
    fn every_result_set_of_a_batch_is_kept_in_the_order_the_batch_returned_them() {
        let mut sets = Vec::new();
        open_set(&mut sets, vec![named_column("a")]);
        sets.last_mut().unwrap().rows.push(vec![Some("1".into())]);
        open_set(&mut sets, vec![named_column("b")]);
        sets.last_mut().unwrap().rows.push(vec![Some("2".into())]);
        sets.last_mut().unwrap().rows.push(vec![Some("3".into())]);

        let collected = collected_sets(sets);
        assert_eq!(collected.sets, 2);
        assert_eq!(collected.result.columns[0].name, "a");
        assert_eq!(collected.result.rows_affected, Some(1));
        assert_eq!(collected.result.rest.len(), 1);
        let second = &collected.result.rest[0];
        assert_eq!(second.columns[0].name, "b");
        assert_eq!(second.rows.len(), 2);
        assert_eq!(second.rows_affected, Some(2));
        // One level deep, never a tree.
        assert!(second.rest.is_empty());
    }

    fn named_column(name: &str) -> Column {
        Column {
            name: name.into(),
            data_type: None,
        }
    }

    #[test]
    fn a_preview_reads_unreadable_columns_as_text_and_nothing_else_is_rewritten() {
        let described = QueryResult {
            columns: ["column_name", "system_type_id"]
                .map(|name| Column {
                    name: name.into(),
                    data_type: None,
                })
                .to_vec(),
            rows: [("id", "56"), ("place", "240"), ("extra", "98")]
                .map(|(name, kind)| vec![Some(name.to_string()), Some(kind.to_string())])
                .to_vec(),
            ..QueryResult::default()
        };
        assert_eq!(
            readable_preview("SELECT * FROM \"dbo\".\"t\" ORDER BY 1", &described).as_deref(),
            Some(
                "SELECT \"id\", CONVERT(nvarchar(max), \"place\") AS \"place\", \
                 CAST(\"extra\" AS nvarchar(4000)) AS \"extra\" \
                 FROM \"dbo\".\"t\" ORDER BY 1"
            )
        );
        assert_eq!(readable_preview("SELECT id FROM t", &described), None);
        // What is sent is gated again, not only the preview it replaced.
        let paged = "SELECT * FROM \"dbo\".\"t\" ORDER BY (SELECT NULL), \"id\" \
                     OFFSET 0 ROWS FETCH NEXT 100 ROWS ONLY";
        assert!(readable_preview(paged, &described).is_some());
        let smuggled = "SELECT * FROM \"dbo\".\"t\"; DROP TABLE \"dbo\".\"t\"";
        assert_eq!(readable_preview(smuggled, &described), None);
        // The grammar does not read a doubled quote inside a name, so neither
        // does the gate: refused, as a preview of such a table already is.
        let mut odd = described.clone();
        odd.rows[2][0] = Some("any\"thing".into());
        assert_eq!(readable_preview(paged, &odd), None);
        assert_eq!(
            unreadable_columns(&described)
                .iter()
                .map(|(index, ..)| *index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn scoped_sql_keeps_the_statement_on_its_own_line_numbers() {
        assert!(!SESSION_OPTIONS.contains('\n'));
        assert_eq!(
            scoped("SELECT 'a'"),
            format!("EXEC sp_executesql N'{SESSION_OPTIONS}SELECT ''a'''")
        );
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
    fn a_computed_or_identity_column_is_not_a_set_target_but_still_locates_its_row() {
        // `id` is the key and not writable (an identity column); `total` is an
        // ordinary column; `label` is a computed column, never part of the key.
        let mut columns = probed(&[
            (Some("orders"), Some("id")),
            (Some("orders"), Some("total")),
            (Some("orders"), Some("label")),
        ]);
        columns[0].writable = false;
        columns[2].writable = false;

        let target = resolve_edit_target(&columns, "dbo", "orders", &["id".to_string()])
            .expect("the key is present");
        assert_eq!(target.keys, vec![0]);
        assert_eq!(
            target.columns[0],
            Some("id".to_string()),
            "a key still names its row"
        );
        assert_eq!(target.columns[1], Some("total".to_string()));
        assert_eq!(
            target.columns[2], None,
            "a computed column is never a SET target"
        );
    }

    #[test]
    #[ignore = "requires the dev bastions and SQL Server configured through dbdelve_SSH_CONFIG and dbdelve_MSSQL_URL"]
    fn live_ssh_a_query_runs_through_the_bastion_and_after_a_reconnect() {
        let connection = Connection::open(&live_tunnelled()).expect("connection should open");
        let result = connection
            .query("SELECT 1 AS one", Fetch::default())
            .expect("query should succeed");
        assert_eq!(result.rows, vec![vec![Some("1".into())]]);

        // Cancel closes the session, so the run reconnects: through the same
        // tunnel, or not at all.
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the cancel should send");
        });
        let error = connection
            .query("WAITFOR DELAY '00:00:30'", Fetch::default())
            .expect_err("the statement should be stopped");
        assert!(error.message.contains("reconnected"), "{}", error.message);
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_the_development_database_is_fully_seeded() {
        let result = live()
            .query(
                "SELECT count(*) AS rows_seeded FROM measurements",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(first(&result), vec![Some("5000")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_the_databases_list_flags_the_one_connected_to() {
        let databases = live().databases().expect("databases should list");

        assert!(databases.names.contains(&"dbdelve_dev".to_string()));
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_blank_database_holds_a_use_to_where_the_login_landed() {
        let connection = Connection::open(&ServerConfig {
            database: String::new(),
            ..live_config()
        })
        .expect("a blank database should connect");
        let landed = connection
            .databases()
            .unwrap()
            .current
            .expect("a login lands somewhere");

        let elsewhere = if landed == "master" {
            "tempdb"
        } else {
            "master"
        };
        let error = connection
            .query(&format!("USE {elsewhere}; SELECT 1"), Fetch::default())
            .expect_err("leaving the database the login landed in is reported");
        assert!(error.message.contains("moved it back"), "{}", error.message);
        let here = connection
            .query("SELECT DB_NAME()", Fetch::default())
            .unwrap();
        assert_eq!(first(&here), vec![Some(landed.as_str())]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_row_limit_keeps_each_sets_first_rows_and_counts_the_rest() {
        let connection = live();
        let feed = crate::db::Feed::default();
        let result = connection
            .query(
                "SELECT TOP 10 id FROM measurements ORDER BY id; \
                 SELECT TOP 5 id FROM measurements ORDER BY id",
                Fetch {
                    limit: Some(3),
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        let fed = feed.take();

        // The first set is the grid's, fed with its types as it arrives.
        assert!(result.rows.is_empty());
        assert_eq!(fed.rows.len(), 3);
        assert_eq!(fed.columns[0].data_type.as_deref(), Some("bigint"));
        assert_eq!(result.capped_from, Some(10));
        // The set behind it is a result of its own, kept and capped here.
        assert_eq!(result.rest.len(), 1);
        assert_eq!(result.rest[0].rows.len(), 3);
        assert_eq!(result.rest[0].capped_from, Some(5));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_query_round_trip() {
        let result = live()
            .query(
                "SELECT 1 AS id, N'alpha' AS label UNION ALL SELECT 2, NULL",
                Fetch::default(),
            )
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
    fn live_every_result_set_of_a_batch_comes_back_in_order() {
        let connection = live();
        // The batch is the unit here, so a selection holding two statements is
        // one submission -- and both its sets are kept, the first in the
        // result and the rest behind it, each becoming its own chip. Splitting
        // is what gets the other engines the same thing.
        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d", Fetch::default())
            .expect("query should succeed");
        assert_eq!(names(&result), vec!["a", "b"]);
        assert_eq!(result.rows, vec![vec![Some("1".into()), Some("2".into())]]);
        assert_eq!(result.rest.len(), 1);
        assert_eq!(names(&result.rest[0]), vec!["d"]);
        assert_eq!(result.rest[0].rows, vec![vec![Some("4".into())]]);
        // More than one result set, so the describe cannot say which it
        // described and none of this is editable.
        assert!(result.edit.is_none());

        let empty = connection
            .query(
                "SELECT 1 AS id, N'x' AS label WHERE 1 = 0",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id", "label"]);
        assert!(empty.rows.is_empty());
        assert!(empty.rest.is_empty());

        // A write returns no rows, and its count. No result set, so nothing
        // trails it either.
        let write = connection
            .query(
                "DECLARE @t TABLE (id int); INSERT INTO @t VALUES (1)",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert!(write.columns.is_empty());
        assert_eq!(write.rows_affected, Some(1));
        assert!(write.rest.is_empty());
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
                Fetch::default(),
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
                Fetch::default(),
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
        assert!(
            dbo.relations
                .iter()
                .any(|relation| { relation.name == "accounts" && relation.size.is_some() })
        );
        assert!(
            dbo.relations
                .iter()
                .any(|relation| { relation.name == "account_overview" && relation.size.is_none() })
        );
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
    fn live_the_ddl_of_a_table_and_a_view_reads_back_as_their_create_statements() {
        let connection = live();
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_ddl; \
                 CREATE TABLE dbdelve_test_ddl (id int NOT NULL, note nvarchar(40) NULL); \
                 CREATE INDEX dbdelve_test_ddl_note ON dbdelve_test_ddl (note DESC) INCLUDE (id)",
                Fetch::default(),
            )
            .expect("the fixture table should be created");
        let indexed = connection.ddl("dbo", "dbdelve_test_ddl", RelationKind::Table);
        connection
            .query("DROP TABLE dbdelve_test_ddl", Fetch::default())
            .expect("the fixture table should be cleaned up");
        let table = connection
            .ddl("dbo", "accounts", RelationKind::Table)
            .expect("the table's DDL should load");
        let view = connection
            .ddl("dbo", "account_overview", RelationKind::View)
            .expect("the view's DDL should load");

        assert!(
            table.starts_with(
                "CREATE TABLE \"dbo\".\"accounts\" (\n    \
                 \"id\" bigint IDENTITY(1,1) NOT NULL,"
            ),
            "{table}"
        );
        assert!(table.contains("PRIMARY KEY CLUSTERED (\"id\")"), "{table}");
        assert!(table.contains("CHECK ("), "{table}");
        assert_eq!(
            indexed.expect("the indexed table's DDL should load"),
            "CREATE TABLE \"dbo\".\"dbdelve_test_ddl\" (\n    \"id\" int NOT NULL,\n    \
             \"note\" nvarchar(40)\n);\nCREATE NONCLUSTERED INDEX \"dbdelve_test_ddl_note\" \
             ON \"dbo\".\"dbdelve_test_ddl\" (\"note\" DESC) INCLUDE (\"id\");"
        );
        assert!(
            view.starts_with("CREATE VIEW account_overview AS"),
            "{view}"
        );
        assert!(view.ends_with(';'), "{view}");
        let missing = |kind| {
            connection
                .ddl("dbo", "dbdelve_test_ddl_missing", kind)
                .unwrap_err()
                .message
        };
        assert_eq!(
            missing(RelationKind::Table),
            "dbo has no relation dbdelve_test_ddl_missing."
        );
        assert_eq!(
            missing(RelationKind::View),
            "\"dbo\".\"dbdelve_test_ddl_missing\" has no definition to show."
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_the_ddl_of_a_nonclustered_descending_key_and_quoted_columns_runs_again() {
        let connection = live();
        let ddl = || connection.ddl("dbo", "dbdelve_test_ddl_keys", RelationKind::Table);
        let drop = "DROP TABLE IF EXISTS dbdelve_test_ddl_keys";
        connection
            .query(
                &format!(
                    "{drop}; \
                 CREATE TABLE dbdelve_test_ddl_keys ( \
                     id int NOT NULL, \
                     [order] int NULL, \
                     [my col] int NULL, \
                     twice AS (id * 2) PERSISTED, \
                     CONSTRAINT dbdelve_test_ddl_keys_pk PRIMARY KEY NONCLUSTERED (id DESC), \
                     CONSTRAINT dbdelve_test_ddl_keys_uq UNIQUE ([my col]), \
                     CONSTRAINT dbdelve_test_ddl_keys_fk FOREIGN KEY ([my col]) \
                         REFERENCES dbdelve_test_ddl_keys (id)); \
                 CREATE CLUSTERED INDEX dbdelve_test_ddl_keys_order \
                     ON dbdelve_test_ddl_keys ([order])"
                ),
                Fetch::default(),
            )
            .expect("the fixture table should be created");
        // The Structure tab's key text is unchanged by what the DDL spells.
        let key = connection
            .structure("dbo", "dbdelve_test_ddl_keys")
            .map(|structure| structure.row_key());
        let written = ddl();
        let rerun = written.as_ref().map_err(Clone::clone).and_then(|written| {
            connection.query(drop, Fetch::default())?;
            connection.query(written, Fetch::default())?;
            ddl()
        });
        connection
            .query(drop, Fetch::default())
            .expect("the fixture table should be cleaned up");

        assert_eq!(key, Ok(vec!["id".to_string()]));
        let written = written.expect("the DDL should load");
        assert_eq!(rerun.as_ref(), Ok(&written));
        for expected in [
            "\"twice\" AS ([id]*(2)) PERSISTED,",
            "CONSTRAINT \"dbdelve_test_ddl_keys_pk\" PRIMARY KEY NONCLUSTERED (\"id\" DESC),",
            "CONSTRAINT \"dbdelve_test_ddl_keys_uq\" UNIQUE NONCLUSTERED (\"my col\")",
            "FOREIGN KEY (\"my col\") REFERENCES",
            "CREATE CLUSTERED INDEX \"dbdelve_test_ddl_keys_order\" ON \
             \"dbo\".\"dbdelve_test_ddl_keys\" (\"order\");",
        ] {
            assert!(written.contains(expected), "{expected} in {written}");
        }
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
                    REFERENCES \"dbo\".\"orders\" (\"account_id\", \"number\")"),
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
    fn live_a_low_privilege_login_reads_structure_without_a_null_type_or_definition() {
        let connection = live();
        // `EXECUTE AS USER` impersonates a user with none of the permissions a
        // real low-privilege login would lack, on the one connection already in
        // hand -- no server-level login to create and drop.
        connection
            .query(
                "IF EXISTS (SELECT 1 FROM sys.database_principals WHERE name = 'fix_cat_lowpriv')
                     DROP USER fix_cat_lowpriv;
                 IF OBJECT_ID('dbo.fix_cat_people') IS NOT NULL DROP TABLE dbo.fix_cat_people;",
                Fetch::default(),
            )
            .expect("stale scratch objects should drop");
        connection
            .query(
                "IF TYPE_ID('dbo.fix_cat_email') IS NOT NULL DROP TYPE dbo.fix_cat_email",
                Fetch::default(),
            )
            .expect("a stale scratch type should drop");
        connection
            .query(
                "CREATE TYPE dbo.fix_cat_email FROM NVARCHAR(50) NOT NULL",
                Fetch::default(),
            )
            .expect("the scratch type should be created");
        connection
            .query(
                "CREATE TABLE dbo.fix_cat_people (
                     id INT IDENTITY(1,1) PRIMARY KEY,
                     email dbo.fix_cat_email,
                     tag AS ('t' + CAST(id AS VARCHAR(10))) PERSISTED
                 );
                 CREATE USER fix_cat_lowpriv WITHOUT LOGIN;
                 GRANT SELECT ON dbo.fix_cat_people TO fix_cat_lowpriv;",
                Fetch::default(),
            )
            .expect("the scratch table and user should be created");

        connection
            .query("EXECUTE AS USER = 'fix_cat_lowpriv'", Fetch::default())
            .expect("impersonation should start");
        let structure = connection.structure("dbo", "fix_cat_people");
        connection
            .query("REVERT", Fetch::default())
            .expect("impersonation should end");
        let structure = structure.expect("structure should load despite the withheld permissions");

        let email = structure
            .columns
            .iter()
            .find(|column| column.name == "email")
            .expect("the email column should be reported");
        assert_eq!(
            email.data_type, "nvarchar",
            "an alias type the login cannot see falls back to its base type"
        );
        let tag = structure
            .columns
            .iter()
            .find(|column| column.name == "tag")
            .expect("the tag column should be reported");
        assert_eq!(tag.default.as_deref(), Some("AS <hidden> PERSISTED"));

        connection
            .query(
                "DROP USER fix_cat_lowpriv;
                 DROP TABLE dbo.fix_cat_people;",
                Fetch::default(),
            )
            .expect("cleanup should succeed");
        connection
            .query("DROP TYPE dbo.fix_cat_email", Fetch::default())
            .expect("cleanup should succeed");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_indexes_show_included_columns_a_filter_and_columnstore_indexes() {
        let connection = live();
        connection
            .query(
                "IF OBJECT_ID('dbo.fix_cat_indexed') IS NOT NULL DROP TABLE dbo.fix_cat_indexed;
                 IF OBJECT_ID('dbo.fix_cat_columnstore') IS NOT NULL
                     DROP TABLE dbo.fix_cat_columnstore;",
                Fetch::default(),
            )
            .expect("stale scratch tables should drop");
        connection
            .query(
                "CREATE TABLE dbo.fix_cat_indexed (id INT PRIMARY KEY, a INT, b INT, c INT);
                 CREATE UNIQUE NONCLUSTERED INDEX fix_cat_ix_filtered
                     ON dbo.fix_cat_indexed (a) INCLUDE (b, c) WHERE a IS NOT NULL;
                 CREATE NONCLUSTERED COLUMNSTORE INDEX fix_cat_ix_ncc
                     ON dbo.fix_cat_indexed (a, b, c);
                 CREATE TABLE dbo.fix_cat_columnstore (x INT NOT NULL, y INT NOT NULL);
                 CREATE CLUSTERED COLUMNSTORE INDEX fix_cat_ix_ccs ON dbo.fix_cat_columnstore;",
                Fetch::default(),
            )
            .expect("the scratch indexes should be created");

        let indexed = connection
            .structure("dbo", "fix_cat_indexed")
            .expect("structure should load");
        assert!(
            indexed
                .indexes
                .iter()
                .any(|index| index.name == "fix_cat_ix_filtered"
                    && index.definition
                        == "UNIQUE NONCLUSTERED INDEX (a) INCLUDE (b, c) WHERE ([a] IS NOT NULL)"),
            "{:?}",
            indexed.indexes
        );
        assert!(
            indexed
                .indexes
                .iter()
                .any(|index| index.name == "fix_cat_ix_ncc"
                    && index.definition == "NONCLUSTERED COLUMNSTORE INDEX (a, b, c)"),
            "{:?}",
            indexed.indexes
        );

        let columnstore = connection
            .structure("dbo", "fix_cat_columnstore")
            .expect("structure should load");
        assert!(
            columnstore
                .indexes
                .iter()
                .any(|index| index.name == "fix_cat_ix_ccs"
                    && index.definition == "CLUSTERED COLUMNSTORE INDEX"),
            "{:?}",
            columnstore.indexes
        );

        connection
            .query(
                "DROP TABLE dbo.fix_cat_indexed;
                 DROP TABLE dbo.fix_cat_columnstore;",
                Fetch::default(),
            )
            .expect("cleanup should succeed");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_foreign_keys_show_actions_and_disabled_state() {
        let connection = live();
        connection
            .query(
                "IF OBJECT_ID('dbo.fix_cat_child') IS NOT NULL DROP TABLE dbo.fix_cat_child;
                 IF OBJECT_ID('dbo.fix_cat_parent') IS NOT NULL DROP TABLE dbo.fix_cat_parent;",
                Fetch::default(),
            )
            .expect("stale scratch tables should drop");
        connection
            .query(
                "CREATE TABLE dbo.fix_cat_parent (id INT PRIMARY KEY);
                 CREATE TABLE dbo.fix_cat_child (
                     id INT PRIMARY KEY,
                     parent_id INT,
                     CONSTRAINT fix_cat_fk_child_parent FOREIGN KEY (parent_id)
                         REFERENCES dbo.fix_cat_parent(id)
                         ON DELETE CASCADE ON UPDATE SET NULL
                 );
                 ALTER TABLE dbo.fix_cat_child NOCHECK CONSTRAINT fix_cat_fk_child_parent;",
                Fetch::default(),
            )
            .expect("the scratch foreign key should be created");

        let child = connection
            .structure("dbo", "fix_cat_child")
            .expect("structure should load");
        assert!(
            child
                .constraints
                .iter()
                .any(|constraint| constraint.name == "fix_cat_fk_child_parent"
                    && constraint.definition
                        == "FOREIGN KEY (parent_id) REFERENCES \"dbo\".\"fix_cat_parent\" (\"id\") \
                        ON DELETE CASCADE ON UPDATE SET NULL DISABLED NOT TRUSTED"),
            "{:?}",
            child.constraints
        );

        connection
            .query(
                "DROP TABLE dbo.fix_cat_child;
                 DROP TABLE dbo.fix_cat_parent;",
                Fetch::default(),
            )
            .expect("cleanup should succeed");
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_computed_or_identity_column_is_not_editable_but_still_keys_the_row() {
        let connection = live();
        connection
            .query(
                "IF OBJECT_ID('dbo.fix_cat_widgets') IS NOT NULL DROP TABLE dbo.fix_cat_widgets",
                Fetch::default(),
            )
            .expect("a stale scratch table should drop");
        connection
            .query(
                "CREATE TABLE dbo.fix_cat_widgets (
                     id INT IDENTITY(1,1) PRIMARY KEY,
                     name NVARCHAR(50) NOT NULL,
                     double_id AS (id * 2) PERSISTED
                 );
                 INSERT INTO dbo.fix_cat_widgets (name) VALUES ('a');",
                Fetch::default(),
            )
            .expect("the scratch table should be created and seeded");

        let edit = connection
            .query(
                "SELECT id, name, double_id FROM dbo.fix_cat_widgets",
                Fetch::default(),
            )
            .expect("query should succeed")
            .edit
            .expect("fix_cat_widgets has a primary key");
        assert_eq!(edit.keys, vec![0], "the identity column is the key");
        assert_eq!(
            edit.columns,
            vec![Some("id".into()), Some("name".into()), None],
            "the identity column still keys the row, but neither it nor the \
             computed column is a SET target"
        );

        connection
            .query("DROP TABLE dbo.fix_cat_widgets", Fetch::default())
            .expect("cleanup should succeed");
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
                Some(10),
                offset,
            );
            assert!(sql::is_generated_select(Engine::SqlServer, &sql), "{sql}");
            let paged = sql::paged(Engine::SqlServer, &sql, &[]).expect("a preview has a page");
            connection
                .generated(&paged, Fetch::default())
                .expect("the preview should run")
        };

        // Rows whose sensor is `sensor-03` are ids 3, 27, 51 … 4995; sorted
        // down, page three of ten starts twenty rows below the top.
        let equals = filter_predicate(
            Engine::SqlServer,
            "sensor",
            None,
            Operator::Equals,
            "sensor-03",
        )
        .unwrap();
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
        let literal = filter_predicate(
            Engine::SqlServer,
            "sensor",
            None,
            Operator::Contains,
            "sensor_03",
        )
        .unwrap();
        assert!(preview(&literal, 0).rows.is_empty());
        // No limit is every matching row on one page, with no order to page by.
        let every = relation_sql(
            Engine::SqlServer,
            "dbo",
            "measurements",
            &equals,
            &[],
            None,
            0,
        );
        let every = sql::paged(Engine::SqlServer, &every, &[]).unwrap();
        assert_eq!(
            connection
                .generated(&every, Fetch::default())
                .unwrap()
                .rows
                .len(),
            209
        );
        let unsorted = relation_sql(Engine::SqlServer, "dbo", "orders", "", &[], Some(100), 0);
        let unsorted = sql::paged(Engine::SqlServer, &unsorted, &[]).unwrap();
        assert_eq!(
            connection
                .generated(&unsorted, Fetch::default())
                .unwrap()
                .rows
                .len(),
            3
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_single_table_select_is_editable_by_its_primary_key() {
        let connection = live();
        let edit = connection
            .query("SELECT name, id FROM accounts", Fetch::default())
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");
        assert_eq!(edit.schema, "dbo");
        assert_eq!(edit.table, "accounts");
        assert_eq!(edit.columns, vec![Some("name".into()), Some("id".into())]);
        assert_eq!(edit.keys, vec![1]);

        let aliased = connection
            .query(
                "SELECT id AS ident, upper(name) AS shouted, name FROM accounts",
                Fetch::default(),
            )
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");
        assert_eq!(
            aliased.columns,
            vec![Some("id".into()), None, Some("name".into())]
        );
        assert_eq!(aliased.keys, vec![0]);

        let composite = connection
            .query(
                "SELECT total, number, account_id FROM orders",
                Fetch::default(),
            )
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
                    .query(sql, Fetch::default())
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
            .query("BEGIN TRANSACTION", Fetch::default())
            .expect("BEGIN TRANSACTION should succeed");
        assert!(
            connection
                .query("SELECT id FROM accounts", Fetch::default())
                .expect("query should succeed")
                .edit
                .is_some()
        );
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        let error = connection
            .query("SELECT * FROM no_such_relation", Fetch::default())
            .unwrap_err();
        assert!(
            error.message.contains("no_such_relation"),
            "{}",
            error.message
        );
        // `XACT_ABORT` ends the transaction on that error, as it would a
        // batch's, and the error says so.
        assert!(
            error
                .message
                .contains("open before this statement was rolled back"),
            "{}",
            error.message
        );
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("0")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_an_edit_round_trips_by_primary_key() {
        let connection = live();
        let run = |sql: &str| {
            assert!(sql::is_generated_write(sql), "the gate refused {sql}");
            connection.generated(sql, Fetch::default())
        };
        let name_of = |id: &str| {
            connection
                .query(
                    &format!("SELECT name FROM locations WHERE id = {id}"),
                    Fetch::default(),
                )
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
            types: Vec::new(),
        };
        for id in ["901", "902"] {
            let _ = connection.query(
                &format!("DELETE FROM locations WHERE id = {id}"),
                Fetch::default(),
            );
            run(&sql::insert_row(
                Engine::SqlServer,
                "dbo",
                "locations",
                &[("id", Some(id)), ("name", Some("placeholder"))],
                &[],
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
            &[],
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
        assert!(
            error.message.contains("nothing the batch wrote remains"),
            "{}",
            error.message
        );
        assert_eq!(name_of("901").as_deref(), Some("first"));
        assert_eq!(
            first(
                &connection
                    .query("SELECT @@TRANCOUNT", Fetch::default())
                    .unwrap()
            ),
            vec![Some("0")]
        );

        for id in ["901", "902"] {
            let delete =
                sql::delete_row(Engine::SqlServer, "dbo", "locations", &[("id", id)], &[]).unwrap();
            assert!(sql::delete_matches_key(&delete, &["id"]));
            run(&delete).expect("the delete should run");
            assert_eq!(name_of(id), None);
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_binary_or_varchar_key_names_its_row() {
        // Through the grid, as an edit and a delete travel: the key is the
        // value the grid shows, its literal spelled by the column's type.
        let connection = live();
        let tables = [
            (
                "fix_vals_binary_key",
                "binary(16)",
                "0x000102030405060708090A0B0C0D0EFF",
            ),
            ("fix_vals_varchar_key", "varchar(20)", "'k-1'"),
        ];
        let drop = || {
            for (table, _, _) in tables {
                let _ = connection.query(
                    &format!("DROP TABLE IF EXISTS dbo.{table}"),
                    Fetch::default(),
                );
            }
        };
        drop();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (table, key, id) in tables {
                connection
                    .query(
                        &format!(
                            "CREATE TABLE dbo.{table} \
                         (id {key} NOT NULL PRIMARY KEY, name nvarchar(20) NOT NULL); \
                         INSERT INTO dbo.{table} VALUES ({id}, N'before')"
                        ),
                        Fetch::default(),
                    )
                    .unwrap();
                let select = format!("SELECT id, name FROM dbo.{table}");
                let name = || {
                    let rows = connection.query(&select, Fetch::default()).unwrap().rows;
                    rows.first().and_then(|row| row[1].clone())
                };
                let run = |sql: &str| {
                    assert!(sql::is_generated_write(sql), "the gate refused {sql}");
                    connection
                        .query(sql, Fetch::default())
                        .expect("the write should run");
                };

                let mut grid = crate::result_grid::ResultGrid::new(
                    connection.query(&select, Fetch::default()).unwrap(),
                    crate::sql::Mode::ReadWrite,
                )
                .with_engine(Engine::SqlServer);
                assert!(grid.set_pending(0, 1, NewValue::Value("after".into())));
                let batch = sql::update_batch(Engine::SqlServer, &grid.pending_updates()).unwrap();
                run(&batch);
                assert_eq!(name().as_deref(), Some("after"), "{batch}");

                let (schema, relation, keys) = grid.row_key(0).expect("a key names the row");
                let keys: Vec<(&str, &str)> = keys
                    .iter()
                    .map(|(column, value)| (column.as_str(), value.as_str()))
                    .collect();
                let delete = sql::delete_row(
                    Engine::SqlServer,
                    &schema,
                    &relation,
                    &keys,
                    &grid.row_types(0),
                )
                .unwrap();
                assert!(sql::delete_matches_key(&delete, &["id"]));
                run(&delete);
                assert!(
                    connection
                        .query(&select, Fetch::default())
                        .unwrap()
                        .rows
                        .is_empty(),
                    "{delete}"
                );
            }
        }));
        drop();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_seeded_binary_or_datetime_key_edits_and_filters_only_its_own_row() {
        // Under `british` a `datetime` literal reads year-day-month, so the
        // first row's key, misread, names the second row.
        let connection = live();
        let reset = || {
            connection
                .query(
                    "UPDATE dbo.blobs_by_hash SET label = N'first' \
                     WHERE hash = 0x000102030405060708090A0B0C0D0EFF; \
                     UPDATE dbo.readings_by_time SET reading = 1.00 \
                     WHERE taken_at = '2024-01-02T03:04:05'",
                    Fetch::default(),
                )
                .expect("the seeded rows should reset");
        };
        reset();
        connection
            .query("SET LANGUAGE british", Fetch::default())
            .unwrap();
        for (table, key, after, untouched) in [
            ("blobs_by_hash", "hash", "edited", "second"),
            ("readings_by_time", "taken_at", "9.50", "2.00"),
        ] {
            let select = format!("SELECT * FROM dbo.{table} ORDER BY {key}");
            let mut grid = crate::result_grid::ResultGrid::new(
                connection.query(&select, Fetch::default()).unwrap(),
                crate::sql::Mode::ReadWrite,
            )
            .with_engine(Engine::SqlServer);
            assert!(grid.set_pending(0, 1, NewValue::Value(after.into())));
            let batch = sql::update_batch(Engine::SqlServer, &grid.pending_updates()).unwrap();
            assert!(sql::is_generated_write(&batch), "the gate refused {batch}");
            connection
                .generated(&batch, Fetch::default())
                .expect("the edit should run");
            let rows = connection.query(&select, Fetch::default()).unwrap().rows;
            assert_eq!(rows[0][1].as_deref(), Some(after), "{batch}");
            assert_eq!(rows[1][1].as_deref(), Some(untouched), "{batch}");

            // The filter bar names the same row by the same literal.
            let structure = connection.structure("dbo", table).unwrap();
            let bar = FilterBar {
                column: Some(key.into()),
                value: rows[0][0].clone().unwrap(),
                ..FilterBar::default()
            };
            let filter = derived_filter(Engine::SqlServer, &[bar], &structure.columns);
            let preview = relation_sql(Engine::SqlServer, "dbo", table, &filter, &[], Some(10), 0);
            assert!(
                sql::is_generated_select(Engine::SqlServer, &preview),
                "{preview}"
            );
            let paged = sql::paged(Engine::SqlServer, &preview, &structure.row_key()).unwrap();
            let found = connection
                .generated(&paged, Fetch::default())
                .expect("the preview should run");
            assert_eq!(found.rows.len(), 1, "{paged}");
            assert_eq!(found.rows[0][1].as_deref(), Some(after), "{paged}");
        }
        reset();
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_seeded_spatial_preview_reads_as_text_inside_the_users_transaction() {
        let connection = live();
        let structure = connection.structure("dbo", "places").unwrap();
        let preview = relation_sql(Engine::SqlServer, "dbo", "places", "", &[], Some(10), 0);
        let paged = sql::paged(Engine::SqlServer, &preview, &structure.row_key()).unwrap();
        connection
            .query("BEGIN TRANSACTION", Fetch::default())
            .unwrap();
        let page = connection
            .generated(&paged, Fetch::default())
            .expect("the preview should run");
        let rows: Vec<Vec<Option<&str>>> = page
            .rows
            .iter()
            .map(|row| row.iter().map(Option::as_deref).collect())
            .collect();
        assert_eq!(
            rows,
            vec![
                vec![
                    Some("1"),
                    Some("POINT (-0.125 51.5)"),
                    Some("/1/"),
                    Some("42")
                ],
                vec![Some("2"), Some("POINT (0 0)"), Some("/1/2/"), Some("text")],
                vec![Some("3"), Some("POINT (151.25 -33.5)"), None, None],
            ]
        );
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    /// A read the server has to work through, about a second of it: there is
    /// no `WAITFOR` in a plain read, and only a plain read runs at once with
    /// others. A `COUNT` of the same join is answered without the work.
    const LIVE_SLOW_SELECT: &str = "SELECT MAX(CHECKSUM(a.name, b.name)) FROM sys.all_objects a \
         CROSS JOIN (SELECT TOP 500 name FROM sys.all_objects) b";

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_reads_run_at_once_take_the_time_of_one() {
        super::super::at_once::reads_overlap(
            &super::super::Connection::SqlServer(live()),
            Engine::SqlServer,
            LIVE_SLOW_SELECT,
            "INSERT INTO accounts DEFAULT VALUES",
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_cancel_stops_every_read_run_at_once() {
        super::super::at_once::a_cancel_stops_them_all(
            &super::super::Connection::SqlServer(live()),
            Engine::SqlServer,
            "SELECT MAX(CHECKSUM(a.name, b.name, c.name)) FROM sys.all_objects a \
             CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c",
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_cancel_stops_the_statement_on_the_server_and_reconnects() {
        // The probe outlives the session it is checked from only as a global
        // temporary table, which lives as long as the session that made it.
        let observer = live();
        observer
            .query(
                "CREATE TABLE ##dbdelve_cancel_probe (id int)",
                Fetch::default(),
            )
            .expect("the probe should be created");

        let connection = live();
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the cancel should send");
        });
        let started = Instant::now();
        let error = connection
            .query(
                "WAITFOR DELAY '00:00:03'; INSERT INTO ##dbdelve_cancel_probe VALUES (1)",
                Fetch::default(),
            )
            .expect_err("the statement should be cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(
            crate::db::is_cancel(Engine::SqlServer, &error),
            "{}",
            error.message
        );
        assert!(error.message.contains("reconnected"), "{}", error.message);

        // Honest only if the server stopped too: past the delay, the insert
        // behind it never ran.
        std::thread::sleep(Duration::from_secs(4));
        let count = observer
            .query(
                "SELECT count(*) FROM ##dbdelve_cancel_probe",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(first(&count), vec![Some("0")]);
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
        // A cancel with nothing running stops nothing.
        connection.cancel().unwrap();
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
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
            .query("WAITFOR DELAY '00:00:30'", Fetch::default())
            .expect_err("the statement should time out");
        assert!(
            error.message.contains("1-second statement timeout"),
            "{}",
            error.message
        );
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    /// Rows of a probe table, numbered from one, in this session alone.
    fn probe_table(connection: &Connection, name: &str, rows: usize) {
        connection
            .query(
                &format!(
                    "CREATE TABLE {name} (id int, touched bit DEFAULT 0); \
                     INSERT INTO {name} (id) SELECT TOP {rows} \
                     ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) FROM sys.all_columns"
                ),
                Fetch::default(),
            )
            .unwrap();
    }

    /// Every row of the probe table, touched: what a session left capped by
    /// `SET ROWCOUNT` would stop short of.
    fn touches_every_row(connection: &Connection, name: &str, rows: usize) {
        let updated = connection
            .query(&format!("UPDATE {name} SET touched = 1"), Fetch::default())
            .unwrap();
        assert_eq!(updated.rows_affected, Some(rows as u64));
        let touched = connection
            .query(
                &format!("SELECT count(*) FROM {name} WHERE touched = 1"),
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(first(&touched), vec![Some(rows.to_string().as_str())]);
    }

    fn stoppable(limit: usize) -> Fetch<'static> {
        Fetch {
            limit: Some(limit),
            reads_only: true,
            ..Fetch::default()
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_lone_read_is_stopped_at_the_limit_by_the_server_and_the_cap_taken_back() {
        let connection = live();
        probe_table(&connection, "#dbdelve_rowcount_probe", 50);
        let started = Instant::now();
        let result = connection
            .query(
                "SELECT a.name FROM sys.all_columns a CROSS JOIN sys.all_columns b",
                stoppable(10),
            )
            .expect("a stopped read is not an error");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.capped_from, None);
        assert_eq!(result.rows.len(), 10);
        touches_every_row(&connection, "#dbdelve_rowcount_probe", 50);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_read_that_fails_still_takes_its_cap_back() {
        let connection = live();
        probe_table(&connection, "#dbdelve_rowcount_probe", 50);
        let error = connection
            .query("SELECT 1 / 0 AS x FROM sys.all_columns", stoppable(10))
            .expect_err("the division fails");
        assert!(error.message.contains("zero"), "{}", error.message);
        touches_every_row(&connection, "#dbdelve_rowcount_probe", 50);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_read_stopped_at_the_limit_leaves_the_users_transaction_open() {
        let connection = live();
        probe_table(&connection, "#dbdelve_rowcount_probe", 50);
        connection
            .query(
                "BEGIN TRANSACTION; UPDATE #dbdelve_rowcount_probe SET touched = 1 WHERE id = 1",
                Fetch::default(),
            )
            .unwrap();
        let result = connection
            .query("SELECT id FROM #dbdelve_rowcount_probe", stoppable(10))
            .unwrap();
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        let kept = connection
            .query(
                "SELECT count(*) FROM #dbdelve_rowcount_probe WHERE touched = 1",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(first(&kept), vec![Some("1")]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_the_timeout_leaves_a_capped_read_to_drain_inside_an_open_transaction() {
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 1,
            ..live_config()
        })
        .expect("connection should open");
        connection
            .query("BEGIN TRANSACTION", Fetch::default())
            .unwrap();
        let started = Instant::now();
        let capped = connection
            .query(
                "SELECT TOP 3000000 a.name, b.name FROM sys.all_columns a \
                 CROSS JOIN sys.all_columns b",
                Fetch {
                    limit: Some(10),
                    ..Fetch::default()
                },
            )
            .expect("rows past the limit are drained, not timed out");
        assert!(
            started.elapsed() > Duration::from_secs(1),
            "the drain has to outlast the timeout to prove anything: {:?}",
            started.elapsed()
        );
        assert_eq!(capped.capped_from, Some(3_000_000));
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_use_is_moved_back_and_said() {
        let connection = live();
        let error = connection
            .query("USE master; SELECT 1", Fetch::default())
            .expect_err("leaving the profile's database is reported");
        assert!(error.message.contains("moved it back"), "{}", error.message);
        let here = connection
            .query("SELECT DB_NAME()", Fetch::default())
            .unwrap();
        assert_eq!(first(&here), vec![Some(live_config().database.as_str())]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_use_is_moved_back_when_the_batch_fails_too() {
        let connection = live();
        let error = connection
            .query(
                "USE master; SELECT * FROM no_such_relation",
                Fetch::default(),
            )
            .expect_err("the relation does not exist");
        assert!(
            error.message.contains("no_such_relation"),
            "{}",
            error.message
        );
        assert!(error.message.contains("moved it back"), "{}", error.message);
        let here = connection
            .query("SELECT DB_NAME()", Fetch::default())
            .unwrap();
        assert_eq!(first(&here), vec![Some(live_config().database.as_str())]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_type_the_driver_cannot_read_is_refused_without_losing_the_session() {
        let connection = live();
        connection
            .query("CREATE TABLE #shapes (id int PRIMARY KEY, place geography, node hierarchyid, anything sql_variant)", Fetch::default())
            .unwrap();
        connection
            .query(
                "INSERT INTO #shapes VALUES \
                 (1, geography::Point(1, 2, 4326), hierarchyid::Parse('/1/2/'), CAST(5 AS int))",
                Fetch::default(),
            )
            .unwrap();
        connection
            .query("BEGIN TRANSACTION", Fetch::default())
            .unwrap();
        for sql in [
            "SELECT SERVERPROPERTY('ProductVersion')",
            "SELECT * FROM #shapes",
        ] {
            let error = connection
                .query(sql, Fetch::default())
                .expect_err("the driver cannot decode it");
            assert!(error.message.contains("was not run"), "{}", error.message);
        }
        // The session, its transaction and its temporary table all survived.
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();

        // A relation tab reads the same columns as text.
        let preview = connection
            .generated(
                "SELECT * FROM \"#shapes\" ORDER BY (SELECT NULL) \
                 OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY",
                Fetch::default(),
            )
            .expect("the preview should run");
        assert_eq!(names(&preview), vec!["id", "place", "node", "anything"]);
        assert_eq!(
            first(&preview),
            vec![Some("1"), Some("POINT (2 1)"), Some("/1/2/"), Some("5")]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_dbdelves_statements_run_under_their_own_options_and_leave_the_users_alone() {
        let connection = live();
        connection
            .query(
                "CREATE TABLE #edits (id int PRIMARY KEY, name nvarchar(20) NOT NULL)",
                Fetch::default(),
            )
            .unwrap();
        connection
            .query(
                "INSERT INTO #edits VALUES (1, N'first'), (2, N'second')",
                Fetch::default(),
            )
            .unwrap();
        connection
            .query(
                "SET XACT_ABORT OFF; SET QUOTED_IDENTIFIER OFF; SET ROWCOUNT 1; \
                 SET LANGUAGE british; SET IMPLICIT_TRANSACTIONS ON",
                Fetch::default(),
            )
            .unwrap();

        // `ROWCOUNT` would cut the page to a row, and `QUOTED_IDENTIFIER OFF`
        // would read the quoted names as strings.
        let page = connection
            .generated(
                "SELECT \"id\" FROM \"dbo\".\"measurements\" ORDER BY \"id\" \
                 OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(page.rows.len(), 5);
        // British reads a `datetime` literal as year-day-month.
        let month = connection
            .generated(
                "SELECT MONTH(CAST(N'2024-01-02 03:04:05.000' AS datetime)) AS month",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(first(&month), vec![Some("1")]);

        // All-or-nothing, though the user turned `XACT_ABORT` off.
        let error = connection
            .generated(
                "BEGIN TRANSACTION; \
                 UPDATE \"#edits\" SET \"name\" = N'changed' WHERE \"id\" = N'1'; \
                 UPDATE \"#edits\" SET \"name\" = NULL WHERE \"id\" = N'2'; COMMIT;",
                Fetch::default(),
            )
            .expect_err("NULL into a NOT NULL column fails");
        assert!(
            error.message.contains("nothing the batch wrote remains"),
            "{}",
            error.message
        );
        // Still counted in the statement's own lines inside `sp_executesql`.
        assert_eq!(error.position, Some(0));
        let names = connection
            .generated(
                "SELECT \"name\" FROM \"#edits\" ORDER BY \"id\"",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(names.rows[0][0].as_deref(), Some("first"));
        // Committed, not left in a transaction `IMPLICIT_TRANSACTIONS` opened.
        let update = connection
            .generated(
                "UPDATE \"#edits\" SET \"name\" = N'third' WHERE \"id\" = N'1'",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(update.rows_affected, Some(1));
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("0")]);

        // And the user's own settings are still theirs.
        let options = connection
            .query("SELECT @@OPTIONS & 16384 AS xact_abort, @@OPTIONS & 256 AS quoted, @@OPTIONS & 2 AS implicit", Fetch::default())
            .unwrap();
        assert_eq!(first(&options), vec![Some("0"), Some("0"), Some("2")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_failure_says_what_it_did_to_a_transaction_begun_before_it() {
        let connection = live();
        connection
            .query("CREATE TABLE #work (id int)", Fetch::default())
            .unwrap();
        connection
            .query(
                "BEGIN TRANSACTION; INSERT INTO #work VALUES (1)",
                Fetch::default(),
            )
            .unwrap();
        // A syntax error: the batch never starts, and the transaction the user
        // began is theirs to finish.
        let error = connection
            .query("BEGIN TRANSACTION;\nSELECT FROM WHERE", Fetch::default())
            .expect_err("the batch does not parse");
        assert!(!error.message.contains("rolled back"), "{}", error.message);
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("1")]);
        let kept = connection
            .query("SELECT count(*) FROM #work", Fetch::default())
            .unwrap();
        assert_eq!(first(&kept), vec![Some("1")]);

        let error = connection
            .query("SELECT 1/0", Fetch::default())
            .expect_err("division by zero");
        assert!(
            error
                .message
                .contains("open before this statement was rolled back"),
            "{}",
            error.message
        );
        let open = connection
            .query("SELECT @@TRANCOUNT", Fetch::default())
            .unwrap();
        assert_eq!(first(&open), vec![Some("0")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_write_reports_the_rows_it_affected() {
        let connection = live();
        let count = |sql: &str| {
            connection
                .query(sql, Fetch::default())
                .unwrap()
                .rows_affected
        };
        assert_eq!(count("CREATE TABLE #counted (id int)"), Some(0));
        assert_eq!(count("INSERT INTO #counted VALUES (1), (2), (3)"), Some(3));
        assert_eq!(count("UPDATE #counted SET id = id WHERE id > 5"), Some(0));
        assert_eq!(count("DELETE FROM #counted WHERE id < 3"), Some(2));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_users_statement_is_checked_under_the_users_own_quoting() {
        let connection = live();
        connection
            .query("SET QUOTED_IDENTIFIER OFF", Fetch::default())
            .unwrap();
        // A string, not the geography column: nothing the driver cannot read.
        let quoted = connection
            .query("SELECT \"position\" AS p FROM places", Fetch::default())
            .unwrap();
        assert_eq!(quoted.rows[0][0].as_deref(), Some("position"));
        // And the user's `ROWCOUNT` does not hide a column from the check.
        connection
            .query("SET QUOTED_IDENTIFIER ON; SET ROWCOUNT 1", Fetch::default())
            .unwrap();
        let error = connection
            .query(
                "SELECT id, id AS copy, position FROM places",
                Fetch::default(),
            )
            .expect_err("the third column is a geography");
        assert!(
            error.message.contains("position (geography)"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_datetime_literal_reads_the_same_under_any_language() {
        let connection = live();
        connection
            .query(
                "CREATE TABLE #stamps (at datetime PRIMARY KEY, small smalldatetime); \
                 INSERT INTO #stamps VALUES ('20240102 03:04:05', '20240102 03:04'), \
                 ('20240201 03:04:05', '20240201 03:04')",
                Fetch::default(),
            )
            .unwrap();
        // The user's own session, not dbdelve's options: a statement appended
        // to a query tab's buffer runs as the user's.
        connection
            .query("SET LANGUAGE british", Fetch::default())
            .unwrap();
        let at = Engine::SqlServer.quote_value("2024-01-02 03:04:05.000", Some("datetime"));
        let small = Engine::SqlServer.quote_value("2024-01-02 03:04:00", Some("smalldatetime"));
        let deleted = connection
            .query(
                &format!("DELETE FROM #stamps WHERE at = {at} AND small = {small}"),
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(deleted.rows_affected, Some(1));
        let left = connection
            .query(
                "SELECT CONVERT(char(8), at, 112) FROM #stamps",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(first(&left), vec![Some("20240201")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_rowversion_filters_by_its_bare_hex() {
        let connection = live();
        connection
            .query(
                "CREATE TABLE #versions (id int PRIMARY KEY, version rowversion); \
                    INSERT INTO #versions (id) VALUES (1), (2)",
                Fetch::default(),
            )
            .unwrap();
        let versions = connection
            .query(
                "SELECT version FROM #versions WHERE id = 2",
                Fetch::default(),
            )
            .unwrap();
        let version = versions.rows[0][0].clone().unwrap();
        // `timestamp` is the name the catalog gives it, and a pasted value may
        // wear an uppercase prefix.
        for value in [version.clone(), version.replacen("0x", "0X", 1)] {
            let predicate = filter_predicate(
                Engine::SqlServer,
                "version",
                Some("timestamp"),
                Operator::Equals,
                &value,
            )
            .unwrap();
            let found = connection
                .generated(
                    &format!("SELECT \"id\" FROM \"#versions\" WHERE {predicate}"),
                    Fetch::default(),
                )
                .unwrap();
            assert_eq!(first(&found), vec![Some("2")], "{predicate}");
        }
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_an_error_inside_a_procedure_points_at_nothing_in_the_batch() {
        let connection = live();
        connection
            .query("CREATE PROCEDURE #fails AS\nSELECT 1/0", Fetch::default())
            .unwrap();
        let error = connection
            .query("SELECT 1;\nEXEC #fails", Fetch::default())
            .expect_err("division by zero");
        assert_eq!(error.position, None, "{}", error.message);
        let error = connection
            .query("SELECT 1;\nSELECT 1/0", Fetch::default())
            .expect_err("division by zero");
        assert_eq!(error.position, Some(10));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_round_trip_beside_the_statement_is_bounded_by_the_timeout() {
        let observer = live();
        observer
            .query(
                "CREATE TABLE ##dbdelve_lock_probe (id int)",
                Fetch::default(),
            )
            .unwrap();
        // A schema lock, held until the rollback, blocks even describing it.
        observer
            .query(
                "BEGIN TRANSACTION; ALTER TABLE ##dbdelve_lock_probe ADD extra int",
                Fetch::default(),
            )
            .unwrap();
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 1,
            ..live_config()
        })
        .expect("connection should open");
        let started = Instant::now();
        let error = connection
            .query("SELECT * FROM ##dbdelve_lock_probe", Fetch::default())
            .expect_err("the lock outlasts the timeout");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.message.contains("1-second"), "{}", error.message);
        observer.query("ROLLBACK", Fetch::default()).unwrap();
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MSSQL_URL"]
    fn live_a_cancel_while_queued_behind_a_catalog_load_stops_the_statement_not_the_load() {
        let connection = live();
        let loader = connection.clone();
        let load = std::thread::spawn(move || {
            loader.internal_query("WAITFOR DELAY '00:00:02'; SELECT 1 AS one")
        });
        // Waits on the state itself, not a sleep: on a busy runner a spawned
        // thread can take longer than any fixed delay to get going.
        while connection.session.try_lock().is_ok() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let user = connection.clone();
        let queued = std::thread::spawn(move || user.query("SELECT 2 AS two", Fetch::default()));
        while connection.in_flight().queued == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        connection.cancel().unwrap();

        assert!(load.join().unwrap().is_ok());
        let error = queued
            .join()
            .unwrap()
            .expect_err("the statement was cancelled");
        assert!(
            error.message.starts_with("Cancelled before it started"),
            "{}",
            error.message
        );
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    fn a_cancel_while_reconnecting_stops_the_statement_before_it_is_sent() {
        // A server that accepts the connection and never answers the login.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connection = Connection {
            session: Arc::new(Mutex::new(None)),
            in_flight: Arc::default(),
            server: config_from_url(&format!(
                "mssql://someone@127.0.0.1:{port}/db?sslmode=require"
            ))
            .unwrap(),
            tunnel: None,
            alongside: false,
        };
        let user = connection.clone();
        let run = std::thread::spawn(move || user.query("SELECT 1", Fetch::default()));
        // Accepted, so the statement is waiting on its reconnect.
        let (socket, _) = listener.accept().unwrap();
        connection.cancel().unwrap();
        drop((socket, listener));

        let error = run
            .join()
            .unwrap()
            .expect_err("the statement was cancelled");
        assert!(
            error.message.starts_with("Cancelled before it started"),
            "{}",
            error.message
        );
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

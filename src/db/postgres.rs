use std::collections::HashSet;
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt, future::Fuse};
use geozero::{CoordDimensions, ToWkt, wkb::Ewkb};
use tokio::runtime::Runtime;
use tokio_postgres::{
    CancelToken, NoTls, SimpleQueryMessage, SimpleQueryStream, config::Host, error::SqlState,
};

use crate::tls;

use super::ssh::{Tunnel, tunnelled};
use super::{
    Catalog, Cell, Column, DbError, EditTarget, Engine, Feed, Fetch, QueryResult, Reference,
    RelationKind, ServerConfig, Sizes, Stopped, Structure, assemble_catalog, assemble_databases,
    assemble_foreign_keys, assemble_references, assemble_sizes, assemble_structure, create_table,
    non_utf8_error, optional_cell, plain_error, required_cell, terminated,
};

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 5432;

const DATABASES_SQL: &str = "
SELECT datname AS name, datname = current_database() AS is_current
FROM pg_database
WHERE datallowconn AND NOT datistemplate
ORDER BY datname";

const RELATIONS_SQL: &str = "
SELECT
    namespace.nspname AS schema_name,
    class.relname AS relation_name,
    CASE class.relkind
        WHEN 'r' THEN 'table'
        WHEN 'p' THEN 'partitioned_table'
        WHEN 'v' THEN 'view'
        WHEN 'm' THEN 'materialized_view'
        WHEN 'f' THEN 'foreign_table'
    END AS relation_kind,
    -- A subquery rather than a join: `pg_inherits` holds legacy inheritance
    -- too, and a table with two ordinary parents would come back as two
    -- relations. A partition has exactly one parent, so this stays scalar.
    -- Restricted to the child's own schema because the sidebar nests by name
    -- within a schema, and a parent elsewhere would nest the child under
    -- whatever happened to share its name.
    (
        SELECT parent.relname
        FROM pg_catalog.pg_inherits AS inheritance
        JOIN pg_catalog.pg_class AS parent
            ON parent.oid = inheritance.inhparent
        WHERE inheritance.inhrelid = class.oid
            AND parent.relkind = 'p'
            AND parent.relnamespace = class.relnamespace
    ) AS partition_of
FROM pg_catalog.pg_class AS class
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
WHERE class.relkind IN ('r', 'p', 'v', 'm', 'f')
    AND namespace.nspname <> 'information_schema'
    AND namespace.nspname NOT LIKE 'pg\\_%' ESCAPE '\\'
ORDER BY namespace.nspname, class.relname
";

// Apart from `RELATIONS_SQL` because `pg_total_relation_size` takes a lock on
// each relation it measures, and a table behind an `ACCESS EXCLUSIVE` lock would
// hold the sidebar, and every query queued behind it, until that lock is let go.
const SIZES_SQL: &str = "
WITH sized AS (
    SELECT
        class.oid,
        class.relkind,
        namespace.nspname,
        class.relname,
        -- Pages as of the last VACUUM or ANALYZE, not `pg_total_relation_size`:
        -- that stats every file of every relation, and databases with thousands
        -- of partitions would pay for all of them on every catalog load.
        -- Zero pages is a table never analyzed (`reltuples < 0` from PG14,
        -- `reltuples = 0` before it) or an empty one, and would claim 0 B for
        -- the first. Those are measured instead: autovacuum only analyzes once
        -- ~50 rows changed, so they are nearly always the few small tables it
        -- never reached, and their files are cheap to stat.
        CASE
        WHEN class.relkind = 'p' THEN NULL
        WHEN class.reltuples < 0 OR class.relpages = 0 THEN
            pg_catalog.pg_total_relation_size(class.oid)
        ELSE
            (
                class.relpages::bigint
                + COALESCE((
                    SELECT toast.relpages
                    FROM pg_catalog.pg_class AS toast
                    WHERE toast.oid = class.reltoastrelid
                ), 0)
                + COALESCE((
                    SELECT sum(idx.relpages)
                    FROM pg_catalog.pg_index AS i
                    JOIN pg_catalog.pg_class AS idx ON idx.oid = i.indexrelid
                    WHERE i.indrelid = class.oid
                ), 0)
            )::bigint * current_setting('block_size')::bigint
        END AS size_bytes,
        -- The planner's estimate, as of the same VACUUM or ANALYZE. Below zero
        -- is the never-analyzed mark from PG14; before it that mark is a zero,
        -- which the status bar never shows as an estimate.
        CASE
        WHEN class.relkind = 'p' OR class.reltuples < 0 THEN NULL
        ELSE class.reltuples::bigint
        END AS row_estimate
    FROM pg_catalog.pg_class AS class
    JOIN pg_catalog.pg_namespace AS namespace
        ON namespace.oid = class.relnamespace
    WHERE class.relkind IN ('r', 'm', 'p')
        AND namespace.nspname <> 'information_schema'
        AND namespace.nspname NOT LIKE 'pg\\_%' ESCAPE '\\'
)
SELECT
    parent.nspname AS schema_name,
    parent.relname AS relation_name,
    -- A partitioned table stores nothing itself, so it is the sum of every
    -- partition under it, at any depth and in any schema. `pg_partition_tree`
    -- is PG12+; on older servers this query fails and sizes are not shown.
    CASE
    WHEN parent.relkind = 'p' THEN
        COALESCE((
            SELECT sum(leaf.size_bytes)
            FROM pg_catalog.pg_partition_tree(parent.oid) AS tree
            JOIN sized AS leaf ON leaf.oid = tree.relid
        ), 0)::bigint
    ELSE parent.size_bytes
    END AS size_bytes,
    -- Only when every leaf has one: a partition never analyzed, or one this
    -- does not measure (a foreign table), would make the sum an undercount.
    CASE
    WHEN parent.relkind = 'p' THEN (
        SELECT CASE WHEN bool_and(leaf.row_estimate IS NOT NULL) THEN sum(leaf.row_estimate) END
        FROM pg_catalog.pg_partition_tree(parent.oid) AS tree
        LEFT JOIN sized AS leaf ON leaf.oid = tree.relid
        WHERE tree.isleaf
    )::bigint
    ELSE parent.row_estimate
    END AS row_estimate
FROM sized AS parent
";

const ROUTINES_SQL: &str = "
SELECT
    namespace.nspname AS schema_name,
    procedure.proname AS routine_name,
    CASE procedure.prokind
        WHEN 'f' THEN 'function'
        WHEN 'p' THEN 'procedure'
    END AS routine_kind,
    pg_catalog.pg_get_function_identity_arguments(procedure.oid) AS identity_arguments,
    COALESCE(pg_catalog.pg_get_function_result(procedure.oid), '') AS result_type,
    language.lanname AS language,
    pg_catalog.pg_get_functiondef(procedure.oid) AS definition
-- Extension-owned routines are excluded. PostGIS alone installs some 750 of
-- them into `public`, and fetching every body at connect buries the handful of
-- routines the user actually wrote.
FROM pg_catalog.pg_proc AS procedure
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = procedure.pronamespace
JOIN pg_catalog.pg_language AS language
    ON language.oid = procedure.prolang
WHERE procedure.prokind IN ('f', 'p')
    AND namespace.nspname <> 'information_schema'
    AND namespace.nspname NOT LIKE 'pg\\_%' ESCAPE '\\'
    AND NOT EXISTS (
        SELECT 1
        FROM pg_catalog.pg_depend AS dependency
        WHERE dependency.objid = procedure.oid
            AND dependency.classid = 'pg_catalog.pg_proc'::regclass
            AND dependency.deptype = 'e'
    )
ORDER BY namespace.nspname, procedure.proname, identity_arguments
";

// The four structure queries name one relation, so they carry `{schema}` and
// `{relation}` placeholders substituted by `structure_sql`.
const STRUCTURE_COLUMNS_SQL: &str = "
SELECT
    attribute.attname AS column_name,
    pg_catalog.format_type(attribute.atttypid, attribute.atttypmod) AS data_type,
    CASE WHEN attribute.attnotnull THEN 'no' ELSE 'yes' END AS nullable,
    -- An identity column has no pg_attrdef row, so reading only the default
    -- would report that the user must supply a value the server generates.
    CASE
        WHEN attribute.attidentity = 'a' THEN 'GENERATED ALWAYS AS IDENTITY'
        WHEN attribute.attidentity = 'd' THEN 'GENERATED BY DEFAULT AS IDENTITY'
        -- A generated column's expression lives where a default's does.
        -- `attgenerated` is new in 12, so it is read through the row as JSON,
        -- where an older server has no such key rather than no such column.
        WHEN to_jsonb(attribute) ->> 'attgenerated' IN ('s', 'v') THEN CONCAT(
            'GENERATED ALWAYS AS (',
            pg_catalog.pg_get_expr(attribute_default.adbin, attribute_default.adrelid),
            CASE to_jsonb(attribute) ->> 'attgenerated' WHEN 's' THEN ') STORED' ELSE ') VIRTUAL' END
        )
        ELSE COALESCE(
            pg_catalog.pg_get_expr(attribute_default.adbin, attribute_default.adrelid),
            ''
        )
    END AS column_default
FROM pg_catalog.pg_attribute AS attribute
JOIN pg_catalog.pg_class AS class
    ON class.oid = attribute.attrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
LEFT JOIN pg_catalog.pg_attrdef AS attribute_default
    ON attribute_default.adrelid = class.oid
    AND attribute_default.adnum = attribute.attnum
WHERE namespace.nspname = {schema}
    AND class.relname = {relation}
    AND attribute.attnum > 0
    AND NOT attribute.attisdropped
ORDER BY attribute.attnum
";

const STRUCTURE_INDEXES_SQL: &str = "
SELECT
    index_class.relname AS object_name,
    pg_catalog.pg_get_indexdef(index_entry.indexrelid) AS definition
FROM pg_catalog.pg_index AS index_entry
JOIN pg_catalog.pg_class AS index_class
    ON index_class.oid = index_entry.indexrelid
JOIN pg_catalog.pg_class AS class
    ON class.oid = index_entry.indrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
WHERE namespace.nspname = {schema}
    AND class.relname = {relation}
ORDER BY index_class.relname
";

const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT
    table_constraint.conname AS object_name,
    pg_catalog.pg_get_constraintdef(table_constraint.oid) AS definition
FROM pg_catalog.pg_constraint AS table_constraint
JOIN pg_catalog.pg_class AS class
    ON class.oid = table_constraint.conrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
WHERE namespace.nspname = {schema}
    AND class.relname = {relation}
ORDER BY table_constraint.conname
";

// What a table's `CREATE` says after its column list: a partitioned table's
// key, or a foreign table's server, one row per option.
const TABLE_STORAGE_SQL: &str = "
SELECT
    pg_catalog.pg_get_partkeydef(class.oid) AS partition_key,
    server.srvname AS foreign_server,
    foreign_option.option_name,
    foreign_option.option_value
FROM pg_catalog.pg_class AS class
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
LEFT JOIN pg_catalog.pg_foreign_table AS foreign_table
    ON foreign_table.ftrelid = class.oid
LEFT JOIN pg_catalog.pg_foreign_server AS server
    ON server.oid = foreign_table.ftserver
LEFT JOIN LATERAL pg_catalog.pg_options_to_table(foreign_table.ftoptions) AS foreign_option
    ON true
WHERE namespace.nspname = {schema}
    AND class.relname = {relation}
";

// The structured half of a foreign key, beside the rendered DDL
// `STRUCTURE_CONSTRAINTS_SQL` keeps.
//
// `unnest(conkey, confkey) WITH ORDINALITY` pairs the two arrays here, in the
// server, because they are parallel by definition and nothing outside this
// query knows that. Carrying two `smallint[]` up and zipping them in Rust would
// put an attribute number across the `src/db/` boundary, which is the
// driver-shaped thing hard rule 4 forbids. The ordinality is what keeps a
// composite key's columns in key order.
const STRUCTURE_FOREIGN_KEYS_SQL: &str = "
SELECT
    source_attribute.attname AS column_name,
    referenced_namespace.nspname AS referenced_schema,
    referenced_class.relname AS referenced_table,
    referenced_attribute.attname AS referenced_column
FROM pg_catalog.pg_constraint AS table_constraint
JOIN pg_catalog.pg_class AS class
    ON class.oid = table_constraint.conrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
JOIN pg_catalog.pg_class AS referenced_class
    ON referenced_class.oid = table_constraint.confrelid
JOIN pg_catalog.pg_namespace AS referenced_namespace
    ON referenced_namespace.oid = referenced_class.relnamespace
CROSS JOIN LATERAL unnest(table_constraint.conkey, table_constraint.confkey)
    WITH ORDINALITY AS key(source_attnum, referenced_attnum, key_position)
JOIN pg_catalog.pg_attribute AS source_attribute
    ON source_attribute.attrelid = table_constraint.conrelid
    AND source_attribute.attnum = key.source_attnum
JOIN pg_catalog.pg_attribute AS referenced_attribute
    ON referenced_attribute.attrelid = table_constraint.confrelid
    AND referenced_attribute.attnum = key.referenced_attnum
WHERE namespace.nspname = {schema}
    AND class.relname = {relation}
    AND table_constraint.contype = 'f'
ORDER BY table_constraint.conname, key.key_position
";

// The same catalog read from the referenced side: who points at this relation.
// `conparentid = 0` keeps a key declared on a partitioned table to the one row
// it was declared as: from PG 12 the server clones it onto every partition, and
// each clone would otherwise be listed as a relation of its own.
const REFERENCES_SQL: &str = "
SELECT
    namespace.nspname AS source_schema,
    class.relname AS source_table,
    source_attribute.attname AS column_name,
    referenced_attribute.attname AS referenced_column,
    table_constraint.conname AS constraint_name
FROM pg_catalog.pg_constraint AS table_constraint
JOIN pg_catalog.pg_class AS class
    ON class.oid = table_constraint.conrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
JOIN pg_catalog.pg_class AS referenced_class
    ON referenced_class.oid = table_constraint.confrelid
JOIN pg_catalog.pg_namespace AS referenced_namespace
    ON referenced_namespace.oid = referenced_class.relnamespace
CROSS JOIN LATERAL unnest(table_constraint.conkey, table_constraint.confkey)
    WITH ORDINALITY AS key(source_attnum, referenced_attnum, key_position)
JOIN pg_catalog.pg_attribute AS source_attribute
    ON source_attribute.attrelid = table_constraint.conrelid
    AND source_attribute.attnum = key.source_attnum
JOIN pg_catalog.pg_attribute AS referenced_attribute
    ON referenced_attribute.attrelid = table_constraint.confrelid
    AND referenced_attribute.attnum = key.referenced_attnum
WHERE referenced_namespace.nspname = {schema}
    AND referenced_class.relname = {relation}
    AND table_constraint.contype = 'f'
    AND table_constraint.conparentid = 0
ORDER BY namespace.nspname, class.relname, table_constraint.conname, key.key_position
";

// Keyed by oid rather than by name: two schemas can hold a table of the same
// name, and the row description says exactly which one the server read.
const PRIMARY_KEY_SQL: &str = "
SELECT
    namespace.nspname AS schema_name,
    class.relname AS table_name,
    attribute.attnum AS attribute_number,
    attribute.attname AS column_name,
    CASE WHEN attribute.attnum = ANY (index_entry.indkey) THEN 'yes' ELSE 'no' END
        AS in_primary_key
-- An inner join on the primary index is what makes a table without one
-- return nothing, which is the same answer as a table DBDelve cannot identify
-- rows in.
FROM pg_catalog.pg_index AS index_entry
JOIN pg_catalog.pg_class AS class
    ON class.oid = index_entry.indrelid
JOIN pg_catalog.pg_namespace AS namespace
    ON namespace.oid = class.relnamespace
JOIN pg_catalog.pg_attribute AS attribute
    ON attribute.attrelid = class.oid
WHERE index_entry.indrelid = {oid}
    AND index_entry.indisprimary
    AND attribute.attnum > 0
    AND NOT attribute.attisdropped
ORDER BY attribute.attnum
";

const CONNECT_TIMEOUT_SECONDS: u64 = 10;

pub fn config_from_url(url: &str) -> Result<ServerConfig, String> {
    let url_parts =
        url::Url::parse(url).map_err(|error| format!("Connection URL is invalid: {error}"))?;
    let has_explicit_port = url_parts.port().is_some()
        || url_parts
            .query_pairs()
            .any(|(key, _)| key.as_ref() == "port");
    // The two keys dbdelve owns come out of the URL before the driver sees
    // it. Its parser has a fixed key list and refuses anything outside it,
    // so `sslrootcert` would die as "invalid connection string" and so
    // would `sslmode=verify-full` — naming neither the option nor the
    // reason. dbdelve carries both itself and re-emits an `sslmode` the
    // driver does know.
    let mut sslmode = tls::SslMode::default();
    let mut root_certificate = None;
    let mut passed_through = Vec::new();
    for (key, value) in url_parts.query_pairs() {
        match key.as_ref() {
            "sslmode" => sslmode = tls::SslMode::parse(value.as_ref())?,
            "sslrootcert" => {
                root_certificate = Some(value.trim().to_string()).filter(|p| !p.is_empty());
            }
            _ => passed_through.push((key.into_owned(), value.into_owned())),
        }
    }
    let mut without_tls_keys = url_parts.clone();
    without_tls_keys.set_query(None);
    {
        let mut query = without_tls_keys.query_pairs_mut();
        for (key, value) in &passed_through {
            query.append_pair(key, value);
        }
    }
    let parsed: tokio_postgres::Config = without_tls_keys
        .as_str()
        .parse()
        .map_err(|error| format!("Connection URL is invalid: {error}"))?;
    let host = match parsed.get_hosts() {
        [Host::Tcp(host)] => host.clone(),
        [] => return Err("Connection URL does not contain a host.".into()),
        #[cfg(unix)]
        [Host::Unix(_)] => return Err("Connection URL contains a Unix socket host.".into()),
        _ => return Err("Connection URL contains more than one host.".into()),
    };
    let database = parsed.get_dbname().unwrap_or_default().to_string();
    let user = parsed
        .get_user()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Connection URL does not contain a username.".to_string())?
        .to_string();
    let password = parsed
        .get_password()
        .map(|password| {
            std::str::from_utf8(password)
                .map(str::to_string)
                .map_err(|_| "Connection URL password is not valid UTF-8.".to_string())
        })
        .transpose()?
        .unwrap_or_default();

    Ok(ServerConfig {
        host,
        // Last wins, as in libpq: the driver's URL parser takes a port
        // from the host section first -- 5432 when there is none -- and
        // then pushes any `?port=` value after it.
        port: has_explicit_port
            .then(|| parsed.get_ports().last().copied())
            .flatten(),
        database,
        user,
        password,
        sslmode,
        root_certificate,
        // A URL has nowhere to say it; the form is where it is set.
        statement_timeout: 0,
        ssh: None,
    })
}

/// libpq key/value connection string.
///
/// Values are single-quoted and escaped rather than interpolated bare, so
/// that a username containing `@` or a password containing a space is
/// passed through intact instead of truncating the string.
fn connection_string(server: &ServerConfig) -> String {
    // An empty `dbname` would have the server pick the user's name, often a
    // database that does not exist; `postgres` is what createdb connects to.
    let dbname = if server.database.is_empty() {
        "postgres"
    } else {
        &server.database
    };
    let mut parts = vec![
        format!("host={}", quote(&server.host)),
        format!("dbname={}", quote(dbname)),
        format!("user={}", quote(&server.user)),
    ];
    if let Some(port) = server.port {
        parts.push(format!("port={port}"));
    }
    if !server.password.is_empty() {
        parts.push(format!("password={}", quote(&server.password)));
    }
    // The driver's three rungs, not dbdelve's five. Verification above
    // `require` belongs to the connector `tls::connector` builds, and
    // handing the driver a word it does not know fails the whole parse.
    parts.push(format!("sslmode={}", server.sslmode.driver_mode()));
    // Without this the driver waits out the OS SYN retry budget, so a host
    // that resolves but drops packets pins the UI in "Connecting…" for
    // minutes with no cancel.
    parts.push(format!("connect_timeout={CONNECT_TIMEOUT_SECONDS}"));
    if server.statement_timeout > 0 {
        // A session default set once, here, rather than a `SET` prepended to
        // the user's submission -- see `ServerConfig::statement_timeout`. It
        // rides in as a startup option because `options` is the only channel
        // libpq has for one, and nothing collides with it: dbdelve's URL parser
        // has no `options` field, so a user-supplied one never reaches here.
        // `statement_timeout` counts milliseconds when given a bare number.
        parts.push(format!(
            "options={}",
            quote(&format!(
                "-c statement_timeout={}",
                u64::from(server.statement_timeout) * 1_000
            ))
        ));
    }
    parts.join(" ")
}

/// The server's own name, which TLS verifies, dialled at the tunnel's address
/// through `hostaddr`, which the certificate check never sees.
fn tunnelled_string(server: &ServerConfig, dial: SocketAddr) -> String {
    let dialled = ServerConfig {
        port: Some(dial.port()),
        ..server.clone()
    };
    format!("{} hostaddr={}", connection_string(&dialled), dial.ip())
}

fn quote(value: &str) -> String {
    let escaped = value.replace('\\', r"\\").replace('\'', r"\'");
    format!("'{escaped}'")
}

type Error = tokio_postgres::Error;
/// The driver's half of a connection, whose type differs with the TLS stream.
type Socket = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

/// tokio-postgres on a current-thread runtime of its own, driven only while a
/// call blocks on it: what the `postgres` crate does, which offers no way to
/// read a simple query's rows as they arrive rather than all at once.
struct Client {
    // Declared before `driver` so it drops first. A dropped client is what
    // tells the connection to send `Terminate`, and `Driver::drop` is what
    // polls it long enough to.
    client: tokio_postgres::Client,
    driver: Driver,
}

struct Driver {
    runtime: Runtime,
    connection: Fuse<Socket>,
}

impl Driver {
    /// `future`, with the connection polled beside it so the socket is read
    /// while the request waits. A connection that fails ends the call with its
    /// own error. For a backend killed mid-statement that is "connection
    /// closed", not the server's FATAL, as it was under the `postgres` crate.
    fn block_on<T>(&mut self, future: impl Future<Output = T>) -> Result<T, Error> {
        let mut future = pin!(future);
        let connection = &mut self.connection;
        self.runtime.block_on(poll_fn(|cx| {
            if let Poll::Ready(Err(error)) = connection.poll_unpin(cx) {
                return Poll::Ready(Err(error));
            }
            future.as_mut().poll(cx).map(Ok)
        }))
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        use futures_util::future::FusedFuture;
        // A finished `Fuse` is pending forever, so waiting on one would sit out
        // the whole grace period.
        if self.connection.is_terminated() {
            return;
        }
        let connection = &mut self.connection;
        // Built inside the runtime: a timer outside one has no clock to read.
        let _ = self
            .runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(1), connection).await });
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise. A
/// profile runs one query at a time by design; revisit only if concurrent
/// statements per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    client: Arc<Mutex<Client>>,
    /// Taken in `open`, off the client, before it goes behind the mutex -- see
    /// [`super::Connection::cancel`]. It is plain data (address, backend PID,
    /// secret key), so it holds no connection open and needs no password: the
    /// cancel opens its own throwaway socket.
    cancel: CancelToken,
    /// The same connector the client was built with, because cancelling opens a
    /// second socket to the same server and an `sslmode` is never quietly
    /// weakened for it either (AGENTS.md, hard rule 7).
    connector: Option<tls::MakeRustlsConnect>,
    /// The server process behind the client, for asking after the statement
    /// it is running from another connection. `None` if it would not say.
    pid: Option<i32>,
    /// Held so ssh runs as long as any clone does. The cancel token dials the
    /// address the client connected to, so a cancel goes through it too.
    tunnel: Option<Arc<Tunnel>>,
    /// What [`Self::sizes`] opens its own connection with. Kept for the reason
    /// MySQL keeps its copy: the password is already held by the live client,
    /// and nothing above `src/db/` may learn which engines want a second one
    /// (AGENTS.md, hard rule 4).
    server: ServerConfig,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        tunnelled(server, DEFAULT_PORT, |tunnel| Self::connect(server, tunnel))
    }

    fn connect(server: &ServerConfig, tunnel: Option<Arc<Tunnel>>) -> Result<Self, DbError> {
        // Two branches rather than a boxed connector: `Client::connect` is
        // generic over it, and `NoTls` is a distinct type whose entire purpose
        // is to refuse. Building one at all is what `disable` means.
        let connector = tls::connector(server.sslmode, server.root_certificate.as_deref())
            .map_err(|message| DbError {
                message,
                position: None,
            })?;
        let string = match &tunnel {
            None => connection_string(server),
            Some(tunnel) => tunnelled_string(server, tunnel.dial()?),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| plain_error(format!("Could not start the connection: {error}")))?;
        let (client, connection) = match &connector {
            None => runtime
                .block_on(tokio_postgres::connect(&string, NoTls))
                .map(|(client, connection)| (client, Box::pin(connection) as Socket)),
            Some(connector) => runtime
                .block_on(tokio_postgres::connect(&string, connector.clone()))
                .map(|(client, connection)| (client, Box::pin(connection) as Socket)),
        }
        .map_err(|error| connect_error(&error, server))?;
        let mut client = Client {
            client,
            driver: Driver {
                runtime,
                connection: connection.fuse(),
            },
        };
        let pid = client
            .driver
            .block_on(
                client
                    .client
                    .simple_query("SELECT pg_catalog.pg_backend_pid()"),
            )
            .ok()
            .and_then(Result::ok)
            .and_then(|messages| {
                messages.into_iter().find_map(|message| match message {
                    SimpleQueryMessage::Row(row) => row.get(0)?.parse().ok(),
                    _ => None,
                })
            });

        Ok(Self {
            cancel: client.client.cancel_token(),
            pid,
            connector,
            client: Arc::new(Mutex::new(client)),
            tunnel,
            server: server.clone(),
        })
    }

    /// A poisoned mutex counts: every later statement would fail on it too.
    pub fn is_lost(&self) -> bool {
        self.client
            .lock()
            .map_or(true, |client| client.client.is_closed())
    }

    /// Cancellation is advisory and racy by the driver's own admission: the
    /// server reports nothing about whether the request landed, and it may
    /// arrive after the statement has already finished. So `Ok` here means the
    /// request was delivered, never that anything stopped -- what the query
    /// eventually returned is the only account dbdelve gives of that.
    pub fn cancel(&self) -> Result<(), DbError> {
        if let Some(tunnel) = &self.tunnel {
            tunnel.dial()?;
        }
        // Its own runtime: the connection's is blocked on the very statement
        // this is here to stop.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| plain_error(format!("Could not ask the server to cancel: {error}")))?;
        match &self.connector {
            None => runtime.block_on(self.cancel.cancel_query(NoTls)),
            Some(connector) => runtime.block_on(self.cancel.cancel_query(connector.clone())),
        }
        .map_err(|error| DbError {
            message: format!("Could not ask the server to cancel: {error}"),
            position: None,
        })
    }

    /// Run one statement verbatim, keeping at most `limit` rows of its result.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. The
    /// server sends every row and the ones past the limit are read and
    /// dropped, so the statement runs to completion exactly as typed. Kept
    /// rows go to `feed` when there is one; see [`super::Connection::query`].
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, true, fetch)
    }

    /// dbdelve's own SQL. Its column types are never shown, so it does not pay
    /// for the extra round trip that learns them.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, false, Fetch::default())
    }

    fn run(&self, sql: &str, typed: bool, fetch: Fetch) -> Result<QueryResult, DbError> {
        let mut client = self.client.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;

        // Timed from here, not from the call: one client serialises a profile's
        // queries, and time spent waiting behind the catalog load is not time
        // the server spent on this statement.
        let started = Instant::now();
        let Client {
            client: inner,
            driver,
        } = &mut *client;
        let failed = |error: Error| query_error(&error, sql);
        let mut messages = Box::pin(
            driver
                .block_on(inner.simple_query_raw(sql))
                .map_err(failed)?
                .map_err(failed)?,
        );
        if typed && let Some(feed) = fetch.feed {
            self.describe_aside(sql, feed);
        }
        let mut assembly = Assembly::new(sql, fetch);
        let stoppable = fetch.reads_only && fetch.limit.is_some();
        // Split at the first row past the limit, because whether to stop there
        // is asked on another connection, and that cannot be done from inside
        // this one's `block_on`.
        if driver
            .block_on(assembly.read(&mut messages, stoppable))
            .map_err(failed)??
        {
            assembly.stopping = self.stop_at_limit();
            driver
                .block_on(assembly.read(&mut messages, false))
                .map_err(failed)??;
        }
        drop(messages);
        let (mut result, commands) = assembly.finish(started);

        // Types are learned after the statement ran, and only from a single
        // statement that returned columns. A refused `Parse` is an error
        // inside the session's open transaction and aborts it, so describing
        // first meant dbdelve's own probe ended the user's transaction and every
        // later failure reported "current transaction is aborted" instead of
        // its cause. Postgres refuses to prepare more than one statement at a
        // time, which is exactly the case that used to do the damage.
        //
        // A single statement `simple_query` accepts but `prepare` refuses --
        // some utility statements -- can still abort an open transaction. That
        // is the whole of the remaining hole, and the driver exposes no
        // transaction state to guard it with.
        // A statement stopped at the limit never completed, so it counts no
        // command; it is still the one statement that returned these columns.
        let single = commands == 1 || (commands == 0 && result.stopped.is_some());
        let mut probed = if typed && !result.columns.is_empty() && single {
            describe_columns(&mut client, sql)
        } else {
            Vec::new()
        };
        // Everything read from the description is positional, and it arrived on
        // its own round trip. If the two disagree on how many columns there
        // are, nothing it says can be trusted -- not a type, and not a column's
        // identity, which would point an edit at the wrong column.
        if probed.len() != result.columns.len() {
            probed.clear();
        }
        apply_types(&mut result.columns, &probed);
        format_spatial_cells(&mut result);

        // The client mutex is not reentrant and `edit_target` runs its catalog
        // query through it, so the lock has to go before the question is asked.
        drop(client);
        result.edit = self.edit_target(&probed);

        Ok(result)
    }

    /// Which table these columns can be written back to, if any.
    ///
    /// Every step is allowed to answer "no": an unprepared statement, a join, a
    /// computed column, a table without a primary key, a key the select omitted.
    /// A failure answers "no" too — this runs after the user's statement
    /// already succeeded, and a catalog query refused inside a transaction the
    /// user has aborted must not turn that success into an error.
    fn edit_target(&self, probed: &[ProbedColumn]) -> Option<EditTarget> {
        let table = sole_table(probed)?;
        let catalog = self
            .internal_query(&PRIMARY_KEY_SQL.replace("{oid}", &table.to_string()))
            .ok()?;
        // ponytail: one catalog round trip per result set, no cache. A map from
        // oid to key held on the connection is the upgrade path if the trip
        // shows up in query timings.
        resolve_edit_target(probed, &keyed_table(&catalog)?)
    }

    /// Learn the statement's column types while it runs, so its rows can be
    /// shown by type -- numbers aligned, geometry as text -- as they stream in
    /// rather than once it is done. On a connection of its own: a describe this session's server
    /// refuses aborts the transaction it is in (see [`Self::run`]), and
    /// another session has none of the user's to abort. One that cannot see
    /// what this session sees -- a temporary table, a `SET search_path` --
    /// learns nothing, and the rows are rendered at the end as before.
    ///
    /// ponytail: a thread and a connection per streamed run. A side
    /// connection kept open is the upgrade path, shared with `stop_at_limit`.
    fn describe_aside(&self, sql: &str, feed: &Feed) {
        let (this, sql, feed) = (self.clone(), sql.to_string(), feed.clone());
        std::thread::spawn(move || {
            let Ok(side) = Self::connect(&this.server, this.tunnel.clone()) else {
                return;
            };
            let Ok(mut client) = side.client.lock() else {
                return;
            };
            let types: Vec<String> = describe_columns(&mut client, &sql)
                .into_iter()
                .map(|column| column.type_name)
                .collect();
            feed.describe(types);
        });
    }

    /// Stop the statement in flight now that it has passed the row limit, if
    /// that undoes nothing: `true` when a cancel went out.
    ///
    /// Only a statement that has written nothing, outside any transaction the
    /// user opened. A cancel aborts the transaction it lands in, so stopping
    /// one inside `BEGIN` would throw the user's open work away, and stopping
    /// a write would leave it half done. In the implicit transaction of a
    /// lone statement, `xact_start` is `query_start` to the microsecond. Asked
    /// on a connection of its own, since this one is busy with the rows.
    ///
    /// A cancel can land after the statement finished on its own; on an idle
    /// session the server ignores it, which is the same race Cancel has.
    ///
    /// ponytail: a connection opened per stop. A side connection kept open is
    /// the upgrade path if the connect shows up in query timings.
    fn stop_at_limit(&self) -> bool {
        let Some(pid) = self.pid else {
            return false;
        };
        let Ok(side) = Self::connect(&self.server, self.tunnel.clone()) else {
            return false;
        };
        let safe = side
            .internal_query(&format!(
                "SELECT xact_start = query_start AND backend_xid IS NULL
                 FROM pg_catalog.pg_stat_activity
                 WHERE pid = {pid} AND state = 'active'"
            ))
            .is_ok_and(|result| {
                result.rows.first().and_then(|row| row.first()) == Some(&Some("t".to_string()))
            });
        safe && self.cancel().is_ok()
    }

    pub fn databases(&self) -> Result<super::Databases, DbError> {
        assemble_databases(&self.internal_query(DATABASES_SQL)?)
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        assemble_catalog(self.internal_query(RELATIONS_SQL)?, QueryResult::default())
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        assemble_catalog(QueryResult::default(), self.internal_query(ROUTINES_SQL)?)
    }

    /// On a connection of its own, opened exactly as this one was, so a size
    /// waiting on a relation's lock never holds the mutex a user's query waits
    /// on. The side connection is dropped on return.
    pub fn sizes(&self) -> Result<Sizes, DbError> {
        let side = Self::connect(&self.server, self.tunnel.clone())?;
        assemble_sizes(side.internal_query(SIZES_SQL)?)
    }

    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        assemble_references(&self.internal_query(&structure_sql(
            REFERENCES_SQL,
            schema,
            relation,
        ))?)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let columns =
            self.internal_query(&structure_sql(STRUCTURE_COLUMNS_SQL, schema, relation))?;
        let indexes =
            self.internal_query(&structure_sql(STRUCTURE_INDEXES_SQL, schema, relation))?;
        let constraints =
            self.internal_query(&structure_sql(STRUCTURE_CONSTRAINTS_SQL, schema, relation))?;
        let keys =
            self.internal_query(&structure_sql(STRUCTURE_FOREIGN_KEYS_SQL, schema, relation))?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&keys)?;
        Ok(structure)
    }

    /// A view's options are its `WITH (…)`, its check option among them.
    ///
    /// ponytail: a partition is written as a table of its own, without its
    /// `PARTITION OF … FOR VALUES`; a foreign table's per-column options are
    /// left out; a `serial` column's `nextval('…_seq'::regclass)` default names
    /// a sequence nothing here creates; a materialized view is written as
    /// populated. `relpartbound`, `attfdwoptions`, `pg_get_serial_sequence`
    /// and `relispopulated` are where each is.
    pub fn ddl(&self, schema: &str, relation: &str, kind: RelationKind) -> Result<String, DbError> {
        let name = Engine::Postgres.qualified(schema, relation);
        let view = match kind {
            RelationKind::View => Some("VIEW"),
            RelationKind::MaterializedView => Some("MATERIALIZED VIEW"),
            RelationKind::Table | RelationKind::PartitionedTable | RelationKind::ForeignTable => {
                None
            }
        };
        if let Some(view) = view {
            let result = self.internal_query(&format!(
                "SELECT pg_catalog.pg_get_viewdef(class.oid, true) AS definition,
                     pg_catalog.array_to_string(class.reloptions, ', ') AS options
                 FROM pg_catalog.pg_class AS class
                 WHERE class.oid = {}::regclass",
                Engine::Postgres.quote_literal(&name)
            ))?;
            let row = result.rows.first().map(Vec::as_slice).unwrap_or_default();
            let definition = required_cell(&result, row, "definition")?;
            let options = match optional_cell(&result, row, "options") {
                Some(options) if !options.is_empty() => format!(" WITH ({options})"),
                _ => String::new(),
            };
            let mut statements = vec![terminated(&format!(
                "CREATE {view} {name}{options} AS\n{definition}"
            ))];
            if kind == RelationKind::MaterializedView {
                let indexes =
                    self.internal_query(&structure_sql(STRUCTURE_INDEXES_SQL, schema, relation))?;
                for row in &indexes.rows {
                    statements.push(terminated(required_cell(&indexes, row, "definition")?));
                }
            }
            return Ok(statements.join("\n"));
        }

        let structure = self.structure(schema, relation)?;
        let storage = self.internal_query(&structure_sql(TABLE_STORAGE_SQL, schema, relation))?;
        if storage.rows.is_empty() {
            return Err(plain_error(format!("{schema} has no relation {relation}.")));
        }
        let first = storage.rows.first().map(Vec::as_slice).unwrap_or_default();
        let (head, tail) = match (
            optional_cell(&storage, first, "partition_key"),
            optional_cell(&storage, first, "foreign_server"),
        ) {
            (_, Some(server)) => {
                let options: Vec<String> = storage
                    .rows
                    .iter()
                    .filter_map(|row| {
                        Some(format!(
                            "{} {}",
                            Engine::Postgres.quote_identifier(optional_cell(
                                &storage,
                                row,
                                "option_name"
                            )?),
                            Engine::Postgres.quote_literal(optional_cell(
                                &storage,
                                row,
                                "option_value"
                            )?),
                        ))
                    })
                    .collect();
                let server = Engine::Postgres.quote_identifier(server);
                let tail = match options.is_empty() {
                    true => format!(" SERVER {server}"),
                    false => format!(" SERVER {server} OPTIONS ({})", options.join(", ")),
                };
                ("FOREIGN TABLE", tail)
            }
            (Some(key), None) => ("TABLE", format!(" PARTITION BY {key}")),
            (None, None) => ("TABLE", String::new()),
        };
        Ok(create_table(
            Engine::Postgres,
            &format!("CREATE {head} {name}"),
            &structure,
            &tail,
            |index| index.definition.clone(),
        ))
    }
}

/// One column as the server describes it: its type, and where it came from.
///
/// The provenance is plumbing between the probe and [`resolve_edit_target`].
/// Neither the table oid nor the attribute number leaves this module.
struct ProbedColumn {
    type_name: String,
    /// The oid of the table the value was read from, absent for a computed
    /// column. The driver already reports the wire protocol's zero as absent.
    table: Option<u32>,
    /// Postgres's own identity for the column within that table.
    attribute: Option<i16>,
}

/// What each column a statement would return is, in order.
///
/// The simple query protocol hands back every value as text and describes none
/// of it, so the only way to know that a column of digits is an `int8` and not
/// a `numeric` — or that it is `accounts.id` rather than an expression — is to
/// ask separately. Preparing the statement asks: the server parses and plans it
/// and answers with the row description, and it never executes anything —
/// Postgres refuses to prepare the utility statements that would have an
/// effect, and refuses more than one statement at a time. Both refusals arrive
/// here as "nothing known" -- honest, but not free: see [`Connection::run`] for
/// why the ask has to come after the statement ran.
fn describe_columns(client: &mut Client, sql: &str) -> Vec<ProbedColumn> {
    client
        .driver
        .block_on(client.client.prepare(sql))
        .ok()
        .and_then(Result::ok)
        .map(|statement| {
            statement
                .columns()
                .iter()
                .map(|column| ProbedColumn {
                    type_name: column.type_().name().to_string(),
                    table: column.table_oid(),
                    attribute: column.column_id(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The one table every column that came from a table came from.
///
/// A result set spanning two of them is a join, and a row of it is not a row of
/// either table, so there is nothing to write back to. A result set spanning
/// none is entirely computed.
fn sole_table(probed: &[ProbedColumn]) -> Option<u32> {
    let mut tables = probed.iter().filter_map(|column| column.table);
    let first = tables.next()?;
    tables.all(|table| table == first).then_some(first)
}

/// One table as the catalog describes it, keyed by Postgres's own column
/// identity so the probe's provenance can be matched to a name.
struct KeyedTable {
    schema: String,
    table: String,
    columns: Vec<(i16, String)>,
    key: Vec<i16>,
}

fn keyed_table(result: &QueryResult) -> Option<KeyedTable> {
    let first = result.rows.first()?;
    let mut keyed = KeyedTable {
        schema: required_cell(result, first, "schema_name")
            .ok()?
            .to_string(),
        table: required_cell(result, first, "table_name").ok()?.to_string(),
        columns: Vec::new(),
        key: Vec::new(),
    };

    for row in &result.rows {
        let attribute: i16 = required_cell(result, row, "attribute_number")
            .ok()?
            .parse()
            .ok()?;
        keyed.columns.push((
            attribute,
            required_cell(result, row, "column_name").ok()?.to_string(),
        ));
        if required_cell(result, row, "in_primary_key").ok()? == "yes" {
            keyed.key.push(attribute);
        }
    }

    Some(keyed)
}

/// The probe's provenance decided against one table's catalog entry.
///
/// Refuses unless *every* primary key column is present in the result set: a
/// partial key matches more rows than the one the user is looking at, and an
/// empty one matches all of them.
fn resolve_edit_target(probed: &[ProbedColumn], table: &KeyedTable) -> Option<EditTarget> {
    // The caller has already established that the columns carrying a table all
    // carry the same one, so carrying a table at all means carrying this one.
    let attribute_of = |column: &ProbedColumn| column.table.and(column.attribute);
    // Two result columns reading the same table column are the two sides of a
    // self-join, and no driver reports the alias that tells them apart. The key
    // below is located by position, so it would resolve to whichever side came
    // first and write the edit at the other row's key; the same shape also
    // generates one SET clause per side for the same column. Refusing the whole
    // result set is one answer to both.
    let mut origins = HashSet::new();
    if !probed
        .iter()
        .filter_map(attribute_of)
        .all(|attribute| origins.insert(attribute))
    {
        return None;
    }
    let keys = table
        .key
        .iter()
        .map(|attribute| {
            probed
                .iter()
                .position(|column| attribute_of(column) == Some(*attribute))
        })
        .collect::<Option<Vec<usize>>>()?;
    if keys.is_empty() {
        return None;
    }

    Some(EditTarget {
        schema: table.schema.clone(),
        table: table.table.clone(),
        columns: probed
            .iter()
            .map(|column| {
                let attribute = attribute_of(column)?;
                table
                    .columns
                    .iter()
                    .find(|(number, _)| *number == attribute)
                    .map(|(_, name)| name.clone())
            })
            .collect(),
        keys,
    })
}

/// A relation name is user data and can contain a quote.
fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    template
        .replace("{schema}", &super::Engine::Postgres.quote_literal(schema))
        .replace(
            "{relation}",
            &super::Engine::Postgres.quote_literal(relation),
        )
}

/// A result read off the wire, held across the reads [`Connection::run`]
/// splits it into. Rows are taken as they arrive, so one past the limit is
/// dropped as soon as it is counted rather than held until the last one lands.
struct Assembly<'a> {
    sql: &'a str,
    fetch: Fetch<'a>,
    result: QueryResult,
    /// Statements the server completed -- one `CommandComplete` each, which is
    /// cheaper and more truthful than re-parsing the SQL to count them.
    commands: usize,
    /// The current set's rows, kept or not, and kept: a fed row is not in
    /// `result.rows` to be counted.
    returned: usize,
    kept: usize,
    /// A cancel went out for the rows past the limit, so the error the
    /// statement ends in is the limit's doing and not the statement's.
    stopping: bool,
    /// Result sets begun. The describe behind the feed's spatial columns
    /// is of the first statement, so it says nothing about any after it.
    sets: usize,
    /// Which of the first set's columns are spatial, once the feed knows.
    spatial: Option<Vec<bool>>,
}

impl<'a> Assembly<'a> {
    fn new(sql: &'a str, fetch: Fetch<'a>) -> Self {
        Self {
            sql,
            fetch,
            result: QueryResult::default(),
            commands: 0,
            returned: 0,
            kept: 0,
            stopping: false,
            sets: 0,
            spatial: None,
        }
    }

    fn begin(&mut self, columns: Vec<Column>) {
        self.sets += 1;
        if let Some(feed) = self.fetch.feed {
            feed.begin(columns.clone());
        }
        self.result.columns = columns;
    }

    /// Read until the statement is done or, with `pause`, until the first row
    /// past the limit, which is counted and dropped like every one after it.
    /// `true` when it paused.
    async fn read(
        &mut self,
        messages: &mut Pin<Box<SimpleQueryStream>>,
        pause: bool,
    ) -> Result<bool, DbError> {
        while let Some(message) = messages.next().await {
            let message = match message {
                Ok(message) => message,
                Err(error) if self.stopping && error.code() == Some(&SqlState::QUERY_CANCELED) => {
                    self.result.stopped = Some(Stopped::AtLimit);
                    return Ok(false);
                }
                Err(error) => return Err(query_error(&error, self.sql)),
            };
            match message {
                // One selection can carry several statements, and the grid
                // shows one result set -- so each new description starts the
                // kept set over and the last statement wins. Accumulating
                // across statements would put one statement's rows under
                // another's column names.
                SimpleQueryMessage::RowDescription(columns) => {
                    self.begin(
                        columns
                            .iter()
                            .map(|column| Column {
                                name: column.name().to_string(),
                                data_type: None,
                            })
                            .collect(),
                    );
                    self.result.rows.clear();
                    self.result.bytes = 0;
                    self.result.rows_affected = None;
                    (self.returned, self.kept) = (0, 0);
                }
                SimpleQueryMessage::Row(row) => {
                    self.returned += 1;
                    if self.fetch.limit.is_some_and(|limit| self.kept >= limit) {
                        if pause && self.returned == self.kept + 1 {
                            return Ok(true);
                        }
                        continue;
                    }
                    self.kept += 1;
                    // A row arriving with no description before it is not a
                    // path the driver takes today; without this the grid would
                    // render headerless and drop every value it was handed.
                    if self.result.columns.is_empty() {
                        self.begin(
                            row.columns()
                                .iter()
                                .map(|column| Column {
                                    name: column.name().to_string(),
                                    data_type: None,
                                })
                                .collect(),
                        );
                    }

                    // `get` panics on a value the driver cannot decode as
                    // UTF-8, and it would panic here holding the client mutex
                    // — poisoning it and taking the process down with it. A
                    // database whose encoding is SQL_ASCII can return such
                    // bytes for ordinary text.
                    let cells: Vec<Cell> = (0..row.len())
                        .map(|index| {
                            row.try_get(index)
                                .map(|cell| cell.map(str::to_string))
                                .map_err(|_| non_utf8_error(&self.result.columns, index))
                        })
                        .collect::<Result<_, _>>()?;

                    self.result.bytes += cells
                        .iter()
                        .filter_map(|cell| cell.as_ref().map(String::len))
                        .sum::<usize>();
                    match self.fetch.feed {
                        Some(feed) => {
                            let mut cells = cells;
                            if self.sets == 1 {
                                if self.spatial.is_none() {
                                    self.spatial = feed.types().map(|types| {
                                        types.iter().map(|name| is_spatial(name)).collect()
                                    });
                                }
                                if let Some(spatial) = &self.spatial {
                                    format_spatial_row(&mut cells, spatial);
                                }
                            }
                            feed.push(cells);
                        }
                        None => self.result.rows.push(cells),
                    }
                }
                SimpleQueryMessage::CommandComplete(count) => {
                    self.result.rows_affected = Some(count);
                    self.commands += 1;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// The result, plus the number of statements the server completed.
    fn finish(mut self, started: Instant) -> (QueryResult, usize) {
        self.result.elapsed = started.elapsed();
        // A stopped statement's count is only what had arrived, not its size.
        self.result.capped_from =
            (self.result.stopped.is_none() && self.returned > self.kept).then_some(self.returned);
        (self.result, self.commands)
    }
}

/// Matched by position, and only when the two agree on how many columns there
/// are: the description came from a separate round trip, and a type put against
/// the wrong column is worse than no type at all.
fn apply_types(columns: &mut [Column], probed: &[ProbedColumn]) {
    if probed.len() != columns.len() {
        return;
    }

    for (column, described) in columns.iter_mut().zip(probed) {
        column.data_type = Some(described.type_name.clone());
    }
}

pub(super) fn format_spatial_cells(result: &mut QueryResult) {
    let spatial = result
        .columns
        .iter()
        .map(|column| is_spatial(column.data_type.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>();
    if !spatial.contains(&true) {
        return;
    }
    for row in &mut result.rows {
        format_spatial_row(row, &spatial);
    }
}

fn is_spatial(type_name: &str) -> bool {
    matches!(type_name, "geometry" | "geography")
}

/// A value that is not hex EWKB is left alone, which is what lets rows already
/// rendered as they streamed in go through this a second time.
fn format_spatial_row(row: &mut [Cell], spatial: &[bool]) {
    for (cell, spatial) in row.iter_mut().zip(spatial) {
        if !spatial {
            continue;
        }
        let Some(value) = cell else {
            continue;
        };
        let Ok(bytes) = hex::decode(&*value) else {
            continue;
        };
        let Ok(wkt) = Ewkb(bytes).to_wkt_ndim(CoordDimensions::xyzm()) else {
            continue;
        };
        *value = readable_wkt(&wkt);
    }
}

fn readable_wkt(wkt: &str) -> String {
    let mut readable = String::with_capacity(wkt.len() + 8);
    for character in wkt.chars() {
        if character == '('
            && readable
                .chars()
                .next_back()
                .is_some_and(char::is_alphabetic)
        {
            readable.push(' ');
        }
        readable.push(character);
        if character == ',' {
            readable.push(' ');
        }
    }
    readable
}

fn connect_error(error: &tokio_postgres::Error, server: &ServerConfig) -> DbError {
    // A refused connection is the most common failure by a wide margin, and the
    // driver's own wording buries the endpoint. Say what happened, and nothing
    // about what the user should do -- we cannot see their machine.
    if let Some(io) = io_source(error)
        && io.kind() == std::io::ErrorKind::ConnectionRefused
    {
        return DbError {
            message: format!(
                "Connection refused: nothing is listening on {}",
                server.endpoint()
            ),
            position: None,
        };
    }

    DbError {
        message: describe(error),
        position: None,
    }
}

fn query_error(error: &tokio_postgres::Error, sql: &str) -> DbError {
    let position = error.as_db_error().and_then(|db| match db.position() {
        // Postgres reports a 1-based character position into the statement.
        Some(tokio_postgres::error::ErrorPosition::Original(p)) => {
            character_position_to_byte_offset(sql, *p)
        }
        _ => None,
    });

    DbError {
        message: describe(error),
        position,
    }
}

fn character_position_to_byte_offset(sql: &str, position: u32) -> Option<usize> {
    let character_index = position.checked_sub(1)? as usize;
    sql.char_indices()
        .map(|(byte_offset, _)| byte_offset)
        .chain(std::iter::once(sql.len()))
        .nth(character_index)
}

/// Prefer the server's own message. It is written for humans and already says
/// the useful part; the driver's wrapper text mostly repeats "db error".
fn describe(error: &tokio_postgres::Error) -> String {
    if let Some(db) = error.as_db_error() {
        let mut message = db.message().to_string();
        if let Some(detail) = db.detail() {
            message.push('\n');
            message.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            message.push('\n');
            message.push_str(hint);
        }
        return message;
    }

    // The driver's own words for a transport failure name a category and stop:
    // "error performing TLS handshake". Which handshake problem is one link
    // further down the source chain, and without it a certificate issued for
    // another host and one signed by nobody we trust read identically.
    let message = error.to_string();
    match root_cause(error).filter(|cause| !message.contains(cause.as_str())) {
        Some(cause) => format!("{message}: {cause}"),
        None => message,
    }
}

/// The last link in an error's source chain, which is where the driver's
/// wrappers finally give way to what actually went wrong.
fn root_cause(error: &tokio_postgres::Error) -> Option<String> {
    let mut source = std::error::Error::source(error);
    let mut deepest = None;
    while let Some(current) = source {
        deepest = Some(current.to_string());
        source = current.source();
    }
    deepest
}

fn io_source(error: &tokio_postgres::Error) -> Option<&std::io::Error> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        source = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ForeignKey, RelationKind, RoutineKind, SslMode, result};

    fn config() -> ServerConfig {
        ServerConfig {
            host: "db.example.test".into(),
            port: Some(8432),
            database: "dbdelve_test".into(),
            user: "someone".into(),
            password: String::new(),
            sslmode: SslMode::default(),
            root_certificate: None,
            statement_timeout: 0,
            ssh: None,
        }
    }

    /// The server the `live_` tests talk to, from the standard `PG*` variables.
    fn live_config() -> ServerConfig {
        ServerConfig {
            host: std::env::var("PGHOST").expect("PGHOST is required"),
            port: std::env::var("PGPORT")
                .ok()
                .map(|port| port.parse().expect("PGPORT must be a number")),
            database: std::env::var("PGDATABASE").expect("PGDATABASE is required"),
            user: std::env::var("PGUSER").expect("PGUSER is required"),
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
            // The compose database speaks no TLS, and these tests are the one
            // place a plaintext connection is the point.
            sslmode: SslMode::Disable,
            root_certificate: None,
            statement_timeout: 0,
            ssh: None,
        }
    }

    /// The compose database as the bastion sees it, by service name and the
    /// port inside the network.
    fn live_tunnelled(alias: &str) -> ServerConfig {
        ServerConfig {
            host: "postgres".into(),
            port: Some(5432),
            ssh: crate::db::ssh::live_bastion(alias),
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

    fn probed(columns: &[(&str, Option<u32>, Option<i16>)]) -> Vec<ProbedColumn> {
        columns
            .iter()
            .map(|(type_name, table, attribute)| ProbedColumn {
                type_name: (*type_name).to_string(),
                table: *table,
                attribute: *attribute,
            })
            .collect()
    }

    fn keyed(columns: &[(i16, &str, bool)]) -> KeyedTable {
        KeyedTable {
            schema: "public".into(),
            table: "accounts".into(),
            columns: columns
                .iter()
                .map(|(attribute, name, _)| (*attribute, (*name).to_string()))
                .collect(),
            key: columns
                .iter()
                .filter(|(_, _, in_key)| *in_key)
                .map(|(attribute, _, _)| *attribute)
                .collect(),
        }
    }

    #[test]
    fn connection_string_quotes_values() {
        let config = config();
        assert_eq!(
            connection_string(&config),
            "host='db.example.test' dbname='dbdelve_test' user='someone' port=8432 \
             sslmode=prefer connect_timeout=10"
        );
    }

    #[test]
    fn statement_timeout_rides_in_as_a_startup_option_in_milliseconds() {
        let config = ServerConfig {
            statement_timeout: 30,
            ..config()
        };
        assert!(
            connection_string(&config).contains("options='-c statement_timeout=30000'"),
            "{}",
            connection_string(&config)
        );
    }

    #[test]
    fn no_statement_timeout_says_nothing_rather_than_saying_zero() {
        // `statement_timeout=0` is how Postgres spells "no limit", so either
        // would work -- but a profile that never asked for one should connect
        // with exactly the string it connected with before the field existed.
        assert!(!connection_string(&config()).contains("options"));
    }

    #[test]
    fn connection_string_connects_a_blank_database_to_postgres() {
        let config = ServerConfig {
            database: String::new(),
            ..config()
        };
        assert!(connection_string(&config).contains("dbname='postgres'"));
        assert!(
            tunnelled_string(&config, "127.0.0.1:1".parse().unwrap()).contains("dbname='postgres'")
        );
    }

    #[test]
    fn connection_string_omits_an_unspecified_port() {
        let config = ServerConfig {
            port: None,
            ..config()
        };
        assert!(!connection_string(&config).contains("port="));
        assert_eq!(config.endpoint(), "db.example.test");
    }

    #[test]
    fn a_tunnelled_connection_verifies_the_servers_name_and_dials_the_tunnel() {
        let string = tunnelled_string(&config(), "127.0.0.1:40000".parse().unwrap());
        let parsed: tokio_postgres::Config = string.parse().expect("the driver should parse it");

        assert_eq!(parsed.get_hosts(), [Host::Tcp("db.example.test".into())]);
        assert_eq!(
            parsed.get_hostaddrs(),
            ["127.0.0.1".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(parsed.get_ports(), [40000]);
    }

    #[test]
    fn blank_password_is_omitted_not_sent_empty() {
        // An empty `password=''` is not the same as offering no password, and
        // IAM auth relies on the latter.
        let config = config();
        assert!(!connection_string(&config).contains("password"));
    }

    #[test]
    fn username_with_at_sign_survives() {
        // Cloud IAM usernames are email addresses. A bare interpolation would
        // still work here, but quoting is what keeps it working.
        let config = ServerConfig {
            user: "person@example.com".into(),
            ..config()
        };
        assert!(connection_string(&config).contains("user='person@example.com'"));
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        let config = ServerConfig {
            password: r"pa'ss\word".into(),
            ..config()
        };
        assert!(connection_string(&config).contains(r"password='pa\'ss\\word'"));
    }

    #[test]
    fn spaces_in_values_do_not_split_the_string() {
        let config = ServerConfig {
            database: "my database".into(),
            ..config()
        };
        assert!(connection_string(&config).contains("dbname='my database'"));
    }

    #[test]
    fn url_populates_fields_without_inventing_a_port() {
        let config =
            config_from_url("postgresql://person%40example.com@db.example.test/dbdelve_test")
                .unwrap();

        assert_eq!(
            config,
            ServerConfig {
                host: "db.example.test".into(),
                port: None,
                database: "dbdelve_test".into(),
                user: "person@example.com".into(),
                password: String::new(),
                sslmode: SslMode::default(),
                root_certificate: None,
                statement_timeout: 0,
                ssh: None,
            }
        );
    }

    #[test]
    fn url_preserves_an_explicit_port_and_password() {
        let config =
            config_from_url("postgres://someone:pa%20ss@db.example.test:8432/dbdelve_demo")
                .unwrap();

        assert_eq!(config.port, Some(8432));
        assert_eq!(config.password, "pa ss");
    }

    #[test]
    fn a_url_carries_its_sslmode_through_instead_of_losing_it() {
        // The whole hazard this replaced: the mode used to be dropped on the
        // way in, so the rebuilt string defaulted to `prefer`, and `prefer`
        // with a connector that cannot do TLS returns a plaintext socket
        // without even sending an SSLRequest. A demand for encryption became a
        // cleartext password and a UI that said Connected.
        for (mode, expected) in [
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            let url = format!("postgresql://someone@db.example.test/dbdelve_test?sslmode={mode}");
            assert_eq!(config_from_url(&url).unwrap().sslmode, expected, "{url}");
        }
    }

    #[test]
    fn the_two_keys_dbdelve_owns_are_kept_away_from_the_drivers_parser() {
        // The driver knows neither `verify-full` nor `sslrootcert` and refuses
        // the whole string for either, naming nothing useful. Both have to be
        // taken out of the URL before it reaches that parser, and everything
        // else has to survive the round trip.
        let config = config_from_url(
            "postgresql://someone@db.example.test:5433/dbdelve_test\
             ?sslmode=verify-full&sslrootcert=/tmp/rds.pem&application_name=dbdelve",
        )
        .unwrap();

        assert_eq!(config.sslmode, SslMode::VerifyFull);
        assert_eq!(config.root_certificate.as_deref(), Some("/tmp/rds.pem"));
        assert_eq!(config.port, Some(5433));
        assert_eq!(config.user, "someone");
    }

    #[test]
    fn a_url_asking_for_a_mode_dbdelve_cannot_honour_says_so() {
        assert!(
            config_from_url("postgresql://someone@db.example.test/dbdelve_test?sslmode=allow")
                .unwrap_err()
                .contains("allow")
        );
    }

    #[test]
    fn a_url_port_parameter_wins_over_the_port_in_the_host() {
        // The driver fills the port from the host section first -- 5432 when
        // there is none -- and pushes `?port=` after it, so reading the first
        // entry connected to a different server than the URL named.
        for (url, expected) in [
            ("postgresql://someone@db.example.test/dbdelve_test", None),
            (
                "postgresql://someone@db.example.test:5433/dbdelve_test",
                Some(5433),
            ),
            (
                "postgresql://someone@db.example.test/dbdelve_test?port=6000",
                Some(6000),
            ),
            (
                "postgresql://someone@db.example.test:5433/dbdelve_test?port=6000",
                Some(6000),
            ),
        ] {
            assert_eq!(config_from_url(url).unwrap().port, expected, "{url}");
        }
    }

    #[test]
    fn the_driver_is_only_ever_handed_a_mode_it_knows() {
        // dbdelve's five rungs collapse to the driver's three on the wire; the
        // rest is the verifier's job. A word the driver does not know fails
        // its parse and takes the whole connection with it.
        for (mode, expected) in [
            (SslMode::Disable, "sslmode=disable"),
            (SslMode::Prefer, "sslmode=prefer"),
            (SslMode::Require, "sslmode=require"),
            (SslMode::VerifyCa, "sslmode=require"),
            (SslMode::VerifyFull, "sslmode=require"),
        ] {
            let config = ServerConfig {
                sslmode: mode,
                ..config()
            };
            let string = connection_string(&config);
            assert!(string.contains(expected), "{mode:?} produced {string}");
            // And it parses: this is the check that would have caught handing
            // the driver "verify-full".
            assert!(
                string.parse::<tokio_postgres::Config>().is_ok(),
                "{mode:?} produced an unparsable string: {string}"
            );
        }
    }

    #[test]
    fn server_character_positions_become_utf8_byte_offsets() {
        let sql = "SELECT 'é', broken";
        let broken_byte_offset = sql.find("broken").unwrap();
        let broken_character_position = sql[..broken_byte_offset].chars().count() as u32 + 1;
        assert_eq!(
            character_position_to_byte_offset(sql, broken_character_position),
            Some(broken_byte_offset)
        );
    }

    #[test]
    fn invalid_server_character_positions_are_rejected() {
        assert_eq!(character_position_to_byte_offset("SELECT 1", 0), None);
        assert_eq!(character_position_to_byte_offset("SELECT 1", 100), None);
    }

    #[test]
    fn postgis_ewkb_is_rendered_as_wkt_only_for_spatial_columns() {
        let ewkb = "0103000020E610000001000000040000003B6F63B323A258C0A4AA09A2EE833D40A29BFD8172A158C0C153C8957A823D4026FDBD141EA258C045A165DD3F823D403B6F63B323A258C0A4AA09A2EE833D40";
        let mut result = QueryResult {
            columns: vec![
                Column {
                    name: "shape".into(),
                    data_type: Some("geometry".into()),
                },
                Column {
                    name: "raw".into(),
                    data_type: Some("text".into()),
                },
            ],
            rows: vec![vec![Some(ewkb.into()), Some(ewkb.into())]],
            ..Default::default()
        };

        format_spatial_cells(&mut result);

        assert_eq!(
            result.rows[0][0].as_deref(),
            Some(
                "POLYGON ((-98.533429 29.51536, -98.522614 29.509683, -98.533086 29.508787, -98.533429 29.51536))"
            )
        );
        assert_eq!(result.rows[0][1].as_deref(), Some(ewkb));
    }

    #[test]
    fn invalid_spatial_text_is_left_visible() {
        let mut result = QueryResult {
            columns: vec![Column {
                name: "shape".into(),
                data_type: Some("geometry".into()),
            }],
            rows: vec![vec![Some("not ewkb".into())]],
            ..Default::default()
        };

        format_spatial_cells(&mut result);

        assert_eq!(result.rows[0][0].as_deref(), Some("not ewkb"));
    }

    #[test]
    fn structure_sql_quotes_a_name_containing_a_quote() {
        let sql = structure_sql(
            "WHERE namespace.nspname = {schema} AND class.relname = {relation}",
            "public",
            "odd'name",
        );

        assert_eq!(
            sql,
            "WHERE namespace.nspname = 'public' AND class.relname = 'odd''name'"
        );
    }

    #[test]
    fn a_describe_disagreeing_on_the_column_count_types_nothing() {
        let mut two = result(&["id", "label"], &[]);

        apply_types(&mut two.columns, &probed(&[("int4", None, None)]));
        assert_eq!(types(&two), vec![None, None]);

        apply_types(
            &mut two.columns,
            &probed(&[
                ("int4", None, None),
                ("text", None, None),
                ("bool", None, None),
            ]),
        );
        assert_eq!(types(&two), vec![None, None]);

        apply_types(
            &mut two.columns,
            &probed(&[("int4", None, None), ("text", None, None)]),
        );
        assert_eq!(types(&two), vec![Some("int4"), Some("text")]);
    }

    #[test]
    fn only_a_result_set_reading_one_table_has_a_table_to_write_back_to() {
        // A join's row is a row of neither table, and an all-computed result
        // set is a row of nothing. Accepting either would send an UPDATE at a
        // table the user never named.
        assert_eq!(
            sole_table(&probed(&[
                ("int4", Some(42), Some(1)),
                ("text", Some(42), Some(3)),
                ("int8", None, None),
            ])),
            Some(42)
        );
        assert_eq!(
            sole_table(&probed(&[
                ("int4", Some(42), Some(1)),
                ("text", Some(77), Some(1)),
            ])),
            None
        );
        assert_eq!(sole_table(&probed(&[("int8", None, None)])), None);
        assert_eq!(sole_table(&[]), None);
    }

    #[test]
    fn an_edit_target_names_real_columns_and_locates_the_whole_key() {
        // The grid holds the aliases the user typed; an UPDATE has to name the
        // columns the table actually has. A computed column has none.
        let target = resolve_edit_target(
            &probed(&[
                ("int4", Some(42), Some(1)),
                ("int8", None, None),
                ("text", Some(42), Some(3)),
            ]),
            &keyed(&[(1, "id", true), (2, "email", false), (3, "name", false)]),
        )
        .expect("one table with its key present is editable");

        assert_eq!(target.schema, "public");
        assert_eq!(target.table, "accounts");
        assert_eq!(
            target.columns,
            vec![Some("id".to_string()), None, Some("name".to_string())]
        );
        assert_eq!(target.keys, vec![0]);
    }

    #[test]
    fn a_composite_key_reports_every_column_it_is_made_of() {
        let target = resolve_edit_target(
            &probed(&[
                ("text", Some(42), Some(3)),
                ("int4", Some(42), Some(1)),
                ("int4", Some(42), Some(2)),
            ]),
            &keyed(&[(1, "id", true), (2, "revision", true), (3, "name", false)]),
        )
        .expect("both key columns are present");

        // Positions of the key in the *result*, not in the table -- the caller
        // reads cells by result column.
        assert_eq!(target.keys, vec![1, 2]);
    }

    #[test]
    fn a_self_join_is_not_editable() {
        // Both sides carry the same oid, so `sole_table` sees one table and the
        // key resolves to the first side -- an edit typed on b.name would be
        // written at a's id.
        assert_eq!(
            resolve_edit_target(
                &probed(&[
                    ("int4", Some(42), Some(1)),
                    ("text", Some(42), Some(3)),
                    ("text", Some(42), Some(3)),
                ]),
                &keyed(&[(1, "id", true), (3, "name", false)]),
            ),
            None
        );
    }

    #[test]
    fn a_result_set_missing_part_of_the_key_is_not_editable() {
        // Half a composite key matches more rows than the one on screen, and
        // no key at all matches every row in the table.
        assert_eq!(
            resolve_edit_target(
                &probed(&[("int4", Some(42), Some(1)), ("text", Some(42), Some(3))]),
                &keyed(&[(1, "id", true), (2, "revision", true), (3, "name", false)]),
            ),
            None
        );
        assert_eq!(
            resolve_edit_target(
                &probed(&[("text", Some(42), Some(3))]),
                &keyed(&[(1, "id", true), (3, "name", false)]),
            ),
            None
        );
        assert_eq!(
            resolve_edit_target(
                &probed(&[("int4", Some(42), Some(1))]),
                &keyed(&[(1, "id", false)]),
            ),
            None
        );
    }

    /// What each mode does against a server with no TLS configured, which is
    /// what the compose development database is.
    ///
    /// This is the test the old `NoTls` code could not have passed. `prefer`
    /// now negotiates and falls back, so it still connects; `require` and above
    /// refuse to be talked down to plaintext and say why. Before, every mode
    /// silently produced the same cleartext socket.
    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_only_the_modes_that_tolerate_plaintext_reach_a_server_without_tls() {
        let connect = |sslmode| {
            Connection::open(&ServerConfig {
                sslmode,
                ..live_config()
            })
        };

        assert!(connect(SslMode::Disable).is_ok());
        assert!(connect(SslMode::Prefer).is_ok());

        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let Err(error) = connect(mode) else {
                panic!("{mode:?} must not fall back to plaintext");
            };
            assert!(
                error.message.to_lowercase().contains("tls"),
                "{mode:?} failed without saying TLS was the reason: {}",
                error.message
            );
        }
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_cancel_stops_a_sleeping_statement_without_closing_the_connection() {
        // The handle has to have been taken in `open`: asking the client for a
        // cancel token here would want the mutex the sleeping statement is
        // holding, and this would hang rather than fail.
        let connection = Connection::open(&live_config()).expect("connection should open");
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the cancel request should send");
        });

        let error = connection
            .query("SELECT pg_sleep(30)", Fetch::default())
            .expect_err("the statement should be cancelled");
        assert!(
            error.message.contains("cancel"),
            "the server's own words: {}",
            error.message
        );
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_terminated_session_is_lost_and_a_failed_statement_is_not() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query("BEGIN", Fetch::default())
            .expect("the transaction should open");
        assert!(connection.query("SELECT broken", Fetch::default()).is_err());
        assert!(
            !connection.is_lost(),
            "an aborted transaction is still a session"
        );
        connection
            .query("ROLLBACK", Fetch::default())
            .expect("the session should still answer");

        let pid = connection
            .query("SELECT pg_backend_pid()", Fetch::default())
            .unwrap()
            .rows[0][0]
            .clone()
            .unwrap();
        Connection::open(&live_config())
            .expect("a second connection should open")
            .query(
                &format!("SELECT pg_terminate_backend({pid})"),
                Fetch::default(),
            )
            .expect("the terminate should run");
        assert!(connection.query("SELECT 1", Fetch::default()).is_err());
        assert!(connection.is_lost());
    }

    #[test]
    #[ignore = "requires the dev bastions and Postgres configured through dbdelve_SSH_CONFIG and PG*"]
    fn live_ssh_a_query_runs_through_the_bastion() {
        let connection =
            Connection::open(&live_tunnelled("dbdelve-bastion")).expect("connection should open");
        let result = connection
            .query("SELECT 1 AS one", Fetch::default())
            .expect("query should succeed");
        assert_eq!(result.rows, vec![vec![Some("1".into())]]);
        let sizes = connection
            .sizes()
            .expect("sizes should load through the tunnel");
        assert!(sizes["public"].contains_key("accounts"));
    }

    #[test]
    #[ignore = "requires the dev bastions and Postgres configured through dbdelve_SSH_CONFIG and PG*"]
    fn live_ssh_a_query_runs_through_a_jump_host() {
        let connection =
            Connection::open(&live_tunnelled("dbdelve-inner")).expect("connection should open");
        let result = connection
            .query("SELECT 1 AS one", Fetch::default())
            .expect("query should succeed");
        assert_eq!(result.rows, vec![vec![Some("1".into())]]);
    }

    #[test]
    #[ignore = "requires the dev bastions and Postgres configured through dbdelve_SSH_CONFIG and PG*"]
    fn live_ssh_a_server_the_bastion_cannot_reach_fails_with_ssh_s_reason() {
        let Err(error) = Connection::open(&ServerConfig {
            host: "dbdelve-nowhere.invalid".into(),
            ..live_tunnelled("dbdelve-bastion")
        }) else {
            panic!("a server that does not resolve from the bastion connected");
        };
        assert!(
            error
                .message
                .contains("\nSSH tunnel through dbdelve-bastion: open failed: "),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires the dev bastions and Postgres configured through dbdelve_SSH_CONFIG and PG*"]
    fn live_ssh_a_cancel_reaches_the_server_through_the_tunnel() {
        let connection =
            Connection::open(&live_tunnelled("dbdelve-bastion")).expect("connection should open");
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the cancel request should send");
        });

        let error = connection
            .query("SELECT pg_sleep(30)", Fetch::default())
            .expect_err("the statement should be cancelled");
        assert!(error.message.contains("cancel"), "{}", error.message);
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_statement_timeout_stops_a_statement_that_outlasts_it() {
        let connection = Connection::open(&ServerConfig {
            statement_timeout: 1,
            ..live_config()
        })
        .expect("connection should open");

        let error = connection
            .query("SELECT pg_sleep(30)", Fetch::default())
            .expect_err("the statement should time out");
        assert!(error.message.contains("timeout"), "{}", error.message);
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_query_round_trip() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let result = connection
            .query(
                "SELECT * FROM (VALUES (1, 'alpha'), (2, NULL)) AS sample(id, label)",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "label"]);
        // Types come from describing the statement, which the simple protocol
        // itself cannot answer for. `text` for the second column is the
        // server's own answer for an unadorned literal.
        assert_eq!(types(&result), vec![Some("int4"), Some("text")]);
        assert_eq!(
            result.rows,
            vec![
                vec![Some("1".into()), Some("alpha".into())],
                vec![Some("2".into()), None]
            ]
        );
        assert_eq!(result.bytes, 7);
        assert_eq!(result.rows_affected, Some(2));
    }

    #[test]
    #[ignore = "needs a live PostgreSQL server; see live_config"]
    fn live_a_row_limit_keeps_the_first_rows_and_counts_the_rest() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let capped = connection
            .query(
                "SELECT n FROM generate_series(1, 10) AS n",
                Fetch {
                    limit: Some(3),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        assert_eq!(
            capped.rows,
            vec![
                vec![Some("1".into())],
                vec![Some("2".into())],
                vec![Some("3".into())]
            ]
        );
        assert_eq!(capped.capped_from, Some(10));
        assert_eq!(capped.total_rows(), 10);

        // The cap is per result set: the last set is the one kept, and its
        // count starts over rather than carrying the first one's.
        let last = connection
            .query(
                "SELECT generate_series(1, 10); SELECT 1 AS one",
                Fetch {
                    limit: Some(3),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        assert_eq!(last.rows, vec![vec![Some("1".into())]]);
        assert_eq!(last.capped_from, None);

        // A statement after the capped one still ran: nothing was cut short.
        let after = connection
            .query("CREATE TEMP TABLE capped AS SELECT generate_series(1, 10) AS n; SELECT n FROM capped", Fetch { limit: Some(3), ..Fetch::default() })
            .expect("query should succeed");
        assert_eq!(after.capped_from, Some(10));
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "needs a live PostgreSQL server; see live_config"]
    fn live_a_read_past_the_limit_is_stopped_rather_than_drained() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let started = Instant::now();
        let result = connection
            .query(
                // Far more rows than a drain could get through in the time
                // allowed below. In the select list, not `FROM`, where the
                // whole series is built before the first row is sent.
                "SELECT generate_series(1, 500000000) AS n",
                Fetch {
                    limit: Some(3),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .expect("a stopped read is not an error");

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.capped_from, None);
        // Described like any single statement, though it never completed.
        assert_eq!(types(&result), vec![Some("int4")]);
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    #[test]
    #[ignore = "needs a live PostgreSQL server; see live_config"]
    fn live_a_read_inside_the_users_transaction_is_drained_not_stopped() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection.query("BEGIN", Fetch::default()).unwrap();
        connection
            .query("CREATE TEMP TABLE kept AS SELECT 1 AS n", Fetch::default())
            .unwrap();
        let result = connection
            .query(
                "SELECT n FROM generate_series(1, 1000) AS n",
                Fetch {
                    limit: Some(3),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .unwrap();

        assert_eq!(result.stopped, None);
        assert_eq!(result.capped_from, Some(1000));
        // A cancel would have aborted the transaction, and this with it.
        assert!(
            connection
                .query("SELECT n FROM kept", Fetch::default())
                .is_ok()
        );
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    #[test]
    #[ignore = "needs a live PostgreSQL server; see live_config"]
    fn live_fed_rows_are_shown_by_type_once_a_describe_aside_has_said() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let feed = crate::db::Feed::default();
        connection
            .query(
                // The sleep holds every row back until the describe on the
                // other connection has had time to answer.
                "SELECT n, ST_MakePoint(n, n) AS p FROM generate_series(1, 3) AS n, pg_sleep(1)",
                Fetch {
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        let fed = feed.take();

        let types: Vec<_> = fed.columns.iter().map(|c| c.data_type.as_deref()).collect();
        assert_eq!(types, vec![Some("int4"), Some("geometry")]);
        assert!(
            fed.rows[0][1]
                .as_deref()
                .is_some_and(|cell| cell.starts_with("POINT")),
            "{:?}",
            fed.rows[0][1]
        );
    }

    #[test]
    #[ignore = "needs a live PostgreSQL server; see live_config"]
    fn live_a_fed_query_hands_its_kept_rows_to_the_feed_and_not_the_result() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let feed = crate::db::Feed::default();
        let result = connection
            .query(
                "SELECT n FROM generate_series(1, 10) AS n",
                Fetch {
                    limit: Some(3),
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        let fed = feed.take();

        assert!(result.rows.is_empty());
        assert_eq!(result.capped_from, Some(10));
        assert_eq!(fed.set, 1);
        assert_eq!(
            names(&QueryResult {
                columns: fed.columns,
                ..Default::default()
            }),
            vec!["n"]
        );
        assert_eq!(
            fed.rows,
            vec![
                vec![Some("1".into())],
                vec![Some("2".into())],
                vec![Some("3".into())]
            ]
        );
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_the_databases_list_flags_the_one_connected_to() {
        let databases = Connection::open(&live_config())
            .expect("connection should open")
            .databases()
            .expect("databases should list");

        assert!(databases.names.contains(&"dbdelve_dev".to_string()));
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_blank_database_lands_on_the_maintenance_database() {
        let databases = Connection::open(&ServerConfig {
            database: String::new(),
            ..live_config()
        })
        .expect("a blank database should connect")
        .databases()
        .expect("databases should list");

        assert_eq!(databases.current.as_deref(), Some("postgres"));
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_the_type_probe_leaves_an_open_transaction_alone() {
        // Postgres refuses to prepare more than one statement at a time, and a
        // refused `Parse` aborts the transaction it arrives in. Describing
        // before running therefore killed a selection the simple protocol
        // handles fine, and reported every later failure as "current
        // transaction is aborted" instead of its own cause.
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query("BEGIN", Fetch::default())
            .expect("BEGIN should succeed");

        let result = connection
            .query("SELECT 1 AS a; SELECT 2 AS b", Fetch::default())
            .expect("a multi-statement selection must survive an open transaction");

        assert_eq!(names(&result), vec!["b"]);
        // Undescribable, so untyped -- but the transaction is still open.
        assert_eq!(types(&result), vec![None]);

        let error = connection
            .query("SELECT * FROM no_such_relation", Fetch::default())
            .unwrap_err();

        assert!(
            error.message.contains("no_such_relation"),
            "an error must report its own cause: {}",
            error.message
        );
        connection
            .query("ROLLBACK", Fetch::default())
            .expect("ROLLBACK should succeed");
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_multi_statement_selection_keeps_one_result_shape() {
        // `assemble` cannot be unit-tested -- SimpleQueryRow and SimpleColumn
        // have no public constructor -- so the ragged-rows panic is guarded
        // here. Before the RowDescription arm existed this produced 3 columns
        // against a 1-cell row, and the grid aborted the process painting it.
        let connection = Connection::open(&live_config()).expect("connection should open");

        let result = connection
            .query(
                "SELECT 1 AS a, 2 AS b, 3 AS c; SELECT 4 AS d",
                Fetch::default(),
            )
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
            .query("SELECT 1 AS id, 'x' AS label WHERE false", Fetch::default())
            .expect("query should succeed");

        assert_eq!(names(&empty), vec!["id", "label"]);
        assert!(empty.rows.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_single_table_select_is_editable_by_its_primary_key() {
        // The whole point of the widened probe: the grid holds names the user
        // chose, in the order they asked for, and an UPDATE needs the table's
        // own names and the key's position among them.
        let result = Connection::open(&live_config())
            .expect("connection should open")
            .query("SELECT name, id FROM accounts", Fetch::default())
            .expect("query should succeed");
        let edit = result.edit.expect("accounts has a primary key");

        assert_eq!(edit.schema, "public");
        assert_eq!(edit.table, "accounts");
        assert_eq!(
            edit.columns,
            vec![Some("name".to_string()), Some("id".to_string())]
        );
        assert_eq!(edit.keys, vec![1]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_an_aliased_or_computed_column_reports_what_the_table_calls_it() {
        // An alias is the one case where the result's own column name is a lie
        // about the table, and an expression has no name in the table at all.
        let result = Connection::open(&live_config())
            .expect("connection should open")
            .query(
                "SELECT id AS ident, upper(name) AS shouted, name FROM accounts",
                Fetch::default(),
            )
            .expect("query should succeed");
        let edit = result.edit.expect("accounts has a primary key");

        assert_eq!(
            edit.columns,
            vec![Some("id".to_string()), None, Some("name".to_string())]
        );
        assert_eq!(edit.keys, vec![0]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_join_or_an_aggregate_is_not_editable() {
        // Two tables, or no table: either way the row on screen is not a row
        // of anything dbdelve could write back to.
        let connection = Connection::open(&live_config()).expect("connection should open");

        for sql in [
            "SELECT accounts.id, locations.name
             FROM accounts JOIN locations ON locations.id = accounts.id",
            "SELECT plan, count(*) FROM accounts GROUP BY plan",
            "SELECT 1 AS one",
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
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_select_omitting_the_key_is_not_editable() {
        // Nothing in this result set identifies which account a row is.
        let result = Connection::open(&live_config())
            .expect("connection should open")
            .query("SELECT name, email FROM accounts", Fetch::default())
            .expect("query should succeed");

        assert!(result.edit.is_none());
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_table_without_a_primary_key_is_not_editable() {
        // There is no predicate that names one of two identical rows.
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query(
                "CREATE TEMP TABLE dbdelve_unkeyed (value integer, label text)",
                Fetch::default(),
            )
            .expect("the temporary table should be created");

        let result = connection
            .query("SELECT value, label FROM dbdelve_unkeyed", Fetch::default())
            .expect("query should succeed");

        assert!(result.edit.is_none());
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_relation_lists_the_single_column_keys_that_point_at_it() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let references = connection
            .references("public", "accounts")
            .expect("references should load");

        assert!(references.contains(&Reference {
            schema: "public".into(),
            table: "orders".into(),
            column: "account_id".into(),
            referenced_column: "id".into(),
        }));
        assert!(
            references
                .iter()
                .all(|reference| reference.referenced_column != "number"),
            "a composite key names no single column: {references:?}"
        );
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_the_ddl_of_a_table_and_a_view_reads_back_as_their_create_statements() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_ddl; \
                 CREATE TABLE dbdelve_test_ddl (id bigint, note text DEFAULT 'x') \
                     PARTITION BY RANGE (id); \
                 CREATE INDEX dbdelve_test_ddl_note ON dbdelve_test_ddl (note)",
                Fetch::default(),
            )
            .expect("the fixture table should be created");
        let partitioned =
            connection.ddl("public", "dbdelve_test_ddl", RelationKind::PartitionedTable);
        connection
            .query("DROP TABLE dbdelve_test_ddl", Fetch::default())
            .expect("the fixture table should be cleaned up");
        let table = connection
            .ddl("public", "accounts", RelationKind::Table)
            .expect("the table's DDL should load");
        let view = connection
            .ddl("public", "account_overview", RelationKind::View)
            .expect("the view's DDL should load");

        assert!(
            table.starts_with(
                "CREATE TABLE \"public\".\"accounts\" (\n    \
                 \"id\" bigint GENERATED ALWAYS AS IDENTITY NOT NULL,"
            ),
            "{table}"
        );
        assert!(table.contains("PRIMARY KEY (id)"), "{table}");
        assert!(!table.contains("CREATE UNIQUE INDEX"), "{table}");
        assert_eq!(
            partitioned.expect("the partitioned table's DDL should load"),
            "CREATE TABLE \"public\".\"dbdelve_test_ddl\" (\n    \"id\" bigint,\n    \
             \"note\" text DEFAULT 'x'::text\n) PARTITION BY RANGE (id);\n\
             CREATE INDEX dbdelve_test_ddl_note ON ONLY public.dbdelve_test_ddl \
             USING btree (note);"
        );
        assert!(
            view.starts_with("CREATE VIEW \"public\".\"account_overview\" AS\n"),
            "{view}"
        );
        assert!(view.ends_with(';') && !view.ends_with(";;"), "{view}");
        assert_eq!(
            connection
                .ddl("public", "dbdelve_test_ddl_missing", RelationKind::Table)
                .unwrap_err()
                .message,
            "public has no relation dbdelve_test_ddl_missing."
        );
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_the_ddl_of_a_generated_column_a_checked_view_and_a_materialized_view_runs_again() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let objects = [
            ("dbdelve_test_ddl_generated", RelationKind::Table),
            ("dbdelve_test_ddl_checked", RelationKind::View),
            (
                "dbdelve_test_ddl_materialized",
                RelationKind::MaterializedView,
            ),
        ];
        let ddl = || {
            objects
                .iter()
                .map(|(name, kind)| connection.ddl("public", name, *kind))
                .collect::<Result<Vec<_>, _>>()
        };
        let drop = "DROP TABLE IF EXISTS dbdelve_test_ddl_generated CASCADE";
        connection
            .query(
                &format!(
                    "{drop}; \
                 CREATE TABLE dbdelve_test_ddl_generated (a int, \
                     twice int GENERATED ALWAYS AS (a * 2) STORED, \
                     CONSTRAINT dbdelve_test_ddl_generated_a CHECK (a > 0)); \
                 CREATE INDEX dbdelve_test_ddl_generated_a ON dbdelve_test_ddl_generated (a); \
                 CREATE VIEW dbdelve_test_ddl_checked WITH (security_barrier) AS \
                     SELECT a FROM dbdelve_test_ddl_generated WHERE a > 1 WITH LOCAL CHECK OPTION; \
                 CREATE MATERIALIZED VIEW dbdelve_test_ddl_materialized AS \
                     SELECT a FROM dbdelve_test_ddl_generated; \
                 CREATE INDEX dbdelve_test_ddl_materialized_a ON dbdelve_test_ddl_materialized (a)"
                ),
                Fetch::default(),
            )
            .expect("the fixtures should be created");
        let written = ddl();
        let rerun = written.as_ref().map_err(Clone::clone).and_then(|written| {
            connection.query(drop, Fetch::default())?;
            connection.query(&written.join("\n"), Fetch::default())?;
            ddl()
        });
        connection
            .query(drop, Fetch::default())
            .expect("the fixtures should be cleaned up");

        let written = written.expect("the DDL should load");
        assert_eq!(rerun.as_ref(), Ok(&written));
        let [table, view, materialized] = &written[..] else {
            unreachable!()
        };
        assert!(
            table.contains("\"twice\" integer GENERATED ALWAYS AS ((a * 2)) STORED,"),
            "{table}"
        );
        assert!(
            table.ends_with(
                "CREATE INDEX dbdelve_test_ddl_generated_a ON public.dbdelve_test_ddl_generated \
                 USING btree (a);"
            ),
            "{table}"
        );
        assert!(
            view.starts_with(
                "CREATE VIEW \"public\".\"dbdelve_test_ddl_checked\" \
                 WITH (security_barrier=true, check_option=local) AS\n"
            ),
            "{view}"
        );
        assert!(
            materialized.ends_with(
                ";\nCREATE INDEX dbdelve_test_ddl_materialized_a ON \
                 public.dbdelve_test_ddl_materialized USING btree (a);"
            ),
            "{materialized}"
        );
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_key_on_a_partitioned_table_is_listed_once_not_per_partition() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_referencing; \
                 CREATE TABLE dbdelve_test_referencing \
                     (account_id bigint REFERENCES accounts (id)) \
                     PARTITION BY RANGE (account_id); \
                 CREATE TABLE dbdelve_test_referencing_p1 \
                     PARTITION OF dbdelve_test_referencing FOR VALUES FROM (0) TO (500); \
                 CREATE TABLE dbdelve_test_referencing_p2 \
                     PARTITION OF dbdelve_test_referencing FOR VALUES FROM (500) TO (1000)",
                Fetch::default(),
            )
            .expect("the fixture tables should be created");

        let references = connection.references("public", "accounts");
        connection
            .query("DROP TABLE dbdelve_test_referencing", Fetch::default())
            .expect("the fixture tables should be cleaned up");
        let tables: Vec<String> = references
            .expect("references should load")
            .into_iter()
            .map(|reference| reference.table)
            .filter(|table| table.starts_with("dbdelve_test_referencing"))
            .collect();

        assert_eq!(tables, ["dbdelve_test_referencing"]);
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_a_composite_primary_key_reports_every_column_it_is_made_of() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query(
                "CREATE TEMP TABLE dbdelve_composite (
                    left_id integer,
                    right_id integer,
                    label text,
                    PRIMARY KEY (left_id, right_id)
                )",
                Fetch::default(),
            )
            .expect("the temporary table should be created");

        let edit = connection
            .query(
                "SELECT label, right_id, left_id FROM dbdelve_composite",
                Fetch::default(),
            )
            .expect("query should succeed")
            .edit
            .expect("both key columns are in the result set");

        assert_eq!(edit.table, "dbdelve_composite");
        assert!(
            edit.schema.starts_with("pg_temp"),
            "a temporary table lives in a per-session schema: {}",
            edit.schema
        );
        // Result positions, in the key's own column order.
        assert_eq!(edit.keys, vec![2, 1]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_resolving_an_edit_target_leaves_an_open_transaction_alone() {
        // The edit target costs a second round trip, on the same connection,
        // inside whatever transaction the user has open. It must be as
        // harmless there as the type probe beside it.
        let connection = Connection::open(&live_config()).expect("connection should open");
        connection
            .query("BEGIN", Fetch::default())
            .expect("BEGIN should succeed");

        assert!(
            connection
                .query("SELECT id FROM accounts", Fetch::default())
                .expect("query should succeed")
                .edit
                .is_some()
        );

        let error = connection
            .query("SELECT * FROM no_such_relation", Fetch::default())
            .unwrap_err();

        assert!(
            error.message.contains("no_such_relation"),
            "an error must report its own cause: {}",
            error.message
        );
        connection
            .query("ROLLBACK", Fetch::default())
            .expect("ROLLBACK should succeed");
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_structure_round_trip() {
        let structure = Connection::open(&live_config())
            .expect("connection should open")
            .structure("public", "accounts")
            .expect("structure should load");

        let id = structure
            .columns
            .iter()
            .find(|column| column.name == "id")
            .expect("accounts.id should exist");
        assert!(!id.nullable);
        assert_eq!(
            id.default.as_deref(),
            Some("GENERATED ALWAYS AS IDENTITY"),
            "an identity column has no pg_attrdef row to read"
        );
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "email" && column.nullable)
        );
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "email" && column.data_type == "text")
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
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_catalog_round_trip() {
        let connection = Connection::open(&live_config()).expect("connection should open");
        let mut catalog = connection.catalog().expect("catalog should load");
        catalog.merge(connection.routines().expect("routines should load"));
        let public = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "public")
            .expect("public schema should exist");

        assert!(
            public
                .relations
                .iter()
                .any(|relation| relation.name == "accounts" && relation.kind == RelationKind::Table)
        );
        assert!(public.relations.iter().any(|relation| {
            relation.name == "account_overview" && relation.kind == RelationKind::View
        }));
        assert!(
            public
                .relations
                .iter()
                .all(|relation| relation.size.is_none()),
            "sizes arrive separately"
        );

        assert!(
            public
                .routines
                .iter()
                .any(|routine| routine.name == "account_label"
                    && routine.kind == RoutineKind::Function),
            "the database's own routines must be listed"
        );
        // PostGIS installs ~750 routines into `public`. None of them belong in
        // a sidebar listing what the user wrote.
        assert!(
            !public
                .routines
                .iter()
                .any(|routine| routine.name.starts_with("st_")
                    || routine.name.starts_with("_postgis")),
            "extension-owned routines must not be listed"
        );

        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_partitioned; \
                 CREATE TABLE dbdelve_test_partitioned (id integer NOT NULL) \
                     PARTITION BY RANGE (id); \
                 CREATE TABLE dbdelve_test_partitioned_p1 \
                     PARTITION OF dbdelve_test_partitioned FOR VALUES FROM (0) TO (500); \
                 CREATE TABLE dbdelve_test_partitioned_p2 \
                     PARTITION OF dbdelve_test_partitioned FOR VALUES FROM (500) TO (1000); \
                 INSERT INTO dbdelve_test_partitioned_p1 (id) SELECT generate_series(1, 10); \
                 INSERT INTO dbdelve_test_partitioned_p2 (id) SELECT generate_series(500, 505); \
                 ANALYZE dbdelve_test_partitioned; \
                 DROP TABLE IF EXISTS dbdelve_test_unanalyzed; \
                 CREATE TABLE dbdelve_test_unanalyzed (id integer NOT NULL); \
                 INSERT INTO dbdelve_test_unanalyzed (id) SELECT generate_series(1, 10)",
                Fetch::default(),
            )
            .expect("the fixture tables should be created");

        let sizes = connection.sizes().expect("sizes should load");
        assert!(sizes["public"].contains_key("accounts"));
        assert!(!sizes["public"].contains_key("account_overview"));

        assert!(
            sizes["public"].contains_key("dbdelve_test_partitioned"),
            "a partitioned table's size must be reported"
        );
        let of = |name: &str| sizes["public"][name];
        let partition_total = of("dbdelve_test_partitioned_p1").size.unwrap()
            + of("dbdelve_test_partitioned_p2").size.unwrap();
        assert_eq!(
            of("dbdelve_test_partitioned").size,
            Some(partition_total),
            "a partitioned table's size is the sum of its partitions"
        );
        assert_eq!(
            of("dbdelve_test_partitioned").rows,
            Some(16),
            "an analyzed partitioned table's estimate is the sum of its partitions'"
        );

        assert!(
            of("dbdelve_test_unanalyzed").size.unwrap_or(0) > 0,
            "a never-analyzed table with rows must not report 0 bytes"
        );
        assert_eq!(
            of("dbdelve_test_unanalyzed").rows,
            None,
            "a never-analyzed table has no estimate, not one of -1"
        );

        connection
            .query(
                "DROP TABLE dbdelve_test_partitioned; DROP TABLE dbdelve_test_unanalyzed",
                Fetch::default(),
            )
            .expect("the fixture tables should be cleaned up");

        catalog.set_sizes(&sizes);
        assert!(
            catalog
                .schemas
                .iter()
                .filter(|schema| schema.name == "public")
                .flat_map(|schema| &schema.relations)
                .any(|relation| relation.name == "accounts" && relation.size.is_some())
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_the_foreign_key_fixtures_are_seeded() {
        // The foreign-key tests have nothing to read unless the reseed that
        // added `orders`, `order_items` and the `archive` schema has actually
        // been applied.
        let connection = Connection::open(&live_config()).expect("connection should open");

        let items = connection
            .query(
                "SELECT count(*) AS rows_seeded FROM order_items",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(items.rows[0][0].as_deref(), Some("3"));

        let closed = connection
            .query(
                "SELECT count(*) AS rows_seeded FROM archive.closed_accounts",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(closed.rows[0][0].as_deref(), Some("2"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_composite_key_reports_one_column_per_key_position_in_order() {
        let structure = Connection::open(&live_config())
            .expect("connection should open")
            .structure("public", "order_items")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![
                ForeignKey {
                    column: "order_account_id".to_string(),
                    referenced_schema: "public".to_string(),
                    referenced_table: "orders".to_string(),
                    referenced_column: "account_id".to_string(),
                },
                ForeignKey {
                    column: "order_number".to_string(),
                    referenced_schema: "public".to_string(),
                    referenced_table: "orders".to_string(),
                    referenced_column: "number".to_string(),
                },
            ],
            "conkey and confkey are parallel, and the pairing survives only in order"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_key_across_schemas_reports_the_schema_it_points_at() {
        let structure = Connection::open(&live_config())
            .expect("connection should open")
            .structure("archive", "closed_accounts")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![ForeignKey {
                column: "account_id".to_string(),
                referenced_schema: "public".to_string(),
                referenced_table: "accounts".to_string(),
                referenced_column: "id".to_string(),
            }]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_a_relation_without_a_foreign_key_reports_none() {
        let structure = Connection::open(&live_config())
            .expect("connection should open")
            .structure("public", "accounts")
            .expect("structure should load");

        assert!(structure.foreign_keys.is_empty());
    }

    #[test]
    #[ignore = "requires the repository development database configured through PG*"]
    fn live_the_structured_key_does_not_replace_the_rendered_ddl() {
        let structure = Connection::open(&live_config())
            .expect("connection should open")
            .structure("public", "order_items")
            .expect("structure should load");

        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition.starts_with("FOREIGN KEY")),
            "pg_get_constraintdef still renders the constraints tab"
        );
    }
}

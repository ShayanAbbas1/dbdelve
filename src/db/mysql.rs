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

use std::borrow::Cow;
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::mysql::consts::{ColumnFlags, ColumnType};
use ::mysql::prelude::Queryable;
use ::mysql::{Conn, OptsBuilder, SslOpts, Value};

use super::tunnel::{Tunnel, tunnelled};
use super::{
    ApplyOutcome, ApplyStage, Catalog, Column, DbError, EditTarget, Engine, Fetch, GraphCancel,
    Index, IndexKind, QueryResult, Reference, RowBuilder, RowMatch, Schema, SchemaGraph, Sending,
    ServerConfig, Sizes, SslMode, StepKind, Stopped, Structure, VerifiedStep, assemble_catalog,
    assemble_databases, assemble_foreign_keys, assemble_references, assemble_schema_graph,
    assemble_sequences, assemble_sizes, assemble_structure, non_utf8_error, optional_cell,
    plain_error, required_cell,
};

/// Without this the driver waits out the OS SYN retry budget, so a host that
/// resolves but drops packets pins the UI in "Connecting…" for minutes with no
/// cancel. The Postgres side sets the same bound for the same reason.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// The port the server listens on when the profile does not say.
pub(super) const DEFAULT_PORT: u16 = 3306;

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
/// a sidebar listing what they wrote, unless they picked one: every query that
/// leaves these out keeps the database the connection selected.
const SYSTEM_SCHEMAS: &str = "('information_schema', 'mysql', 'performance_schema', 'sys')";

/// What `TABLE_TYPE` calls a table that holds rows: MariaDB names one it keeps
/// the history of apart.
const STORED_TABLES: &str = "('BASE TABLE', 'SYSTEM VERSIONED')";

/// Every table. MariaDB's sequence is a `TABLE_TYPE` of its own and is listed
/// apart, as a sequence (`SEQUENCES_SQL`): it is not a relation rows go into.
const TABLES: &str = "('BASE TABLE', 'SYSTEM VERSIONED')";

/// `SYSTEM VIEW` is `information_schema`'s own, and listed only where it is
/// the database selected.
const VIEWS: &str = "('VIEW', 'SYSTEM VIEW')";

const DATABASES_SQL: &str = "
SELECT SCHEMA_NAME AS name, SCHEMA_NAME = DATABASE() AS is_current
FROM information_schema.SCHEMATA
ORDER BY SCHEMA_NAME";

// Apart from `RELATIONS_SQL` because a database with nothing in it has no
// relation to be listed by, and one just created is exactly that.
const SCHEMAS_SQL: &str = "
SELECT SCHEMA_NAME AS schema_name
FROM information_schema.SCHEMATA
WHERE SCHEMA_NAME NOT IN {system} OR SCHEMA_NAME = DATABASE()
";

const RELATIONS_SQL: &str = "
SELECT TABLE_SCHEMA AS schema_name,
       TABLE_NAME AS relation_name,
       CASE
           WHEN TABLE_TYPE IN {tables} THEN 'table'
           WHEN TABLE_TYPE IN {views} THEN 'view'
       END AS relation_kind
FROM information_schema.TABLES
WHERE (TABLE_SCHEMA NOT IN {system} OR TABLE_SCHEMA = DATABASE())
  AND (TABLE_TYPE IN {tables} OR TABLE_TYPE IN {views})
ORDER BY TABLE_SCHEMA, TABLE_NAME
";

// Apart from `RELATIONS_SQL` because naming `DATA_LENGTH` makes the server open
// every table for its engine's statistics, where the name and type alone come
// from the data dictionary. `TABLE_ROWS` is InnoDB's sampled estimate, from the
// same statistics.
const SIZES_SQL: &str = "
SELECT TABLE_SCHEMA AS schema_name,
       TABLE_NAME AS relation_name,
       DATA_LENGTH + INDEX_LENGTH AS size_bytes,
       TABLE_ROWS AS row_estimate
FROM information_schema.TABLES
WHERE (TABLE_SCHEMA NOT IN {system} OR TABLE_SCHEMA = DATABASE())
  AND TABLE_TYPE IN {stored}
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
WHERE (routine.ROUTINE_SCHEMA NOT IN {system} OR routine.ROUTINE_SCHEMA = DATABASE())
  AND routine.ROUTINE_TYPE IN ('FUNCTION', 'PROCEDURE')
ORDER BY routine.ROUTINE_SCHEMA, routine.ROUTINE_NAME
";

// The names alone: reading a sequence's own table for its current value is not
// needed to list it.
const SEQUENCES_SQL: &str = "
SELECT TABLE_SCHEMA AS schema_name,
       TABLE_NAME AS sequence_name
FROM information_schema.TABLES
WHERE TABLE_TYPE = 'SEQUENCE'
  AND TABLE_SCHEMA NOT IN {system}
ORDER BY 1, 2
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

// One row per key part, grouped by `indexes` rather than `GROUP_CONCAT`, which
// `group_concat_max_len` cuts short without a word. A functional key part has
// no `COLUMN_NAME`, only an `EXPRESSION`, a column MySQL grew in 8.0.13 and
// MariaDB lacks. The versioned comment reads it on such a MySQL alone: MariaDB
// skips one written for a MySQL version.
const STRUCTURE_INDEXES_SQL: &str = "
SELECT INDEX_NAME AS object_name,
       NON_UNIQUE AS non_unique,
       COLUMN_NAME AS column_name,
       CONCAT(
           COALESCE(COLUMN_NAME /*!80013 , EXPRESSION */),
           COALESCE(CONCAT('(', SUB_PART, ')'), ''),
           CASE WHEN COLLATION = 'D' THEN ' DESC' ELSE '' END
       ) AS key_part
FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = {schema}
  AND TABLE_NAME = {relation}
ORDER BY INDEX_NAME, SEQ_IN_INDEX
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

// The same view read from the referenced side: who points at this relation.
const REFERENCES_SQL: &str = "
SELECT TABLE_SCHEMA AS source_schema,
       TABLE_NAME AS source_table,
       COLUMN_NAME AS column_name,
       REFERENCED_COLUMN_NAME AS referenced_column,
       CONSTRAINT_NAME AS constraint_name
FROM information_schema.KEY_COLUMN_USAGE
WHERE REFERENCED_TABLE_SCHEMA = {schema}
  AND REFERENCED_TABLE_NAME = {relation}
ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION
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

// The ER diagram's three reads, each over a whole database. A table with no
// columns still comes back, as one row of NULLs, so it is drawn.
const GRAPH_COLUMNS_SQL: &str = "
SELECT base_table.TABLE_NAME AS table_name,
       column_entry.COLUMN_NAME AS column_name,
       column_entry.COLUMN_TYPE AS data_type,
       CASE column_entry.IS_NULLABLE WHEN 'YES' THEN 'yes' ELSE 'no' END AS nullable
FROM information_schema.TABLES AS base_table
LEFT JOIN information_schema.COLUMNS AS column_entry
    ON column_entry.TABLE_SCHEMA = base_table.TABLE_SCHEMA
    AND column_entry.TABLE_NAME = base_table.TABLE_NAME
WHERE base_table.TABLE_SCHEMA = {schema}
  AND base_table.TABLE_TYPE IN {stored}
ORDER BY base_table.TABLE_NAME, column_entry.ORDINAL_POSITION
";

// A functional part (no `COLUMN_NAME`) leaves the columns not unique in
// themselves. A prefix part (`SUB_PART`) does not: equal values have equal
// prefixes, so the full columns are unique too.
const GRAPH_UNIQUE_KEYS_SQL: &str = "
SELECT statistic.TABLE_NAME AS table_name,
       statistic.INDEX_NAME AS constraint_name,
       statistic.COLUMN_NAME AS column_name,
       CASE WHEN statistic.INDEX_NAME = 'PRIMARY' THEN 'primary' ELSE 'unique' END AS key_kind
FROM information_schema.STATISTICS AS statistic
WHERE statistic.TABLE_SCHEMA = {schema}
  AND statistic.NON_UNIQUE = 0
  AND NOT EXISTS (
      SELECT 1
      FROM information_schema.STATISTICS AS part
      WHERE part.TABLE_SCHEMA = statistic.TABLE_SCHEMA
        AND part.TABLE_NAME = statistic.TABLE_NAME
        AND part.INDEX_NAME = statistic.INDEX_NAME
        AND part.COLUMN_NAME IS NULL
  )
ORDER BY statistic.TABLE_NAME, statistic.INDEX_NAME, statistic.SEQ_IN_INDEX
";

const GRAPH_FOREIGN_KEYS_SQL: &str = "
SELECT TABLE_NAME AS table_name,
       CONSTRAINT_NAME AS constraint_name,
       COLUMN_NAME AS column_name,
       REFERENCED_TABLE_SCHEMA AS referenced_schema,
       REFERENCED_TABLE_NAME AS referenced_table,
       REFERENCED_COLUMN_NAME AS referenced_column
FROM information_schema.KEY_COLUMN_USAGE
WHERE TABLE_SCHEMA = {schema}
  AND REFERENCED_TABLE_NAME IS NOT NULL
ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION
";

/// The storage engine of a table whose engine cannot roll back: no row when
/// it can, or when it has none (a view). Read from the server's own list of
/// engines rather than a list of names: MyISAM, Aria and MEMORY answer `NO`.
const UNTRANSACTIONAL_SQL: &str = "
SELECT t.ENGINE AS engine
FROM information_schema.TABLES AS t
JOIN information_schema.ENGINES AS e ON e.ENGINE = t.ENGINE
WHERE t.TABLE_SCHEMA = {schema}
  AND t.TABLE_NAME = {relation}
  AND e.TRANSACTIONS <> 'YES'
";

/// The primary key's columns, one row each in key order, and on every row the
/// storage engine of the table when it cannot roll back (NULL when it can, or
/// for a view). One round trip answers both: a table with no key still comes
/// back as one row, with a NULL `column_name`. The engine is read from the
/// server's own list of engines rather than a list of names: MyISAM, Aria and
/// MEMORY answer `NO`.
const PRIMARY_KEY_SQL: &str = "
SELECT s.COLUMN_NAME AS column_name,
       IF(e.TRANSACTIONS <> 'YES', t.ENGINE, NULL) AS untransactional_engine
FROM information_schema.TABLES AS t
LEFT JOIN information_schema.ENGINES AS e ON e.ENGINE = t.ENGINE
LEFT JOIN information_schema.STATISTICS AS s
  ON s.TABLE_SCHEMA = t.TABLE_SCHEMA
  AND s.TABLE_NAME = t.TABLE_NAME
  AND s.INDEX_NAME = 'PRIMARY'
WHERE t.TABLE_SCHEMA = {schema}
  AND t.TABLE_NAME = {relation}
ORDER BY s.SEQ_IN_INDEX
";

/// What a table with no primary key is edited by: each of its columns, the type
/// the catalog names and whether the server computes it. Answers for a base
/// table alone; a view has no row of its own to name by its values.
///
/// `{data_type}` is [`edit_columns_sql`]'s to fill: the catalog's own type name,
/// except where the engine hides a type behind another.
const EDIT_COLUMNS_SQL: &str = "
SELECT c.COLUMN_NAME AS column_name,
       {data_type} AS data_type,
       IF(c.EXTRA LIKE '%VIRTUAL GENERATED%' OR c.EXTRA LIKE '%STORED GENERATED%',
          'yes', 'no') AS is_generated,
       @@session.time_zone AS session_time_zone,
       @@system_time_zone AS system_time_zone
FROM information_schema.COLUMNS AS c
JOIN information_schema.TABLES AS t
  ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
WHERE c.TABLE_SCHEMA = {schema}
  AND c.TABLE_NAME = {relation}
  AND t.TABLE_TYPE = 'BASE TABLE'
ORDER BY c.ORDINAL_POSITION
";

/// [`EDIT_COLUMNS_SQL`] for `engine`.
///
/// MariaDB has no JSON type: `JSON` is a `LONGTEXT` with a `json_valid` check
/// of the column's own name, and the catalog's `DATA_TYPE` says `longtext`. The
/// check is what tells such a column from text, so it is reported as `json`.
fn edit_columns_sql(engine: Engine) -> String {
    let data_type = match engine {
        Engine::MariaDb => {
            "IF(c.DATA_TYPE = 'longtext' AND EXISTS (
                SELECT 1 FROM information_schema.CHECK_CONSTRAINTS AS k
                WHERE k.CONSTRAINT_SCHEMA = c.TABLE_SCHEMA
                  AND k.TABLE_NAME = c.TABLE_NAME
                  AND k.CONSTRAINT_NAME = c.COLUMN_NAME
                  AND k.CHECK_CLAUSE LIKE 'json\\_valid(%'
            ), 'json', c.DATA_TYPE)"
        }
        _ => "c.DATA_TYPE",
    };
    EDIT_COLUMNS_SQL.replace("{data_type}", data_type)
}

/// The key-part rows of [`STRUCTURE_INDEXES_SQL`] as one index per name. The
/// definition names the plain columns alone, as it always has.
fn assemble_indexes(result: &QueryResult) -> Result<Vec<Index>, DbError> {
    let mut indexes = Vec::new();
    // Grouped by `object_name`, the first column.
    for parts in result.rows.chunk_by(|a, b| a.get(0) == b.get(0)) {
        let name = required_cell(result, &parts[0], "object_name")?;
        let unique = required_cell(result, &parts[0], "non_unique")? == "0";
        let covered: Vec<&str> = parts
            .iter()
            .filter_map(|row| optional_cell(result, row, "column_name"))
            .collect();
        indexes.push(Index {
            name: name.to_string(),
            kind: match () {
                _ if name == "PRIMARY" => IndexKind::Primary,
                _ if unique => IndexKind::Unique,
                _ => IndexKind::Plain,
            },
            columns: parts
                .iter()
                .map(|row| required_cell(result, row, "key_part").map(str::to_string))
                .collect::<Result<_, _>>()?,
            predicate: None,
            definition: format!(
                "{} ({})",
                if unique { "UNIQUE INDEX" } else { "INDEX" },
                covered.join(", ")
            ),
        });
    }
    Ok(indexes)
}

/// A `mysql://` URL, read by dbdelve rather than by the driver.
///
/// The driver has its own URL parser with a fixed key list, and it would refuse
/// the whole string over an `sslmode` it does not know — naming neither the
/// option nor the reason, which is the same trap the Postgres side documents.
/// dbdelve owns both TLS keys and builds the driver's options from fields.
pub fn config_from_url(url: &str) -> Result<ServerConfig, String> {
    super::server_from_url(url, "MySQL")
}

/// The fields every connection needs, query or cancel alike. `dial` is an
/// address in place of the server's host: a tunnel's, or the one of the
/// host's own addresses a session reached when the first was not there.
fn base_options(server: &ServerConfig, dial: Option<SocketAddr>) -> OptsBuilder {
    let (host, port) = match dial {
        Some(dial) => (dial.ip().to_string(), dial.port()),
        None => (server.host.clone(), server.port.unwrap_or(DEFAULT_PORT)),
    };
    OptsBuilder::new()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        // A loopback address would otherwise have the driver ask the server
        // for its socket path and reconnect to that path here, on this
        // machine: past the tunnel, or past the TLS the profile asked for,
        // since a socket is never encrypted, to whatever server is listening
        // locally.
        .prefer_socket(dial.is_none())
        .db_name((!server.database.is_empty()).then(|| server.database.clone()))
        .user(Some(server.user.clone()))
        .tcp_connect_timeout(Some(Duration::from_secs(CONNECT_TIMEOUT_SECONDS)))
}

/// Open a connection to `dial`, or to the host by name where there is none,
/// falling back to plaintext exactly where `sslmode=prefer` allows it. With
/// `fail_over`, a name whose first address is not there is tried at the
/// others, and the address reached comes back with the connection, for
/// everything opened beside it to reach the same server. Shared by the query
/// connection and the cancel connection so the fallbacks cannot drift between
/// them.
fn open(
    server: &ServerConfig,
    engine: Engine,
    dial: Option<SocketAddr>,
    fail_over: bool,
    options: impl Fn(Option<SocketAddr>) -> OptsBuilder,
) -> Result<(Conn, Option<SocketAddr>), DbError> {
    let password = server.login_password(engine)?;
    // Offered only when there is one. An empty password is not the same as no
    // password, and IAM auth relies on the latter.
    let options = |dial| options(dial).pass((!password.is_empty()).then(|| password.to_string()));
    let opened = match handshake(options(dial), server) {
        // verify-full checks the certificate against the host dialled, so an
        // address in the name's place would check the certificate against
        // something the user never typed.
        Err(error)
            if fail_over
                && dial.is_none()
                && unusable_address(&error)
                && server.sslmode != SslMode::VerifyFull =>
        {
            on_any_address(resolved(server), error, |address| {
                handshake(options(Some(address)), server)
            })
        }
        opened => opened.map(|connection| (connection, dial)),
    };
    server.forgetting_on_failure(
        engine,
        opened.map_err(|error| connect_error(&error, server)),
    )
}

fn handshake(options: OptsBuilder, server: &ServerConfig) -> Result<Conn, ::mysql::Error> {
    match Conn::new(over(options.clone(), ssl_options(server))) {
        // `prefer` is the one rung where a weaker connection is reachable, and
        // reaching it is what the word means. The driver cannot say "encrypt if
        // you can" in one attempt, so this is two — which is what libpq's own
        // `prefer` amounts to. No other mode falls back, and not where nothing
        // answered at all: plaintext would find nobody there either.
        Err(encrypted) if server.sslmode == SslMode::Prefer && !unusable_address(&encrypted) => {
            Conn::new(over(options, None)).map_err(|plaintext| after_prefer(encrypted, plaintext))
        }
        connected => connected,
    }
}

/// Which of `prefer`'s two failures to show. A server did answer the encrypted
/// attempt, so a plaintext one that finds nobody must not read as nothing
/// there. Nor may one refused for wanting the password in clear text, unless
/// the server could not take TLS: the encrypted attempt got past that point,
/// and its error (a rejected token, say) is the one to act on.
fn after_prefer(encrypted: ::mysql::Error, plaintext: ::mysql::Error) -> ::mysql::Error {
    let tls_refused = matches!(
        encrypted,
        ::mysql::Error::DriverError(::mysql::DriverError::TlsNotSupported)
    );
    if unusable_address(&plaintext) || (wants_cleartext(&plaintext) && !tls_refused) {
        encrypted
    } else {
        plaintext
    }
}

/// Every address the server's host resolves to, in the resolver's order.
///
/// ponytail: the first is dialled again, though the name already failed
/// there: the driver does not say which address it tried, and under DNS
/// round-robin it need not be the first of a fresh lookup. One more refusal
/// is cheap; matching it up would need the driver's own lookup.
fn resolved(server: &ServerConfig) -> Vec<SocketAddr> {
    (server.host.as_str(), server.port.unwrap_or(DEFAULT_PORT))
        .to_socket_addrs()
        .map_or_else(|_| Vec::new(), Iterator::collect)
}

/// `attempt` at each of `addresses` in turn, after `first` came back from the
/// name, until one is there: the connection and the address it reached.
///
/// The driver dials only the first address a name resolves to: its connect
/// gives up at the first one that refuses rather than moving to the next. So
/// `localhost` resolving to `::1` first never reaches a server listening on
/// `127.0.0.1` alone, which every other client does. A name with one address
/// has nothing left to try, and the first error is the one the user can act
/// on, since it names what they typed.
fn on_any_address(
    addresses: Vec<SocketAddr>,
    first: ::mysql::Error,
    attempt: impl Fn(SocketAddr) -> Result<Conn, ::mysql::Error>,
) -> Result<(Conn, Option<SocketAddr>), ::mysql::Error> {
    if addresses.len() < 2 {
        return Err(first);
    }
    for address in addresses {
        match attempt(address) {
            Err(error) if unusable_address(&error) => {}
            reached => return reached.map(|connection| (connection, Some(address))),
        }
    }
    Err(first)
}

/// Nothing answered at the address dialled, where another may. A timeout is
/// left out: it costs `CONNECT_TIMEOUT_SECONDS` an address to find out.
fn unusable_address(error: &::mysql::Error) -> bool {
    use std::io::ErrorKind::{
        AddrNotAvailable, ConnectionRefused, HostUnreachable, NetworkUnreachable,
    };
    matches!(
        error,
        ::mysql::Error::DriverError(::mysql::DriverError::CouldNotConnect(Some((_, _, kind))))
            if matches!(kind, ConnectionRefused | HostUnreachable | NetworkUnreachable | AddrNotAvailable)
    )
}

/// `options` over `tls`, answering a server that asks for the password in
/// clear text (`mysql_clear_password`, which RDS IAM and Azure Entra ID use)
/// only when this attempt goes encrypted. The driver refuses the plugin by
/// default, as libmysqlclient does, and a plaintext socket must never carry a
/// password in the clear. With TLS asked for, the driver fails rather than
/// carry on unencrypted, and `prefer`'s plaintext retry is built without TLS,
/// so without the plugin. Over TLS it gives away nothing new: a
/// `caching_sha2_password` server already gets the password in clear text
/// over TLS whenever its cache misses.
fn over(options: OptsBuilder, tls: Option<SslOpts>) -> OptsBuilder {
    let cleartext = tls.is_some();
    options.ssl_opts(tls).enable_cleartext_plugin(cleartext)
}

/// Another session at the server `dial` names, as the one a profile keeps.
fn connect(
    server: &ServerConfig,
    engine: Engine,
    dial: Option<SocketAddr>,
) -> Result<Conn, DbError> {
    open(server, engine, dial, false, |dial| {
        session_options(server, engine, dial)
    })
    .map(|(connection, _)| connection)
}

fn session_options(server: &ServerConfig, engine: Engine, dial: Option<SocketAddr>) -> OptsBuilder {
    match server.statement_timeout {
        0 => base_options(server, dial),
        // Run once at connect as a session default, never spliced into the
        // user's own submission — see `ServerConfig::statement_timeout` for
        // what this does and does not bound. MySQL's `max_execution_time`
        // counts milliseconds and MariaDB's `max_statement_time` seconds, and a
        // server too old to know the variable fails the connect here rather
        // than the statement later.
        seconds => base_options(server, dial).init(vec![if engine == Engine::MariaDb {
            format!("SET SESSION max_statement_time = {seconds}")
        } else {
            format!(
                "SET SESSION max_execution_time = {}",
                u64::from(seconds) * 1_000
            )
        }]),
    }
}

/// Where a session's every socket goes: through its tunnel, which may have
/// closed, or else to the address it reached, or else to its host by name.
fn dialled(
    tunnel: Option<&Tunnel>,
    address: Option<SocketAddr>,
) -> Result<Option<SocketAddr>, DbError> {
    tunnel.map_or(Ok(address), |tunnel| tunnel.dial().map(Some))
}

/// The throwaway connection a cancel opens: the same fields, but bounded end
/// to end and with no init statement it has no use for.
fn cancel_options(server: &ServerConfig, dial: Option<SocketAddr>) -> OptsBuilder {
    let timeout = Some(Duration::from_secs(CANCEL_TIMEOUT_SECONDS));
    base_options(server, dial)
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
    /// MySQL or MariaDB, which differ in how a statement timeout is spelled.
    engine: Engine,
    /// What the cancel socket dials too, and held so ssh runs as long as any
    /// clone does.
    tunnel: Option<Arc<Tunnel>>,
    /// The address this session reached in its host's place, where the
    /// name's first address was not there. Every socket opened beside it
    /// dials the same one: a `KILL QUERY` names a session by a number only
    /// its own server knows, and another server would stop someone else's.
    address: Option<SocketAddr>,
    /// A second session, for stopping a statement at the row limit while
    /// this one is busy reading its rows. Opened the first time it is needed
    /// and kept, so a run does not pay for a connect.
    side: Arc<Mutex<Option<Conn>>>,
    /// A Cancel for this session's schema graph read ([`Self::schema_graph`]).
    graph: GraphCancel,
}

impl Connection {
    pub fn open(server: &ServerConfig, engine: Engine) -> Result<Self, DbError> {
        // Before the tunnel, which may have cost a hardware-key touch.
        if server.has_tunnel() && server.sslmode == SslMode::VerifyFull {
            return Err(unverifiable_through_a_tunnel(server, engine));
        }
        tunnelled(server, DEFAULT_PORT, |tunnel| {
            let dial = tunnel.as_deref().map(Tunnel::dial).transpose()?;
            let (connection, address) = open(server, engine, dial, true, |dial| {
                session_options(server, engine, dial)
            })?;
            Ok(Self {
                connection_id: connection.connection_id(),
                server: server.clone(),
                engine,
                connection: Arc::new(Mutex::new(connection)),
                tunnel,
                address,
                side: Arc::default(),
                graph: GraphCancel::default(),
            })
        })
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// A second session, for a statement to run on beside this one's; see
    /// [`super::Connection::query_alongside`]. It shares the side session.
    pub fn alongside(&self) -> Result<Self, DbError> {
        let connection = connect(&self.server, self.engine, self.dial()?)?;
        Ok(Self {
            connection_id: connection.connection_id(),
            connection: Arc::new(Mutex::new(connection)),
            graph: GraphCancel::default(),
            ..self.clone()
        })
    }

    /// A session on `database`, for the (connection, database) pair: a new
    /// connection from this one's live server config -- its resolved password
    /// and its tunnel -- with the database replaced, selected by the driver's
    /// own handshake and not a `USE` statement. It shares no mutable state
    /// with this connection: `alongside` shares the `side` session a cancel
    /// reads through, and this has one of its own. It carries no read-only
    /// hold of its own; the caller applies it.
    pub fn on_database(&self, database: &str) -> Result<Self, DbError> {
        let server = ServerConfig {
            database: database.to_string(),
            ..self.server.clone()
        };
        let connection = connect(&server, self.engine, self.dial()?)?;
        Ok(Self {
            connection_id: connection.connection_id(),
            server,
            connection: Arc::new(Mutex::new(connection)),
            side: Arc::default(),
            // Its own: a Cancel on this session would otherwise drop the next
            // statement of a diagram read on the first.
            graph: GraphCancel::default(),
            ..self.clone()
        })
    }

    /// A round trip, unlike Postgres's check: the driver keeps no closed flag.
    /// `COM_PING` is answered inside a failed transaction too, so a statement
    /// that merely errored is not mistaken for a dropped session.
    pub fn is_lost(&self) -> bool {
        self.connection
            .lock()
            .map_or(true, |mut connection| connection.ping().is_err())
    }

    fn dial(&self) -> Result<Option<SocketAddr>, DbError> {
        dialled(self.tunnel.as_deref(), self.address)
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
        self.graph.ask();
        let server = &self.server;
        let dial = self.dial()?;
        let (mut connection, _) = open(server, self.engine, dial, false, |dial| {
            cancel_options(server, dial)
        })?;
        connection
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .map_err(|error| DbError {
                message: format!("Could not ask the server to cancel: {error}"),
                position: None,
            })
    }

    /// Run one statement verbatim, keeping what `fetch` asks for of its rows;
    /// see [`super::Connection::query`].
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. The
    /// rows are read off the socket as they come, so one past the limit is
    /// dropped as soon as it is counted.
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, true, fetch, false)
    }

    /// [`Self::query`] for a tab bound to this session's database, which is
    /// the one it was opened on ([`Self::on_database`]): the session is put
    /// back on it, by the driver's own call and not a `USE`, before the
    /// statement and under the lock that runs it. A `USE` of the user's, in
    /// this tab or another bound to the same database, may have moved it
    /// since, and applying and running as two acquisitions would let a run
    /// land in the other's database.
    pub fn query_bound(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, true, fetch, true)
    }

    /// Applies `steps` as one verified sequence on this session, under one
    /// acquisition of its lock, so no other tab's statement runs between the
    /// re-reads and the writes: see [`super::run_verified`]. Every statement
    /// goes through [`Self::ran`], so none takes the lock again.
    ///
    /// An `UPDATE`'s count is the rows it matched ([`matched_count`]): the
    /// server's own count is the rows it changed, and a row already holding the
    /// value would count none. An `INSERT`'s and a `DELETE`'s is the count as
    /// reported.
    ///
    /// `bound` puts the session back on its own database first, under the same
    /// lock, as [`Self::query_bound`] does.
    pub fn apply_verified(
        &self,
        steps: &[VerifiedStep],
        proceed: &dyn Fn() -> bool,
        bound: bool,
    ) -> ApplyOutcome {
        let failed = |error| ApplyOutcome::Failed {
            stage: ApplyStage::Begin,
            error,
        };
        let mut connection = match self.connection.lock() {
            Ok(connection) => connection,
            Err(_) => {
                return failed(plain_error(
                    "The connection is unavailable after an earlier internal failure.".into(),
                ));
            }
        };
        // Registers the run, so a Cancel finds the session, and checks for one
        // that came first, as `run` does before it touches the session.
        if !proceed() {
            return failed(plain_error("Cancelled before it was sent.".into()));
        }
        if bound && let Err(error) = connection.select_db(&self.server.database) {
            return failed(super::unplaced(query_error(&error)));
        }
        // Asked of the held connection when a statement fails, and read by the
        // lost test, which cannot take the lock `send` holds: only an error at
        // `COMMIT` with the session gone, or the `COMMIT` interrupted, is an
        // outcome nobody can know.
        let gone = std::cell::Cell::new(false);
        let mut send = |sql: &str, sending: Sending| {
            if sending == Sending::Open {
                return open_verified(&mut connection, sql, steps, self.engine);
            }
            // A Cancel's `KILL QUERY` that lands on the `COMMIT` answers 1317
            // on a live session, while InnoDB may have committed already: as
            // unknown as a `COMMIT` whose session went.
            // A session whose `ROLLBACK` failed is gone, which ended the
            // transaction, or is reset, which ends it and the session's state.
            if sending == Sending::Abandon {
                if connection.ping().is_err() || connection.query_drop(sql).is_ok() {
                    return Ok(QueryResult::default());
                }
                return connection
                    .reset()
                    .map(|()| QueryResult::default())
                    .map_err(|error| query_error(&error));
            }
            if sending == Sending::Bracket && sql == "COMMIT" {
                return match connection.query_drop(sql) {
                    Ok(()) => Ok(QueryResult::default()),
                    Err(error) => {
                        let interrupted = interrupted(&error);
                        // A kill that came before the `COMMIT` ran leaves the
                        // apply's transaction open on the live session, its
                        // writes for the user's next `COMMIT` to land. Whether
                        // it landed stays unknown; what it left open does not
                        // stay.
                        if interrupted && connection.query_drop("ROLLBACK").is_err() {
                            let _ = connection.reset();
                        }
                        gone.set(interrupted || connection.ping().is_err());
                        Err(query_error(&error))
                    }
                };
            }
            let update = sending == Sending::Write(StepKind::Update);
            let result = self
                .ran(&mut connection, sql, false, Fetch::default(), update)
                .map(|(result, _)| result);
            if result.is_err() {
                gone.set(connection.ping().is_err());
            }
            result
        };
        super::run_verified(self.engine, steps, &mut send, proceed, &|_| false, &|_| {
            gone.get()
        })
    }

    /// dbdelve's own SQL. Its rows are never editable, so it does not pay for the
    /// round trip that reads a primary key.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, false, Fetch::default(), false)
    }

    /// `ask` on the side session, opened if there is none. A failure is taken
    /// for a session that has gone stale while it sat idle, and asked once
    /// more on a fresh one.
    fn aside<T>(&self, mut ask: impl FnMut(&mut Conn) -> Result<T, ::mysql::Error>) -> Option<T> {
        let mut side = self.side.lock().ok()?;
        for _ in 0..2 {
            if side.is_none() {
                let dial = self.dial().ok()?;
                *side = open(&self.server, self.engine, dial, false, |dial| {
                    cancel_options(&self.server, dial)
                })
                .map(|(connection, _)| connection)
                .ok();
            }
            match ask(side.as_mut()?) {
                Ok(answer) => return Some(answer),
                Err(_) => *side = None,
            }
        }
        None
    }

    /// Stop the statement in flight now that it has passed the row limit:
    /// `true` when a `KILL QUERY` went out.
    ///
    /// Safe wherever the statement only reads, unlike on Postgres: a killed
    /// statement is rolled back alone and the transaction around it stays
    /// open, so a `SELECT` inside the user's own `BEGIN` can be stopped too.
    /// Only while the session is still executing it: one that has already
    /// finished, its rows waiting in the socket, has nothing to stop, and a
    /// kill sent to an idle session is one this check exists not to send.
    fn stop_at_limit(&self) -> bool {
        let id = self.connection_id;
        self.aside(|side| {
            let running: Option<u32> = side.query_first(format!(
                "SELECT ID FROM information_schema.PROCESSLIST
                 WHERE ID = {id} AND COMMAND = 'Query'"
            ))?;
            if running.is_none() {
                return Ok(false);
            }
            side.query_drop(format!("KILL QUERY {id}"))?;
            Ok(true)
        }) == Some(true)
    }

    /// `bound` puts the session back on its own database first.
    fn run(
        &self,
        sql: &str,
        editable: bool,
        fetch: Fetch,
        bound: bool,
    ) -> Result<QueryResult, DbError> {
        let mut connection = self.connection.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        let held = fetch.hold()?;
        if bound {
            connection
                .select_db(&self.server.database)
                .map_err(|error| super::unplaced(query_error(&error)))?;
        }

        let (mut result, probed) = match self.ran(&mut connection, sql, editable, fetch, false) {
            Ok(ran) => ran,
            Err(error) => return Err(rolled_back(&mut connection, sql, error)),
        };

        // The connection mutex is not reentrant and `edit_target` runs a catalog
        // query through it, so the lock has to go before the question is asked.
        drop(held);
        drop(connection);
        if editable {
            (result.edit, result.read_only_reason) = self.edit_target(sql, &probed);
        }

        Ok(result)
    }

    /// The part of [`Self::run`] that works under the held connection, for the
    /// verified apply, which holds it across several statements and so cannot
    /// take its lock again. The lock, the binding and the question of which
    /// table the columns belong to stay with the callers.
    ///
    /// `update` reads the count as the rows the statement *matched*
    /// ([`matched_count`]) and not the ones it changed, for an `UPDATE` step.
    fn ran(
        &self,
        connection: &mut Conn,
        sql: &str,
        editable: bool,
        fetch: Fetch,
        update: bool,
    ) -> Result<(QueryResult, Vec<ProbedColumn>), DbError> {
        let mut result = QueryResult::default();
        let mut probed = Vec::new();
        // How long the kept rows took, once there were as many as the limit.
        let mut filled = None;
        let mut past_limit = false;
        // What the sets before the last one raised, which `SHOW WARNINGS` no
        // longer lists. A set that returned rows is not counted: the driver
        // has moved past its count by the time its rows are read.
        let mut warned = 0u16;
        let mut earlier = 0u32;

        let mut submit = || -> Result<(), DbError> {
            // Timed from here, not from the call: one connection serialises a
            // profile's queries, and time spent waiting behind the catalog load
            // is not time the server spent on this statement.
            let started = Instant::now();
            let mut builder = RowBuilder::default();
            let mut sets = connection
                .query_iter(sql)
                .map_err(|error| query_error(&error))?;

            while let Some(mut set) = sets.iter() {
                earlier += u32::from(warned);
                warned = 0;
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
                    // Read first: draining the set moves the driver past its
                    // OK packet, after which the count reads as zero and the
                    // info as empty.
                    let affected = set.affected_rows();
                    result.rows_affected = match update {
                        true => matched_count(affected, &set.info_str()),
                        false => Some(affected),
                    };
                    warned = set.warnings();
                    for row in &mut set {
                        row.map_err(|error| query_error(&error))?;
                    }
                    continue;
                }

                // One selection can carry several statements, and the grid shows
                // one result set, so each new description starts the kept set
                // over and the last statement wins. The types came with the
                // description, so the rows are shown by type from the first.
                if let Some(feed) = fetch.feed {
                    feed.begin(columns.clone());
                }
                let mut rows = Vec::new();
                let mut bytes = 0;
                // This set's rows, kept or not, and kept: a fed row is not in
                // `rows` to be counted.
                let (mut returned, mut kept) = (0, 0);
                let mut stopping = false;
                filled = None;
                for row in &mut set {
                    let row = match row {
                        Ok(row) => row,
                        Err(error) if stopping && interrupted(&error) => {
                            result.stopped = Some(Stopped::AtLimit);
                            break;
                        }
                        Err(error) => return Err(query_error(&error)),
                    };
                    returned += 1;
                    if fetch.limit.is_some_and(|limit| kept >= limit) {
                        if fetch.reads_only && !stopping && returned == kept + 1 {
                            stopping = self.stop_at_limit();
                        }
                        continue;
                    }
                    kept += 1;
                    if fetch.limit == Some(kept) {
                        filled = Some(started.elapsed());
                        if let Some(feed) = fetch.feed {
                            feed.fill();
                        }
                    }
                    for (index, column) in described.iter().enumerate() {
                        let value = row.as_ref(index).unwrap_or(&Value::NULL);
                        let cell = render(value, column.binary, &columns, index)?;
                        builder.push(cell.as_deref());
                    }
                    let cells = builder.finish()?;
                    bytes += cells.text_len();
                    match fetch.feed {
                        Some(feed) => feed.push(cells),
                        None => rows.push(cells),
                    }
                }

                // A query's count is the rows it returned, which is what
                // Postgres reports for a select too. `affected_rows` is zero for
                // one, and zero would read as a result set that came back empty.
                result.rows_affected = Some(returned as u64);
                // A stopped statement's count is only what had arrived.
                result.capped_from =
                    (result.stopped.is_none() && returned > kept).then_some(returned);
                past_limit = returned > kept || result.stopped.is_some();
                result.columns = columns;
                result.rows = rows;
                result.bytes = bytes;
                probed = described;
                if result.stopped.is_some() {
                    break;
                }
            }
            // Timed to the last kept row when rows past the limit came after
            // it: stopping or draining the statement is not the wait for these.
            result.elapsed = match filled {
                Some(filled) if past_limit => filled,
                _ => started.elapsed(),
            };
            drop(sets);
            if editable {
                if earlier > 0 {
                    result.messages.push(format!(
                        "An earlier statement raised {earlier} warning{}, which the server no longer lists.",
                        if earlier == 1 { "" } else { "s" }
                    ));
                }
                if connection.warnings() > 0 {
                    result.messages.extend(warnings(connection));
                }
            }
            Ok(())
        };

        submit()?;
        Ok((result, probed))
    }

    /// Which table these columns can be written back to, if any, and when the
    /// answer is no for a reason the user can act on, that reason.
    ///
    /// Every step is allowed to answer "no": a join, a computed column, a key
    /// the select omitted. A failure answers "no" too — this runs after the
    /// user's statement already succeeded, and a catalog query that was refused
    /// must not turn that success into an error.
    fn edit_target(
        &self,
        sql: &str,
        probed: &[ProbedColumn],
    ) -> (Option<EditTarget>, Option<String>) {
        let Some((schema, table)) = sole_table(probed) else {
            return (None, None);
        };
        // An apply is verified inside a transaction, and its `ROLLBACK` is what
        // undoes a write that drifted, counted wrong or failed part way. On an
        // engine with none, that write stays and a kept draft sends it again.
        //
        // ponytail: one catalog round trip per result set, no cache. A map from
        // table to key held on the connection is the upgrade path if the trip
        // shows up in query timings.
        let Ok(found) = self.internal_query(&structure_sql(PRIMARY_KEY_SQL, &schema, &table))
        else {
            return (None, None);
        };
        if let Some(reason) = untransactional_reason(&found) {
            return (None, Some(reason));
        }
        let key = primary_key_columns(&found);
        if !key.is_empty() {
            return (resolve_edit_target(probed, &schema, &table, &key), None);
        }
        // No primary key: the table's rows are named by every column, which
        // takes the columns' types.
        // Provenance names a column's table, never which instance of it: the
        // two sides of a self-join each read `t`, and every-column matching
        // would then name a row that is neither. Only a plain read of one
        // table is edited by its values.
        if let Err(why) = crate::sql::table_read_or_why(self.engine, sql) {
            return (None, Some(why.reason().into()));
        }
        let Ok(catalog) = self.internal_query(&structure_sql(
            &edit_columns_sql(self.engine),
            &schema,
            &table,
        )) else {
            return (None, None);
        };
        match table_columns(&catalog) {
            Some(columns) => resolve_keyless_target(probed, &schema, &table, &columns),
            None => (None, None),
        }
    }

    pub fn databases(&self) -> Result<super::Databases, DbError> {
        assemble_databases(&self.internal_query(DATABASES_SQL)?)
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let relations = self.internal_query(
            &RELATIONS_SQL
                .replace("{system}", SYSTEM_SCHEMAS)
                .replace("{tables}", TABLES)
                .replace("{views}", VIEWS),
        )?;
        let mut catalog = assemble_catalog(relations, QueryResult::default())?;
        let schemas = self.internal_query(&SCHEMAS_SQL.replace("{system}", SYSTEM_SCHEMAS))?;
        for name in column_of(&schemas, "schema_name")? {
            if !catalog.schemas.iter().any(|schema| schema.name == name) {
                catalog.schemas.push(Schema {
                    name,
                    relations: Vec::new(),
                    routines: Vec::new(),
                    sequences: Vec::new(),
                });
            }
        }
        Ok(catalog)
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        let routines = self.internal_query(&ROUTINES_SQL.replace("{system}", SYSTEM_SCHEMAS))?;
        assemble_catalog(QueryResult::default(), routines)
    }

    /// MariaDB's only: MySQL has no sequences, and a MariaDB before 10.3 answers
    /// the filter with no rows, so there is no version probe.
    pub fn sequences(&self) -> Result<Catalog, DbError> {
        if self.engine != Engine::MariaDb {
            return Ok(Catalog::default());
        }
        let sequences = self.internal_query(&SEQUENCES_SQL.replace("{system}", SYSTEM_SCHEMAS))?;
        assemble_sequences(sequences)
    }

    /// MariaDB's `SHOW CREATE SEQUENCE`, whose second column is the statement,
    /// as [`Self::ddl`] reads `SHOW CREATE TABLE`'s. MySQL has no sequences.
    pub fn sequence_ddl(&self, schema: &str, name: &str) -> Result<String, DbError> {
        if self.engine != Engine::MariaDb {
            return Err(plain_error("MySQL has no sequences.".into()));
        }
        self.show_create("SEQUENCE", schema, name)
    }

    /// On a connection of its own, as [`Self::cancel`] is, so opening every
    /// table for its statistics never holds the mutex a user's query waits on.
    /// The side connection is dropped on return.
    pub fn sizes(&self) -> Result<Sizes, DbError> {
        let connection = connect(&self.server, self.engine, self.dial()?)?;
        let side = Self {
            connection_id: connection.connection_id(),
            server: self.server.clone(),
            engine: self.engine,
            connection: Arc::new(Mutex::new(connection)),
            tunnel: self.tunnel.clone(),
            address: self.address,
            side: Arc::default(),
            graph: GraphCancel::default(),
        };
        assemble_sizes(
            side.internal_query(
                &SIZES_SQL
                    .replace("{system}", SYSTEM_SCHEMAS)
                    .replace("{stored}", STORED_TABLES),
            )?,
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

        let mut structure = assemble_structure(columns, QueryResult::default(), constraints)?;
        structure.indexes = assemble_indexes(&indexes)?;
        structure.foreign_keys = assemble_foreign_keys(&keys)?;
        Ok(structure)
    }

    /// A Cancel stops it before any of its statements as well as during one:
    /// a `KILL QUERY` that reaches the session while it is idle between them
    /// stops nothing, and would let the rest run.
    pub fn schema_graph(&self, schema: &str) -> Result<SchemaGraph, DbError> {
        self.graph.read(|held| {
            let query = |template| {
                let held = Fetch {
                    held: Some(held),
                    ..Fetch::default()
                };
                self.run(&graph_sql(template, schema), false, held, false)
            };
            let columns = query(GRAPH_COLUMNS_SQL)?;
            let unique_keys = query(GRAPH_UNIQUE_KEYS_SQL)?;
            let foreign_keys = query(GRAPH_FOREIGN_KEYS_SQL)?;
            assemble_schema_graph(schema, &columns, &unique_keys, &foreign_keys)
        })
    }

    /// `SHOW CREATE TABLE` answers for a view too, with its `CREATE VIEW` in
    /// the same second column.
    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        self.show_create("TABLE", schema, relation)
    }

    /// `SHOW CREATE {what}` for `schema.name`, the statement being the second
    /// column.
    fn show_create(&self, what: &str, schema: &str, name: &str) -> Result<String, DbError> {
        let shown = self.internal_query(&format!(
            "SHOW CREATE {what} {}",
            self.engine.qualified(schema, name)
        ))?;
        shown
            .rows
            .first()
            .and_then(|row| row.get(1))
            .map(|definition| format!("{definition};"))
            .ok_or_else(|| plain_error(format!("{schema}.{name} has no definition to show.")))
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
fn render<'a>(
    value: &'a Value,
    binary: bool,
    columns: &[Column],
    index: usize,
) -> Result<Option<Cow<'a, str>>, DbError> {
    Ok(match value {
        Value::NULL => None,
        // MySQL's own literal syntax for a blob, rather than bare hex, so a
        // value copied out of the grid is a value that can be pasted into a
        // statement.
        Value::Bytes(bytes) if binary => Some(format!("0x{}", hex::encode_upper(bytes)).into()),
        Value::Bytes(bytes) => Some(
            // A column whose declared character set does not match what was
            // stored can return bytes that are not text at all. Postgres raises
            // the same error for the same reason, naming the column.
            std::str::from_utf8(bytes)
                .map_err(|_| non_utf8_error(columns, index))?
                .into(),
        ),
        // Unreachable through the text protocol, which sends everything as
        // bytes. `Value` is shared with the binary protocol, and a driver that
        // surprised us here should still render something true.
        other => Some(other.as_sql(true).trim_matches('\'').to_string().into()),
    })
}

/// `bit`, `vector` and the spatial types come back as bytes too, without being
/// the blobs an edit refuses, and a literal that quoted their hex would store
/// its text. An empty `0x` is no literal here: it reads as a column of that name.
pub(super) fn fetched_literal(value: &str, data_type: &str) -> Option<String> {
    let bytes = Engine::MySql.is_binary_type(data_type)
        || matches!(data_type, "bit" | "geometry" | "vector");
    (bytes && value.starts_with("0x")).then(|| match value {
        "0x" => "x''".to_string(),
        hex => hex.to_string(),
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
        ..Default::default()
    })
}

fn graph_sql(template: &str, schema: &str) -> String {
    // The quoted name goes in last: a database may be called `{stored}`.
    template
        .replace("{stored}", STORED_TABLES)
        .replace("{schema}", &Engine::MySql.quote_literal(schema))
}

/// One column of a table as [`EDIT_COLUMNS_SQL`] describes it.
struct TableColumn {
    name: String,
    /// The catalog's type name, which [`is_matchable`] is asked about.
    data_type: String,
    /// Computed by the server: matched, never set.
    generated: bool,
    /// A `timestamp` read under a session time zone that is not a fixed
    /// offset: in the hour a daylight-saving change repeats, two instants
    /// render as the same local text, whose literal names only one of them.
    zoned: bool,
}

/// Whether a session renders a `timestamp` under a zone with no daylight
/// saving: UTC or a fixed offset, or `SYSTEM`, the default on both servers,
/// where the server's own zone is UTC. Any other system zone is refused: its
/// name is the abbreviation in force at startup, and `GMT` is London's winter.
fn fixed_time_zone(session: &str, system: &str) -> bool {
    let offset = |zone: &str| {
        let bytes = zone.as_bytes();
        bytes.len() == 6
            && matches!(bytes[0], b'+' | b'-')
            && bytes[1..3].iter().all(u8::is_ascii_digit)
            && bytes[3] == b':'
            && bytes[4..].iter().all(u8::is_ascii_digit)
    };
    match session {
        "SYSTEM" => system == "UTC",
        zone => zone == "UTC" || offset(zone),
    }
}

/// The table's columns in order, or `None` where the catalog said none (a view,
/// or a table that is not there) or an answer was malformed.
fn table_columns(result: &QueryResult) -> Option<Vec<TableColumn>> {
    let columns = result
        .rows
        .iter()
        .map(|row| {
            let data_type = required_cell(result, row, "data_type").ok()?.to_string();
            Some(TableColumn {
                name: required_cell(result, row, "column_name").ok()?.to_string(),
                zoned: data_type == "timestamp"
                    && !fixed_time_zone(
                        required_cell(result, row, "session_time_zone").ok()?,
                        required_cell(result, row, "system_time_zone").ok()?,
                    ),
                data_type,
                generated: required_cell(result, row, "is_generated").ok()? == "yes",
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!columns.is_empty()).then_some(columns)
}

/// Whether `=` against a column's value rendered as text finds it, for the
/// type [`EDIT_COLUMNS_SQL`] names. Fail closed: only types the compose servers
/// were seen to match, MySQL's and MariaDB's alike, are listed.
///
/// ponytail: a type is added when
/// `tests::live_every_listed_type_matches_its_row_by_its_rendered_text_on_mysql`
/// and `_on_mariadb` both exercise it. Left off: `float` and `json`, whose `=`
/// against their own text finds nothing (a `FLOAT` is held as a single and
/// compared as a double; a `JSON` value is never equal to a string), so a
/// matched row would read as gone; the byte types (`binary`, `varbinary`, the
/// blobs, `bit`), which the grid renders as `0x…` and the writer quotes as that
/// text; and `geometry` and `vector`, never exercised.
fn is_matchable(data_type: &str) -> bool {
    matches!(
        data_type,
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "bigint"
            | "decimal"
            | "double"
            | "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "date"
            | "time"
            | "datetime"
            | "timestamp"
            | "year"
            | "enum"
            | "set"
    )
}

/// The target of a result set over a table with no primary key: every column
/// of the table names the row, a NULL as `IS NULL`, the computed columns
/// matched but never set. Or the reason it cannot be, when the user can do
/// something about it.
///
/// Never a unique index, and never some of the columns: a column left out of
/// the `WHERE` matches rows the user is not looking at, so a column that cannot
/// be matched by its value, or that the result omits, makes the whole result
/// read-only.
fn resolve_keyless_target(
    probed: &[ProbedColumn],
    schema: &str,
    table: &str,
    columns: &[TableColumn],
) -> (Option<EditTarget>, Option<String>) {
    let column_of = |column: &ProbedColumn| column.table.as_ref().and(column.column.clone());
    // As for a key: two result columns reading one table column are the two
    // sides of a self-join, and a column computed beside the real ones is no
    // table's row.
    let mut origins = HashSet::new();
    if !probed
        .iter()
        .map(column_of)
        .all(|column| column.is_some_and(|column| origins.insert(column)))
    {
        return (None, None);
    }
    let refusal = |what: String| {
        Some(format!(
            "A table without a primary key is edited by matching every column, and {what}."
        ))
    };
    if let Some(column) = columns.iter().find(|column| column.zoned) {
        return (
            None,
            refusal(format!(
                "{} (timestamp) cannot be matched by its value under a session time zone \
                 that is not a fixed offset",
                column.name
            )),
        );
    }
    if let Some(column) = columns
        .iter()
        .find(|column| !is_matchable(&column.data_type))
    {
        return (
            None,
            refusal(format!(
                "{} ({}) cannot be matched by its value",
                column.name, column.data_type
            )),
        );
    }
    let mut keys = Vec::new();
    let mut fixed = Vec::new();
    for column in columns {
        let Some(at) = probed
            .iter()
            .position(|probed| column_of(probed).as_deref() == Some(column.name.as_str()))
        else {
            return (
                None,
                refusal(format!("{} is not in the result", column.name)),
            );
        };
        keys.push(at);
        if column.generated {
            fixed.push(at);
        }
    }
    let target = EditTarget {
        schema: schema.to_string(),
        table: table.to_string(),
        columns: probed.iter().map(column_of).collect(),
        keys,
        row_match: RowMatch::EveryColumn { fixed },
        ..Default::default()
    };
    (Some(target), None)
}

fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    super::structure_sql(Engine::MySql, template, schema, relation)
}

/// The columns of the primary key [`PRIMARY_KEY_SQL`] found, in key order:
/// none for a table without one, whose one row has a NULL column.
fn primary_key_columns(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| optional_cell(result, row, "column_name"))
        .map(str::to_string)
        .collect()
}

/// Why a table [`PRIMARY_KEY_SQL`] found is not edited, `None` when it
/// found a transactional one.
fn untransactional_reason(result: &QueryResult) -> Option<String> {
    let engine = result
        .rows
        .iter()
        .find_map(|row| optional_cell(result, row, "untransactional_engine"))?;
    Some(untransactional(engine))
}

/// Why a table on `engine`, a storage engine with no transactions, is not
/// edited.
fn untransactional(engine: &str) -> String {
    format!(
        "The table's storage engine, {engine}, has no transactions, so a failed edit \
         could not be rolled back; dbdelve will not edit it."
    )
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
/// hard rule 6 is that nothing below quietly accepts less than it was asked for:
/// each rung either gets its checks or fails saying so.
fn ssl_options(server: &ServerConfig) -> Option<SslOpts> {
    let with_root = || {
        SslOpts::default().with_root_cert_path(
            server
                .root_certificate
                .as_deref()
                .map(crate::store::home_expanded),
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

/// The driver checks the certificate's name against the address it dials and
/// offers no way to name another, so through a tunnel verify-full could only
/// ever check the name against the loopback address (AGENTS.md, hard rule 6).
fn unverifiable_through_a_tunnel(server: &ServerConfig, engine: Engine) -> DbError {
    plain_error(format!(
        "sslmode=verify-full cannot be honoured through an {} tunnel on {}: \
         the driver checks the certificate against the address it dials, which is \
         the tunnel's loopback address rather than {}.",
        if server.ssm.is_some() { "SSM" } else { "SSH" },
        engine.label(),
        server.host
    ))
}

fn connect_error(error: &::mysql::Error, server: &ServerConfig) -> DbError {
    // A refused connection is the most common failure by a wide margin, and the
    // driver's own wording buries the endpoint. Say what happened, and nothing
    // about what the user should do -- we cannot see their machine.
    let refused = match error {
        ::mysql::Error::IoError(io) => io.kind() == std::io::ErrorKind::ConnectionRefused,
        // How the driver reports a TCP connect it could not make.
        ::mysql::Error::DriverError(::mysql::DriverError::CouldNotConnect(Some((_, _, kind)))) => {
            *kind == std::io::ErrorKind::ConnectionRefused
        }
        _ => false,
    };
    if refused {
        return plain_error(format!(
            "Connection refused: nothing is listening on {}",
            server.endpoint()
        ));
    }
    if wants_cleartext(error) {
        return plain_error(
            "The server asked for the password in clear text (mysql_clear_password), \
             which DBDelve sends only over TLS. That needs a server that supports TLS, \
             with Encryption set to Require or stricter."
                .into(),
        );
    }

    plain_error(describe(error))
}

/// The driver refusing `mysql_clear_password`, which [`over`] allows on every
/// encrypted attempt, so this is a plaintext one: `disable`, or `prefer`'s retry
/// after a server that could not take TLS ([`after_prefer`]).
fn wants_cleartext(error: &::mysql::Error) -> bool {
    use ::mysql::DriverError::{CleartextPluginDisabled, UnknownAuthPlugin};
    matches!(error, ::mysql::Error::DriverError(CleartextPluginDisabled))
        || matches!(
            error,
            ::mysql::Error::DriverError(UnknownAuthPlugin(plugin)) if plugin == "mysql_clear_password"
        )
}

/// The opening of a verified apply, `opening`, sent only when the session is
/// in no transaction. A `BEGIN` inside one commits it, the user's writes with
/// it, so the server is asked first: `@@autocommit` off is a session whose
/// every statement opens one, and `SET TRANSACTION` is refused with 1568
/// while one is open, a `BEGIN`'s even before any statement ran in it.
///
/// The `SET` applies to the next transaction alone, the apply's. Under
/// `SERIALIZABLE` InnoDB reads each re-read row with a shared lock held to the
/// `COMMIT`, so no other session changes a row between its re-read and its
/// write: a writer waits, or one side fails with a deadlock and rolls back.
fn open_verified(
    connection: &mut Conn,
    opening: &str,
    steps: &[VerifiedStep],
    engine: Engine,
) -> Result<QueryResult, DbError> {
    let autocommit: Option<i64> = connection
        .query_first("SELECT @@autocommit")
        .map_err(|error| query_error(&error))?;
    if autocommit == Some(0) {
        return Err(super::in_a_transaction());
    }
    let mut tables: Vec<Vec<String>> = Vec::new();
    for step in steps {
        let table = crate::sql::written_table(engine, &step.sql).ok_or_else(|| {
            plain_error("The table an edit writes could not be read off its statement.".into())
        })?;
        if !tables.contains(&table) {
            tables.push(table);
        }
    }
    match connection.query_drop("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE") {
        Err(::mysql::Error::MySqlError(error)) if error.code == 1568 => {
            return Err(super::in_a_transaction());
        }
        Err(error) => return Err(query_error(&error)),
        Ok(()) => {}
    }
    connection
        .query_drop(opening)
        .map_err(|error| query_error(&error))?;
    // The transaction is the apply's own now, so a refusal ends it.
    if let Err(error) = transactional_targets(connection, &tables, engine) {
        if connection.query_drop("ROLLBACK").is_err() {
            let _ = connection.reset();
        }
        return Err(error);
    }
    Ok(QueryResult::default())
}

/// A table's storage engine was asked when its grid loaded; it is asked again
/// inside the apply's transaction, since an `ALTER TABLE … ENGINE` since then
/// would leave the `ROLLBACK` undoing nothing. Each table is read first, which
/// takes the metadata lock the transaction then holds to its end: an `ALTER`
/// from another session waits for it, so the engine checked is the engine the
/// writes land in.
fn transactional_targets(
    connection: &mut Conn,
    tables: &[Vec<String>],
    engine: Engine,
) -> Result<(), DbError> {
    for table in tables {
        let (schema, relation) = match table.as_slice() {
            [schema, relation] => (schema.clone(), relation.clone()),
            [relation] => (
                connection
                    .query_first::<String, _>("SELECT DATABASE()")
                    .map_err(|error| query_error(&error))?
                    .unwrap_or_default(),
                relation.clone(),
            ),
            _ => {
                return Err(plain_error(
                    "The table an edit writes could not be read off its statement.".into(),
                ));
            }
        };
        connection
            .query_drop(metadata_lock_sql(engine, &schema, &relation))
            .map_err(|error| query_error(&error))?;
        let engines: Vec<String> = connection
            .query(structure_sql(UNTRANSACTIONAL_SQL, &schema, &relation))
            .map_err(|error| query_error(&error))?;
        if let Some(storage) = engines.first() {
            return Err(plain_error(untransactional(storage)));
        }
    }
    Ok(())
}

/// A read of no rows of `schema.relation`, sent only to open the table.
fn metadata_lock_sql(engine: Engine, schema: &str, relation: &str) -> String {
    format!(
        "SELECT 1 FROM {}.{} LIMIT 0",
        engine.quote_identifier(schema),
        engine.quote_identifier(relation)
    )
}

/// `KILL QUERY`'s doing: "Query execution was interrupted", on both servers.
fn interrupted(error: &::mysql::Error) -> bool {
    matches!(error, ::mysql::Error::MySqlError(error) if error.code == 1317)
}

fn query_error(error: &::mysql::Error) -> DbError {
    DbError {
        message: describe(error),
        // MySQL reports no offset into the statement, so there is nothing to
        // point the editor at. Absent rather than guessed.
        position: None,
    }
}

/// A failure lists nothing: the statement already succeeded, and a refused
/// `SHOW WARNINGS` must not turn that into an error.
///
/// `SHOW WARNINGS` leaves `ROW_COUNT()` at -1, so a later statement that reads
/// it sees that rather than the count of the statement that warned. The mysql
/// client's `\W` costs the same, and the warnings are worth it.
fn warnings(connection: &mut Conn) -> Vec<String> {
    connection
        .query::<(String, u32, String), _>("SHOW WARNINGS")
        .unwrap_or_default()
        .into_iter()
        .map(|(level, code, message)| format!("{level} ({code}): {message}"))
        .collect()
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

/// The rows an `UPDATE` matched, which is not what `affected_rows` says:
/// that counts the rows that *changed*, so a statement that writes the value a
/// row already holds counts none, and a count of none cannot be told from a
/// statement that matched nothing. The OK packet's info text carries the
/// matched count, as three integers (matched, changed, warnings) in the
/// server's language, which `lc_messages` sets: Russian and Japanese keep the
/// words different and the order the same. It is read by position, never by
/// label.
///
/// The changed number must be what the server affected. A locale that
/// reorders the numbers then fails that check and refuses the count, rather
/// than being believed. Text that is not three integers, or one no `u64`
/// holds, is not read, and the affected count stands only when it names a row.
fn matched_count(affected: u64, info: &str) -> Option<u64> {
    let numbers: Option<Vec<u64>> = info
        .split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(|run| run.parse().ok())
        .collect();
    match numbers.as_deref() {
        Some(&[matched, changed, _warnings]) => (changed == affected).then_some(matched),
        _ => (affected > 0).then_some(affected),
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
    use crate::db::{
        ColumnDefinition, FetchedCell, ForeignKey, IndexKind, RelationKind, RoutineKind,
        VerifiedStep,
    };

    /// The server the `live_` tests talk to, from `dbdelve_MYSQL_URL` or
    /// `dbdelve_MARIADB_URL`.
    fn live_config(engine: Engine) -> ServerConfig {
        let variable = match engine {
            Engine::MariaDb => "dbdelve_MARIADB_URL",
            _ => "dbdelve_MYSQL_URL",
        };
        let url = std::env::var(variable).unwrap_or_else(|_| panic!("{variable} is required"));
        config_from_url(&url).unwrap_or_else(|_| panic!("{variable} should parse"))
    }

    fn live(engine: Engine) -> Connection {
        Connection::open(
            &ServerConfig {
                // The compose database speaks no TLS, and these tests are the one
                // place a plaintext connection is the point.
                sslmode: SslMode::Disable,
                ..live_config(engine)
            },
            engine,
        )
        .expect("connection should open")
    }

    #[test]
    fn a_mysql_count_is_the_matched_number_read_by_position_and_refuses_when_changed_disagrees() {
        // Matched, changed and warnings, in that order, whatever the language.
        let english = "Rows matched: 1  Changed: 0  Warnings: 0";
        let russian = "Совпало записей: 1  Изменено: 0  Предупреждений: 0";
        let japanese = "該当した行: 1  変更: 0  警告: 0";
        for info in [english, russian, japanese] {
            assert_eq!(matched_count(0, info), Some(1), "{info}");
        }
        // A changed row is a matched one, and the changed count must be what
        // the server said it affected.
        let changed = "Rows matched: 1  Changed: 1  Warnings: 0";
        assert_eq!(matched_count(1, changed), Some(1));
        assert_eq!(matched_count(0, changed), None);
        // Two rows matched is two, never read as one, whether or not the
        // second differs.
        let two = "Rows matched: 2  Changed: 1  Warnings: 0";
        assert_eq!(matched_count(1, two), Some(2));
        assert_eq!(matched_count(2, two), None);
        assert_eq!(
            matched_count(0, "Rows matched: 0  Changed: 0  Warnings: 0"),
            Some(0)
        );
        // No text to read: the affected count stands when it names a row, and
        // none at all is a count that cannot be told from a miss.
        assert_eq!(matched_count(0, ""), None);
        assert_eq!(matched_count(1, ""), Some(1));
        // Not three numbers: not the text this reads.
        assert_eq!(matched_count(1, "Records: 1  Duplicates: 0"), Some(1));
        assert_eq!(matched_count(0, "Records: 1  Duplicates: 0"), None);
        // A run of digits no `u64` holds is not a count.
        let long = "Rows matched: 99999999999999999999999  Changed: 1  Warnings: 0";
        assert_eq!(matched_count(1, long), Some(1));
        assert_eq!(matched_count(0, long), None);
    }
    /// The compose database as the bastion sees it, by service name and the
    /// port inside the network.
    fn live_tunnelled() -> ServerConfig {
        let url = std::env::var("dbdelve_MYSQL_URL").expect("dbdelve_MYSQL_URL is required");
        ServerConfig {
            host: "mysql".into(),
            port: Some(3306),
            sslmode: SslMode::Disable,
            ssm: None,
            ssh: crate::db::ssh::live_bastion("dbdelve-bastion"),
            ..config_from_url(&url).expect("dbdelve_MYSQL_URL should parse")
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
                password_command: None,
                sslmode: SslMode::default(),
                root_certificate: None,
                statement_timeout: 0,
                ssm: None,
                ssh: None,
                schema: None,
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
    fn a_url_without_a_database_leaves_it_blank() {
        let config = config_from_url("mysql://someone@db.example.test/").unwrap();
        assert_eq!(config.database, "");
    }

    #[test]
    fn every_rung_gets_the_checks_it_asked_for() {
        // Hard rule 6 in code. `disable` builds nothing, the two middle rungs
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
    fn only_an_encrypted_attempt_may_answer_in_cleartext() {
        let cleartext = |sslmode| {
            let tls = ssl_options(&ServerConfig {
                sslmode,
                ..ServerConfig::default()
            });
            let opened = |tls| {
                ::mysql::Opts::from(over(OptsBuilder::new(), tls)).get_enable_cleartext_plugin()
            };
            // The second is `prefer`'s plaintext retry.
            (opened(tls), opened(None))
        };
        assert_eq!(cleartext(SslMode::Disable), (false, false));
        for mode in [
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(cleartext(mode), (true, false), "{mode:?}");
        }
    }

    #[test]
    fn a_refused_cleartext_password_says_to_require_encryption() {
        use ::mysql::DriverError::{CleartextPluginDisabled, UnknownAuthPlugin};
        let server = ServerConfig::default();
        for refused in [
            UnknownAuthPlugin("mysql_clear_password".into()),
            CleartextPluginDisabled,
        ] {
            let message = connect_error(&::mysql::Error::DriverError(refused), &server).message;
            assert!(message.contains("Encryption set to Require"), "{message}");
        }
        let other = UnknownAuthPlugin("auth_gssapi_client".into());
        assert!(
            !connect_error(&::mysql::Error::DriverError(other), &server)
                .message
                .contains("Encryption")
        );
    }

    #[test]
    fn prefer_shows_the_encrypted_error_unless_the_server_refused_tls() {
        use ::mysql::DriverError::{TlsNotSupported, UnknownAuthPlugin};
        let cleartext =
            || ::mysql::Error::DriverError(UnknownAuthPlugin("mysql_clear_password".into()));
        let denied = || {
            ::mysql::Error::MySqlError(::mysql::MySqlError {
                state: "28000".into(),
                message: "Access denied".into(),
                code: 1045,
            })
        };
        let refused = || {
            ::mysql::Error::DriverError(::mysql::DriverError::CouldNotConnect(Some((
                "127.0.0.1".into(),
                "refused".into(),
                std::io::ErrorKind::ConnectionRefused,
            ))))
        };

        assert!(matches!(
            after_prefer(denied(), cleartext()),
            ::mysql::Error::MySqlError(ref error) if error.code == 1045
        ));
        assert!(wants_cleartext(&after_prefer(
            ::mysql::Error::DriverError(TlsNotSupported),
            cleartext(),
        )));
        assert!(matches!(
            after_prefer(denied(), refused()),
            ::mysql::Error::MySqlError(_)
        ));
        assert!(matches!(
            after_prefer(::mysql::Error::DriverError(TlsNotSupported), denied()),
            ::mysql::Error::MySqlError(_)
        ));
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

    fn keyless(columns: &[(&str, &str, bool)]) -> Vec<TableColumn> {
        columns
            .iter()
            .map(|(name, data_type, generated)| TableColumn {
                name: (*name).to_string(),
                data_type: (*data_type).to_string(),
                generated: *generated,
                zoned: false,
            })
            .collect()
    }

    /// A `timestamp` renders in the session's time zone, so under one with
    /// daylight saving the hour that repeats names two instants by one text:
    /// such a table is read-only unless the zone is a fixed offset.
    #[test]
    fn a_timestamp_under_a_zone_with_daylight_saving_is_not_matched_by_its_value() {
        let catalog = |session: &str| QueryResult {
            columns: [
                "column_name",
                "data_type",
                "is_generated",
                "session_time_zone",
                "system_time_zone",
            ]
            .map(|name| Column {
                name: name.into(),
                data_type: None,
            })
            .into(),
            rows: vec![
                crate::db::Row::new([
                    Some("bird"),
                    Some("varchar"),
                    Some("no"),
                    Some(session),
                    Some("CET"),
                ])
                .unwrap(),
                crate::db::Row::new([
                    Some("at"),
                    Some("timestamp"),
                    Some("no"),
                    Some(session),
                    Some("CET"),
                ])
                .unwrap(),
            ],
            ..QueryResult::default()
        };
        let resolve = |session: &str| {
            let columns = table_columns(&catalog(session)).expect("two columns");
            let probed = probed(&[
                (Some("sightings"), Some("bird")),
                (Some("sightings"), Some("at")),
            ]);
            resolve_keyless_target(&probed, "dbdelve_dev", "sightings", &columns)
        };
        let (target, reason) = resolve("SYSTEM");
        assert!(target.is_none());
        assert_eq!(
            reason.as_deref(),
            Some(
                "A table without a primary key is edited by matching every column, and at \
                 (timestamp) cannot be matched by its value under a session time zone that \
                 is not a fixed offset."
            )
        );
        let (target, reason) = resolve("+05:30");
        assert!(target.is_some(), "{reason:?}");
    }

    /// `SYSTEM` is the default zone on both servers, and a server running on
    /// UTC renders every instant by one text.
    #[test]
    fn a_system_session_zone_is_fixed_only_where_the_server_runs_on_utc() {
        assert!(fixed_time_zone("SYSTEM", "UTC"));
        assert!(!fixed_time_zone("SYSTEM", "GMT"));
        assert!(!fixed_time_zone("SYSTEM", "CET"));
        assert!(fixed_time_zone("UTC", "CET"));
        assert!(fixed_time_zone("+00:00", "CET"));
        assert!(fixed_time_zone("-03:30", "CET"));
        assert!(!fixed_time_zone("Europe/Paris", "UTC"));
        assert!(!fixed_time_zone("+5:30", "UTC"));
    }

    /// A result over `sightings` carrying these columns, in this order.
    fn sightings(columns: &[&str]) -> Vec<ProbedColumn> {
        probed(
            &columns
                .iter()
                .map(|column| (Some("sightings"), Some(*column)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_table_without_a_primary_key_is_matched_by_every_column_or_read_only_with_a_reason() {
        use crate::db::RowMatch;
        let table = keyless(&[
            ("bird", "varchar", false),
            ("place", "text", false),
            ("seen", "int", false),
            ("weight", "double", false),
            ("at", "datetime", false),
        ]);
        let whole = sightings(&["bird", "place", "seen", "weight", "at"]);
        let (target, reason) = resolve_keyless_target(&whole, "dbdelve_dev", "sightings", &table);
        let target = target.expect("every column is of a listed type");
        assert_eq!(target.schema, "dbdelve_dev");
        assert_eq!(target.table, "sightings");
        assert_eq!(target.keys, [0, 1, 2, 3, 4]);
        assert_eq!(
            target.columns,
            ["bird", "place", "seen", "weight", "at"].map(|name| Some(name.to_string()))
        );
        assert_eq!(target.row_match, RowMatch::EveryColumn { fixed: vec![] });
        assert_eq!(reason, None);

        // Table order, whatever order the result lists them in.
        let (reordered, _) = resolve_keyless_target(
            &sightings(&["place", "bird", "seen", "weight", "at"]),
            "dbdelve_dev",
            "sightings",
            &table,
        );
        assert_eq!(reordered.expect("reordered").keys, [1, 0, 2, 3, 4]);

        // A column the server computes is matched and never set.
        let (computed, _) = resolve_keyless_target(
            &sightings(&["bird", "place", "label"]),
            "dbdelve_dev",
            "sightings",
            &keyless(&[
                ("bird", "varchar", false),
                ("place", "varchar", false),
                ("label", "varchar", true),
            ]),
        );
        assert_eq!(
            computed.expect("a computed column is matched").row_match,
            RowMatch::EveryColumn { fixed: vec![2] }
        );

        let refused = |column: &str| {
            Some(format!(
                "A table without a primary key is edited by matching every column, \
                 and {column} cannot be matched by its value."
            ))
        };
        // FLOAT and JSON find nothing by `=` against their own text; the rest are
        // types no test matched.
        for data_type in [
            "float",
            "json",
            "bit",
            "geometry",
            "point",
            "blob",
            "binary",
            "varbinary",
            "tinyblob",
            "mediumblob",
            "longblob",
            "vector",
            "null",
            "",
        ] {
            let (target, reason) = resolve_keyless_target(
                &sightings(&["bird", "shape"]),
                "dbdelve_dev",
                "sightings",
                &keyless(&[("bird", "varchar", false), ("shape", data_type, false)]),
            );
            assert!(target.is_none(), "{data_type}");
            assert_eq!(
                reason,
                refused(&format!("shape ({data_type})")),
                "{data_type}"
            );
        }
        for data_type in [
            "tinyint",
            "smallint",
            "mediumint",
            "int",
            "bigint",
            "decimal",
            "double",
            "char",
            "varchar",
            "tinytext",
            "text",
            "mediumtext",
            "longtext",
            "date",
            "time",
            "datetime",
            "timestamp",
            "year",
            "enum",
            "set",
        ] {
            let (target, reason) = resolve_keyless_target(
                &sightings(&["one"]),
                "dbdelve_dev",
                "sightings",
                &keyless(&[("one", data_type, false)]),
            );
            assert!(target.is_some(), "{data_type}");
            assert_eq!(reason, None, "{data_type}");
        }

        // A column the result leaves out would be left out of the WHERE.
        let (target, reason) =
            resolve_keyless_target(&sightings(&["bird"]), "dbdelve_dev", "sightings", &table);
        assert!(target.is_none());
        assert_eq!(
            reason,
            Some(
                "A table without a primary key is edited by matching every column, \
                 and place is not in the result."
                    .to_string()
            )
        );
        // Computed beside real columns, and a self-join, are no table's rows.
        let one = keyless(&[("bird", "varchar", false)]);
        assert_eq!(
            resolve_keyless_target(
                &probed(&[(Some("sightings"), Some("bird")), (None, None)]),
                "dbdelve_dev",
                "sightings",
                &one,
            ),
            (None, None)
        );
        assert_eq!(
            resolve_keyless_target(
                &sightings(&["bird", "bird"]),
                "dbdelve_dev",
                "sightings",
                &one
            ),
            (None, None)
        );
    }

    #[test]
    fn a_tunnelled_connection_dials_the_tunnel_and_never_a_local_socket() {
        let opts: ::mysql::Opts = base_options(
            &ServerConfig::default(),
            Some("127.0.0.1:40000".parse().unwrap()),
        )
        .into();
        assert_eq!(opts.get_ip_or_hostname(), "127.0.0.1");
        assert_eq!(opts.get_tcp_port(), 40000);
        assert!(!opts.get_prefer_socket());
    }

    #[test]
    fn a_name_whose_first_address_refuses_is_tried_at_its_others() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listening = listener.local_addr().unwrap();
        // Hanging up is enough: the handshake then fails as something other
        // than a refusal, which only reaching the listener can cause.
        let reached = std::thread::spawn(move || listener.accept().is_ok());
        let at = |address| Conn::new(base_options(&ServerConfig::default(), Some(address)));
        let first = at(closed).expect_err("nothing listens on a closed port");
        assert!(unusable_address(&first), "{first}");

        let error = on_any_address(vec![closed, listening], first, at)
            .expect_err("the listener speaks no MySQL");
        assert!(!unusable_address(&error), "{error}");
        assert!(reached.join().unwrap());
    }

    #[test]
    fn an_address_dialled_for_a_name_never_switches_to_a_local_socket() {
        let server = ServerConfig {
            host: "db.example.test".into(),
            ..ServerConfig::default()
        };
        let opts: ::mysql::Opts = session_options(
            &server,
            Engine::MySql,
            Some("127.0.0.1:3306".parse().unwrap()),
        )
        .into();
        assert_eq!(opts.get_ip_or_hostname(), "127.0.0.1");
        assert!(!opts.get_prefer_socket());
    }

    #[test]
    fn a_cancel_dials_the_address_its_session_reached() {
        let server = ServerConfig {
            host: "db.example.test".into(),
            ..ServerConfig::default()
        };
        let reached = "192.0.2.7:3307".parse().unwrap();
        let opts: ::mysql::Opts =
            cancel_options(&server, dialled(None, Some(reached)).unwrap()).into();
        assert_eq!(opts.get_ip_or_hostname(), "192.0.2.7");
        assert_eq!(opts.get_tcp_port(), 3307);
        assert!(!opts.get_prefer_socket());
    }

    #[test]
    fn a_refused_connect_names_the_endpoint() {
        let server = ServerConfig {
            host: "db.example.test".into(),
            port: Some(3307),
            ..ServerConfig::default()
        };
        let refused = ::mysql::Error::DriverError(::mysql::DriverError::CouldNotConnect(Some((
            "db.example.test:3307".into(),
            "Connection refused (os error 111)".into(),
            std::io::ErrorKind::ConnectionRefused,
        ))));
        assert_eq!(
            connect_error(&refused, &server).message,
            "Connection refused: nothing is listening on db.example.test:3307"
        );
    }

    #[test]
    fn verify_full_through_a_tunnel_is_refused_before_ssh_starts() {
        let server = ServerConfig {
            host: "db.example.test".into(),
            sslmode: SslMode::VerifyFull,
            // Nothing that resolves: starting ssh at all would fail differently.
            ssm: None,
            ssh: Some(crate::db::SshTunnel {
                host: "dbdelve-nowhere.invalid".into(),
                ..crate::db::SshTunnel::default()
            }),
            ..ServerConfig::default()
        };
        let Err(error) = Connection::open(&server, Engine::MySql) else {
            panic!("verify-full through a tunnel connected");
        };
        assert!(
            error.message.starts_with(
                "sslmode=verify-full cannot be honoured through an SSH tunnel on MySQL"
            ),
            "{}",
            error.message
        );
    }

    #[test]
    fn ssm_does_not_weaken_mysql_hostname_verification() {
        let server = ServerConfig {
            host: "db.example.test".into(),
            sslmode: SslMode::VerifyFull,
            ssm: Some(crate::db::SsmTunnel {
                target: "i-0123456789abcdef0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let error = Connection::open(&server, Engine::MySql).err().unwrap();
        assert!(
            error
                .message
                .contains("cannot be honoured through an SSM tunnel")
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
        let opts: ::mysql::Opts = cancel_options(&server, None).into();

        let timeout = Some(Duration::from_secs(CANCEL_TIMEOUT_SECONDS));
        assert_eq!(opts.get_read_timeout(), timeout.as_ref());
        assert_eq!(opts.get_write_timeout(), timeout.as_ref());
        assert!(opts.get_init().is_empty());
    }

    #[test]
    fn a_placeholder_inside_a_name_is_not_replaced_again() {
        assert_eq!(
            structure_sql(
                "WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {relation}",
                "a{relation}",
                "; DROP TABLE t; --",
            ),
            "WHERE TABLE_SCHEMA = 'a{relation}' AND TABLE_NAME = '; DROP TABLE t; --'"
        );
    }

    #[test]
    fn a_table_found_without_transactions_is_named_in_the_refusal() {
        let column = |name: &str| Column {
            name: name.into(),
            data_type: None,
        };
        let found = QueryResult {
            columns: vec![column("column_name"), column("untransactional_engine")],
            rows: crate::db::rows(vec![
                vec![Some("id".into()), Some("MyISAM".into())],
                vec![Some("tenant".into()), Some("MyISAM".into())],
            ]),
            ..QueryResult::default()
        };
        assert!(untransactional_reason(&found).is_some_and(|reason| reason.contains("MyISAM")));
        assert_eq!(primary_key_columns(&found), ["id", "tenant"]);
        let transactional = QueryResult {
            rows: crate::db::rows(vec![vec![Some("id".into()), None]]),
            ..found.clone()
        };
        assert_eq!(untransactional_reason(&transactional), None);
        assert_eq!(primary_key_columns(&transactional), ["id"]);
    }

    #[test]
    fn a_table_with_no_primary_key_comes_back_as_one_row_with_no_column() {
        let column = |name: &str| Column {
            name: name.into(),
            data_type: None,
        };
        let keyless = QueryResult {
            columns: vec![column("column_name"), column("untransactional_engine")],
            rows: crate::db::rows(vec![vec![None, None]]),
            ..QueryResult::default()
        };
        assert_eq!(untransactional_reason(&keyless), None);
        assert!(primary_key_columns(&keyless).is_empty());
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
    #[ignore = "requires the dev bastions and MySQL configured through dbdelve_SSH_CONFIG and dbdelve_MYSQL_URL"]
    fn live_ssh_a_query_and_a_cancel_run_through_the_bastion() {
        let connection =
            Connection::open(&live_tunnelled(), Engine::MySql).expect("connection should open");
        let result = connection
            .query("SELECT 1 AS one", Fetch::default())
            .expect("query should succeed");
        assert_eq!(result.rows, vec![vec![Some("1".into())]]);

        // `KILL QUERY` needs a second connection, which has to find the
        // server through the same tunnel; so do the sizes, which also must not
        // wait behind the sleep holding the mutex.
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            assert!(
                canceller.sizes().expect("sizes should load")["dbdelve_dev"]
                    .contains_key("accounts")
            );
            canceller.cancel().expect("the KILL QUERY should send");
        });
        let started = Instant::now();
        let _ = connection.query("SELECT SLEEP(30)", Fetch::default());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A session on another database is held to reads when it is asked to be,
    /// from the moment it exists; one that was not asked is not.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_session_on_another_database_is_held_to_reads_when_asked() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            let plain = live(engine);
            let clean = || {
                plain
                    .query("DROP TABLE IF EXISTS dbdelve_held_probe", Fetch::default())
                    .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
            };
            clean();
            for statement in [
                "CREATE TABLE dbdelve_held_probe (id int)",
                "INSERT INTO dbdelve_held_probe VALUES (1)",
            ] {
                plain
                    .query(statement, Fetch::default())
                    .unwrap_or_else(|error| panic!("{engine:?}: {statement}: {error}"));
            }

            let profile = live_profile(engine);
            let unheld = profile
                .on_database("dbdelve_dev")
                .expect("a session should open");
            let write = |session: &super::super::Connection| {
                session.query(
                    "INSERT INTO dbdelve_held_probe VALUES (2)",
                    &crate::db::CancelToken::default(),
                    Fetch::default(),
                )
            };
            write(&unheld).unwrap_or_else(|error| panic!("{engine:?}: unheld: {error}"));

            let held = profile
                .on_database("dbdelve_dev")
                .expect("a session should open");
            held.set_read_only(true).expect("the hold should take");
            let refusal = write(&held)
                .err()
                .unwrap_or_else(|| panic!("{engine:?}: a held session accepted a write"));
            assert!(
                refusal.message.contains("READ ONLY"),
                "{engine:?}, the server's own words: {}",
                refusal.message
            );
            clean();
        }
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_session_on_another_database_is_on_it() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            let profile = live_profile(engine);
            let database = |connection: &super::super::Connection| {
                connection
                    .query(
                        "SELECT DATABASE()",
                        &crate::db::CancelToken::default(),
                        Fetch::default(),
                    )
                    .unwrap_or_else(|error| panic!("{engine:?}: {error}"))
                    .rows[0]
                    .iter()
                    .next()
                    .flatten()
                    .map(str::to_string)
            };
            let session = profile
                .on_database("information_schema")
                .expect("a session should open");
            assert_eq!(database(&session).as_deref(), Some("information_schema"));
            assert_eq!(
                database(&profile).as_deref(),
                Some("dbdelve_dev"),
                "{engine:?}: the profile's own connection is where it was"
            );
        }
    }

    /// A Cancel on a session opened for another database is that session's,
    /// and the next diagram read on the first is sent as usual.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_cancel_on_another_databases_session_leaves_the_firsts_diagram_alone() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            let first = live(engine);
            let other = first
                .on_database("information_schema")
                .expect("a session should open");
            other.cancel().expect("the cancel should be asked");
            first
                .schema_graph("dbdelve_dev")
                .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
        }
    }

    /// A used session is put back on its own database by a bound run, whichever
    /// database a statement of the user's moved it to.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_bound_run_moves_a_used_session_back_to_its_database() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            let session = live_profile(engine)
                .on_database("information_schema")
                .expect("a session should open");
            let cancel = crate::db::CancelToken::default();
            let database = |bound: bool| {
                let sql = "SELECT DATABASE()";
                let result = match bound {
                    true => session.query_bound(None, sql, &cancel, Fetch::default()),
                    false => session.query(sql, &cancel, Fetch::default()),
                };
                result
                    .ok()
                    .and_then(|result| result.rows.first()?.get(0).map(str::to_string))
            };
            session
                .query("USE dbdelve_dev", &cancel, Fetch::default())
                .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
            assert_eq!(database(false), Some("dbdelve_dev".into()), "{engine:?}");
            assert_eq!(
                database(true),
                Some("information_schema".into()),
                "{engine:?}: put back before the statement"
            );
            assert_eq!(
                database(false),
                Some("information_schema".into()),
                "{engine:?}: and it stays there"
            );
        }
    }

    /// The reads behind Show DDL, the diagram and an object tab's structure
    /// name their schema, so a `USE` of a tab's own cannot send them to a table
    /// of the same name in the database it named.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_used_sessions_reads_name_their_schema() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            let session = live(engine)
                .on_database("information_schema")
                .expect("a session should open");
            session
                .query("USE dbdelve_dev", Fetch::default())
                .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
            let structure = session
                .structure("information_schema", "SCHEMATA")
                .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
            assert!(
                structure
                    .columns
                    .iter()
                    .any(|column| column.name == "SCHEMA_NAME"),
                "{engine:?}"
            );
            let ddl = session
                .ddl("information_schema", "SCHEMATA")
                .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
            assert!(ddl.contains("SCHEMA_NAME"), "{engine:?}: {ddl}");
        }
    }

    fn live_profile(engine: Engine) -> super::super::Connection {
        super::super::Connection::MySql(live(engine))
    }

    fn live_reads_run_at_once_take_the_time_of_one(engine: Engine) {
        super::super::at_once::reads_overlap(
            &live_profile(engine),
            engine,
            "SELECT SLEEP(1), 1",
            "INSERT INTO accounts () VALUES ()",
        );
    }

    fn live_a_cancel_stops_every_read_run_at_once(engine: Engine) {
        super::super::at_once::a_cancel_stops_them_all(
            &live_profile(engine),
            engine,
            LIVE_SLOW_SELECT,
        );
    }

    fn live_a_killed_session_is_lost_and_a_failed_statement_is_not(engine: Engine) {
        let connection = live(engine);
        assert!(connection.query("SELECT broken", Fetch::default()).is_err());
        assert!(!connection.is_lost());

        assert!(
            connection
                .query("KILL CONNECTION_ID()", Fetch::default())
                .is_err()
        );
        assert!(connection.is_lost());
    }

    fn live_a_cancel_stops_a_running_statement_without_closing_the_session(engine: Engine) {
        // The connection id has to have been read in `open`: asking the live
        // connection for it here would want the mutex the sleeping statement is
        // holding, and this would hang rather than fail.
        let connection = live(engine);
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller.cancel().expect("the KILL QUERY should send");
        });

        let error = connection
            .query(LIVE_SLOW_SELECT, Fetch::default())
            .expect_err("the statement should be cancelled");
        assert!(
            error.message.contains("interrupt"),
            "the server's own words: {}",
            error.message
        );
        // `KILL QUERY` ends the statement, not the session.
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    fn live_a_schema_graph_reads_the_seeds_keys_back(engine: Engine) {
        use crate::db::graph::seed;

        let graph = live_profile(engine)
            .schema_graph("dbdelve_dev")
            .expect("the graph should read");

        assert!(
            graph
                .tables
                .iter()
                .all(|table| table.name.as_ref() != "account_overview"),
            "a view is not drawn"
        );
        seed::assert_keys_read_back(&graph);
        assert_eq!(
            graph.types[seed::column(&graph, "orders", "total").type_label as usize].as_ref(),
            "decimal(14,2)"
        );

        let archive = live_profile(engine)
            .schema_graph("dbdelve_archive")
            .expect("the graph should read");
        seed::assert_archive_points_at_accounts(&archive, "dbdelve_dev");
    }

    fn live_a_unique_index_on_a_prefix_makes_its_column_unique(engine: Engine) {
        let connection = live(engine);
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_prefix; \
                 CREATE TABLE dbdelve_test_prefix (id INT PRIMARY KEY, email VARCHAR(80), \
                     UNIQUE KEY by_email (email(10)))",
                Fetch::default(),
            )
            .expect("the fixture table should be created");
        let graph = connection.schema_graph("dbdelve_dev");
        connection
            .query("DROP TABLE dbdelve_test_prefix", Fetch::default())
            .expect("the fixture table should be cleaned up");
        let graph = graph.expect("the graph should read");
        assert!(crate::db::graph::seed::column(&graph, "dbdelve_test_prefix", "email").unique);
    }

    #[test]
    fn a_schema_named_like_a_placeholder_keeps_its_name_in_the_graph_sql() {
        let sql = graph_sql(GRAPH_COLUMNS_SQL, "tenant{stored}");
        assert!(sql.contains("'tenant{stored}'"), "{sql}");
        assert!(
            sql.contains("IN ('BASE TABLE', 'SYSTEM VERSIONED')"),
            "{sql}"
        );
    }

    fn live_a_cancel_on_its_own_connection_stops_a_catalog_statement(engine: Engine) {
        // The catalog reads of a diagram take no lock a second session could
        // hold, so a sleeping statement stands in for a slow one. `schema_graph`
        // is nothing but three of these on the side connection.
        let side = live(engine)
            .alongside()
            .expect("a side connection should open");
        let canceller = super::super::Connection::MySql(side.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            canceller
                .cancel(&crate::db::CancelToken::default())
                .expect("the KILL QUERY should send");
        });

        let started = Instant::now();
        let error = side
            .internal_query(LIVE_SLOW_SELECT)
            .expect_err("the statement should be cancelled");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(error.message.contains("interrupt"), "{}", error.message);
    }

    fn live_a_cancel_before_a_schema_graph_read_drops_it_unsent(engine: Engine) {
        // A `KILL QUERY` on an idle session stops nothing, so a Cancel that
        // came before the read, or between its statements, is kept for it.
        let side = live(engine)
            .alongside()
            .expect("a side connection should open");
        side.cancel().expect("the KILL QUERY should send");
        let error = side
            .schema_graph("dbdelve_dev")
            .expect_err("the read should be dropped");
        assert_eq!(error.message, "Cancelled before it was sent.");
        assert!(
            side.schema_graph("dbdelve_dev").is_ok(),
            "kept for one read"
        );
    }

    fn live_a_statement_timeout_bounds_a_select_and_a_write_only_on_mariadb(engine: Engine) {
        let config = live_config(engine);
        let connection = Connection::open(
            &ServerConfig {
                sslmode: SslMode::Disable,
                statement_timeout: 1,
                ..config
            },
            engine,
        )
        .expect("connection should open");

        let error = connection
            .query(LIVE_SLOW_SELECT, Fetch::default())
            .expect_err("a read-only SELECT should time out");
        assert!(error.message.contains("exceeded"), "{}", error.message);

        // The asymmetry `ServerConfig::statement_timeout` names: on MySQL the
        // same wait inside a statement that is not a `SELECT` is not bounded at
        // all, and Cancel is the only thing that reaches it.
        if engine == Engine::MariaDb {
            // Not `DO`, which MariaDB lets off with a warning when interrupted.
            let error = connection
                .query(
                    &format!("SET @bounded = ({LIVE_SLOW_SELECT})"),
                    Fetch::default(),
                )
                .expect_err("MariaDB bounds every statement");
            assert!(error.message.contains("exceeded"), "{}", error.message);
        } else {
            assert!(
                connection
                    .query("DO SLEEP(2)", Fetch::default())
                    .expect("a non-SELECT is not bounded")
                    .rows
                    .is_empty()
            );
        }
    }

    /// What each mode does against the compose server, which speaks TLS with a
    /// certificate signed by nobody.
    ///
    /// Hard rule 6 in code, and the mode the connection form actually defaults
    /// to. `prefer` and `require` promise encryption and no more, so a
    /// certificate they cannot check is not their business. The two verifying
    /// rungs refuse it and say TLS was the reason, rather than quietly
    /// connecting anyway.
    fn live_only_the_modes_that_tolerate_an_unchecked_certificate_connect(engine: Engine) {
        let base = live_config(engine);
        let connect = |sslmode| {
            Connection::open(
                &ServerConfig {
                    sslmode,
                    ..base.clone()
                },
                engine,
            )
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

    fn live_the_development_database_is_fully_seeded(engine: Engine) {
        // The seed is applied by a tool outside this test suite, and a seed that
        // half-applied still leaves something to connect to -- the MySQL
        // container reports itself healthy either way. So the volume table is
        // checked by count rather than assumed.
        let result = live(engine)
            .query(
                "SELECT count(*) AS rows_seeded FROM measurements",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(result.rows[0].get(0), Some("5000"));
    }

    fn live_the_databases_list_flags_the_one_connected_to(engine: Engine) {
        let databases = live(engine).databases().expect("databases should list");

        assert!(databases.names.contains(&"dbdelve_dev".to_string()));
        assert!(databases.names.contains(&"information_schema".to_string()));
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
    }

    fn live_a_blank_database_connects_to_none(engine: Engine) {
        let databases = Connection::open(
            &ServerConfig {
                database: String::new(),
                sslmode: SslMode::Disable,
                ..live_config(engine)
            },
            engine,
        )
        .expect("a blank database should connect")
        .databases()
        .expect("databases should list");

        assert_eq!(databases.current, None);
        assert!(databases.names.contains(&"dbdelve_dev".to_string()));
    }

    fn live_query_round_trip(engine: Engine) {
        let result = live(engine)
            .query(
                "SELECT 1 AS id, 'alpha' AS label UNION ALL SELECT 2, NULL",
                Fetch::default(),
            )
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

    fn live_a_multi_statement_selection_keeps_one_result_shape(engine: Engine) {
        let connection = live(engine);

        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d", Fetch::default())
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
            .query(
                "SELECT 1 AS id, 'x' AS label FROM DUAL WHERE false",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id", "label"]);
        assert!(empty.rows.is_empty());
    }

    fn live_a_column_is_tagged_with_the_type_the_server_would_name(engine: Engine) {
        let result = live(engine)
            .query("SELECT id, name, email FROM accounts", Fetch::default())
            .expect("query should succeed");

        // `bigint` and not `int`: the wire protocol carries a type code rather
        // than a name, and reading it as the narrower type would be a quiet lie
        // about what the column holds.
        assert_eq!(
            types(&result),
            vec![Some("bigint"), Some("text"), Some("text")]
        );
    }

    fn live_catalog_round_trip(engine: Engine) {
        let connection = live(engine);
        let mut catalog = connection.catalog().expect("catalog should load");
        catalog.merge(connection.routines().expect("routines should load"));
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
        assert!(
            schema
                .relations
                .iter()
                .all(|relation| relation.size.is_none()),
            "sizes arrive separately"
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

        let sizes = connection.sizes().expect("sizes should load");
        assert!(sizes["dbdelve_dev"].contains_key("accounts"));
        assert!(!sizes["dbdelve_dev"].contains_key("account_overview"));
        catalog.set_sizes(&sizes);
        assert!(
            catalog
                .schemas
                .iter()
                .filter(|schema| schema.name == "dbdelve_dev")
                .flat_map(|schema| &schema.relations)
                .any(|relation| relation.name == "accounts" && relation.size.is_some())
        );
    }

    /// The user's own database alone is no proof: the system schemas have to
    /// stay out while the one selected comes in. `information_schema` is the
    /// one an ordinary user may select.
    fn live_the_selected_database_is_listed_even_when_the_server_owns_it(engine: Engine) {
        let connection = Connection::open(
            &ServerConfig {
                sslmode: SslMode::Disable,
                database: "information_schema".into(),
                ..live_config(engine)
            },
            engine,
        )
        .expect("connection should open");
        let catalog = connection.catalog().expect("catalog should load");
        let names = catalog
            .schemas
            .iter()
            .map(|schema| schema.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"dbdelve_dev"), "{names:?}");
        assert!(!names.contains(&"mysql"), "{names:?}");
        assert!(!names.contains(&"performance_schema"), "{names:?}");
        let selected = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "information_schema")
            .expect("the selected database should be listed");
        assert!(
            selected
                .relations
                .iter()
                .any(|relation| relation.name == "TABLES" && relation.kind == RelationKind::View)
        );
    }

    fn live_an_empty_database_is_listed(engine: Engine) {
        let catalog = live(engine).catalog().expect("catalog should load");
        let empty = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "dbdelve_empty")
            .expect("the seeded empty database should be listed");
        assert!(empty.relations.is_empty());
    }

    /// MariaDB's own two kinds of table, which MySQL has neither of.
    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MARIADB_URL"]
    fn live_mariadb_lists_a_versioned_table_and_a_sequence() {
        let connection = live(Engine::MariaDb);
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_versioned; \
                 DROP SEQUENCE IF EXISTS dbdelve_test_sequence; \
                 CREATE TABLE dbdelve_test_versioned (id INT PRIMARY KEY, label TEXT) \
                     WITH SYSTEM VERSIONING; \
                 CREATE SEQUENCE dbdelve_test_sequence",
                Fetch::default(),
            )
            .expect("the fixtures should create");
        let catalog = connection.catalog();
        let sequences = connection.sequences();
        let sizes = connection.sizes();
        let versioned = connection.query("SELECT * FROM dbdelve_test_versioned", Fetch::default());
        let sequence = connection.query("SELECT * FROM dbdelve_test_sequence", Fetch::default());
        connection
            .query(
                "DROP TABLE dbdelve_test_versioned; DROP SEQUENCE dbdelve_test_sequence",
                Fetch::default(),
            )
            .expect("the fixtures should drop");

        let catalog = catalog.expect("catalog should load");
        let relations = &catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "dbdelve_dev")
            .expect("dbdelve_dev should exist")
            .relations;
        assert!(
            relations
                .iter()
                .any(|relation| relation.name == "dbdelve_test_versioned"
                    && relation.kind == RelationKind::Table)
        );
        assert!(
            !relations
                .iter()
                .any(|relation| relation.name == "dbdelve_test_sequence"),
            "a sequence is listed as a sequence and not among the relations"
        );
        let sequences = sequences.expect("sequences should load");
        assert!(
            sequences
                .schemas
                .iter()
                .find(|schema| schema.name == "dbdelve_dev")
                .is_some_and(|schema| schema
                    .sequences
                    .iter()
                    .any(|name| name == "dbdelve_test_sequence"))
        );
        assert!(
            sizes.expect("sizes should load")["dbdelve_dev"].contains_key("dbdelve_test_versioned")
        );
        let edit = versioned
            .expect("the versioned table should read")
            .edit
            .expect("a versioned table is edited by its primary key");
        assert_eq!(edit.table, "dbdelve_test_versioned");
        assert!(
            sequence.expect("the sequence should read").edit.is_none(),
            "a sequence has no primary key to name a row by"
        );
    }

    fn live_structure_round_trip(engine: Engine) {
        let structure = live(engine)
            .structure("dbdelve_dev", "accounts")
            .expect("structure should load");

        assert!(
            structure.columns.contains(&ColumnDefinition {
                name: "id".into(),
                // MariaDB still reports the display width MySQL 8 dropped.
                data_type: match engine {
                    Engine::MariaDb => "bigint(20)",
                    _ => "bigint",
                }
                .into(),
                nullable: false,
                // Not the empty default `COLUMN_DEFAULT` reports: the server
                // supplies this one, and saying otherwise would tell the user they
                // have to.
                default: Some("AUTO_INCREMENT".into()),
            })
        );
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
        let mut kinds: Vec<_> = structure
            .indexes
            .iter()
            .map(|index| (index.name.as_str(), index.kind, index.columns.clone()))
            .collect();
        kinds.sort_by_key(|(name, ..)| *name);
        assert_eq!(
            kinds,
            [
                ("PRIMARY", IndexKind::Primary, vec!["id".into()]),
                ("external_id", IndexKind::Unique, vec!["external_id".into()]),
            ]
        );

        let connection = live(engine);
        // MariaDB has no functional key part. The second is longer than
        // `group_concat_max_len`'s default 1024 bytes.
        let functional = match engine {
            Engine::MariaDb => String::new(),
            _ => format!(
                ", KEY by_lower ((lower(email)), id DESC), KEY by_sum ((id{}))",
                " + 1".repeat(300)
            ),
        };
        connection
            .query(
                &format!(
                    "DROP TABLE IF EXISTS dbdelve_test_indexes; \
                     CREATE TABLE dbdelve_test_indexes (id INT, email VARCHAR(80), \
                         UNIQUE KEY by_email (email(10), id){functional})"
                ),
                Fetch::default(),
            )
            .expect("the fixture table should be created");
        let fixture = connection.structure("dbdelve_dev", "dbdelve_test_indexes");
        connection
            .query("DROP TABLE dbdelve_test_indexes", Fetch::default())
            .expect("the fixture table should be cleaned up");
        let fixture = fixture.expect("structure should load");
        let by_email = &fixture.indexes[0];
        assert_eq!(by_email.kind, IndexKind::Unique);
        assert_eq!(by_email.columns, ["email(10)", "id"]);
        assert_eq!(by_email.predicate, None);
        if engine == Engine::MySql {
            assert_eq!(fixture.indexes[1].columns, ["lower(`email`)", "id DESC"]);
            assert_eq!(fixture.indexes[1].definition, "INDEX (id)");
            let by_sum = &fixture.indexes[2].columns;
            assert!(
                by_sum[0].len() > 1200 && by_sum[0].ends_with("+ 1)"),
                "{by_sum:?}"
            );
        }
    }

    fn live_a_single_table_select_is_editable_by_its_primary_key(engine: Engine) {
        let edit = live(engine)
            .query("SELECT name, id FROM accounts", Fetch::default())
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

    fn live_an_aliased_or_computed_column_reports_what_the_table_calls_it(engine: Engine) {
        let edit = live(engine)
            .query(
                "SELECT id AS ident, upper(name) AS shouted, name FROM accounts",
                Fetch::default(),
            )
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");

        assert_eq!(
            edit.columns,
            vec![Some("id".to_string()), None, Some("name".to_string())]
        );
        assert_eq!(edit.keys, vec![0]);
    }

    /// A table on an engine with no transactions is not edited: the verified
    /// apply's `ROLLBACK` would not undo what it wrote.
    fn live_a_table_without_transactions_is_not_editable(engine: Engine) {
        let connection = live(engine);
        // Not a temporary table: `information_schema.TABLES` does not list one.
        connection
            .query(
                "DROP TABLE IF EXISTS dbdelve_test_myisam; \
                 CREATE TABLE dbdelve_test_myisam (id INT PRIMARY KEY, name TEXT) \
                 ENGINE = MyISAM",
                Fetch::default(),
            )
            .expect("a MyISAM table is created");
        let result = connection.query("SELECT id, name FROM dbdelve_test_myisam", Fetch::default());
        connection
            .query("DROP TABLE dbdelve_test_myisam", Fetch::default())
            .expect("the table is dropped");
        let result = result.expect("query should succeed");
        assert!(result.edit.is_none());
        assert!(
            result
                .read_only_reason
                .is_some_and(|reason| reason.contains("MyISAM")),
        );
    }

    fn live_a_join_an_aggregate_or_a_missing_key_is_not_editable(engine: Engine) {
        let connection = live(engine);

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
                    .query(sql, Fetch::default())
                    .expect("query should succeed")
                    .edit
                    .is_none(),
                "{sql}"
            );
        }
    }

    fn live_the_foreign_key_fixtures_are_seeded(engine: Engine) {
        // The foreign-key tests have nothing to read unless the reseed that
        // added `orders`, `order_items` and the `dbdelve_archive` database has
        // actually been applied.
        let connection = live(engine);

        let items = connection
            .query(
                "SELECT count(*) AS rows_seeded FROM order_items",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(items.rows[0].get(0), Some("3"));

        let closed = connection
            .query(
                "SELECT count(*) AS rows_seeded FROM dbdelve_archive.closed_accounts",
                Fetch::default(),
            )
            .expect("query should succeed");
        assert_eq!(closed.rows[0].get(0), Some("2"));
    }

    fn live_a_composite_foreign_key_arrives_as_one_key_per_column_in_key_order(engine: Engine) {
        let structure = live(engine)
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

    fn live_a_foreign_key_across_databases_names_the_database_it_references(engine: Engine) {
        let structure = live(engine)
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

    fn live_a_table_referencing_nothing_reports_no_foreign_keys(engine: Engine) {
        let structure = live(engine)
            .structure("dbdelve_dev", "accounts")
            .expect("structure should load");

        assert!(structure.foreign_keys.is_empty());
    }

    fn live_the_rendered_foreign_key_ddl_survives_beside_the_structured_form(engine: Engine) {
        let structure = live(engine)
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

    /// Every row of the seed's `measurements` against every other: far more
    /// rows than a drain could get through quickly, sent as they are made.
    const LIVE_ENDLESS_SELECT: &str = "SELECT a.id FROM measurements a JOIN measurements b";

    fn live_a_row_limit_keeps_the_first_rows_and_counts_the_rest(engine: Engine) {
        let connection = live(engine);
        let capped = connection
            .query(
                "SELECT id FROM measurements ORDER BY id LIMIT 10",
                Fetch {
                    limit: Some(3),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        assert_eq!(capped.rows.len(), 3);
        assert_eq!(capped.capped_from, Some(10));
        assert_eq!(capped.total_rows(), 10);
    }

    fn live_a_fed_query_hands_its_typed_rows_to_the_feed(engine: Engine) {
        let connection = live(engine);
        let feed = crate::db::Feed::default();
        let result = connection
            .query(
                "SELECT id FROM measurements ORDER BY id LIMIT 10",
                Fetch {
                    limit: Some(3),
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        let fed = feed.take();

        assert!(result.rows.is_empty());
        assert_eq!(fed.rows.len(), 3);
        // Typed from the description, before any row arrived.
        assert!(fed.columns[0].data_type.is_some());
    }

    fn live_a_read_past_the_limit_is_stopped_rather_than_drained(engine: Engine) {
        let connection = live(engine);
        let started = Instant::now();
        let result = connection
            .query(
                LIVE_ENDLESS_SELECT,
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
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    fn live_a_read_inside_the_users_transaction_is_stopped_and_the_transaction_kept(
        engine: Engine,
    ) {
        let connection = live(engine);
        connection.query("BEGIN", Fetch::default()).unwrap();
        connection
            .query("CREATE TEMPORARY TABLE kept (n INT)", Fetch::default())
            .unwrap();
        connection
            .query("INSERT INTO kept VALUES (1)", Fetch::default())
            .unwrap();
        let result = connection
            .query(
                LIVE_ENDLESS_SELECT,
                Fetch {
                    limit: Some(3),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .unwrap();

        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        // A killed statement is rolled back alone: the insert before it is
        // still there to read inside the same transaction.
        let kept = connection
            .query("SELECT n FROM kept", Fetch::default())
            .unwrap();
        assert_eq!(kept.rows, vec![vec![Some("1".to_string())]]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    fn live_a_write_reports_the_rows_it_affected(engine: Engine) {
        let connection = live(engine);
        connection
            .query("CREATE TEMPORARY TABLE counted (n INT)", Fetch::default())
            .unwrap();
        let inserted = connection
            .query("INSERT INTO counted VALUES (1), (2), (3)", Fetch::default())
            .unwrap();
        let updated = connection
            .query(
                "UPDATE counted SET n = n + 10 WHERE n > 1",
                Fetch::default(),
            )
            .unwrap();

        assert_eq!(inserted.rows_affected, Some(3));
        assert_eq!(updated.rows_affected, Some(2));
    }

    fn live_a_statements_warnings_arrive_as_messages(engine: Engine) {
        let connection = live(engine);
        let dropped = connection
            .query("DROP TABLE IF EXISTS no_such_163", Fetch::default())
            .unwrap();
        assert_eq!(dropped.messages.len(), 1);
        assert!(
            dropped.messages[0].starts_with("Note (1051): Unknown table"),
            "{:?}",
            dropped.messages
        );

        connection
            .query(
                "CREATE TEMPORARY TABLE narrow_163 (a CHAR(2))",
                Fetch::default(),
            )
            .unwrap();
        let truncated = connection
            .query(
                "INSERT IGNORE INTO narrow_163 VALUES ('abcd')",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(truncated.rows_affected, Some(1));
        assert_eq!(
            truncated.messages,
            vec!["Warning (1265): Data truncated for column 'a' at row 1".to_string()]
        );

        let selected = connection.query("SELECT 1", Fetch::default()).unwrap();
        assert!(selected.messages.is_empty());

        let cast = connection
            .query("SELECT CAST('abc' AS SIGNED)", Fetch::default())
            .unwrap();
        assert_eq!(cast.messages.len(), 1, "{:?}", cast.messages);
        assert!(cast.messages[0].starts_with("Warning (1292)"));
    }

    fn live_a_multi_statement_submission_counts_the_warnings_it_cannot_list(engine: Engine) {
        let connection = live(engine);
        let result = connection
            .query(
                "DROP TABLE IF EXISTS no_such_163; DROP TABLE IF EXISTS no_such_163_b; SELECT 1",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(
            result.messages,
            vec![
                "An earlier statement raised 2 warnings, which the server no longer lists."
                    .to_string()
            ]
        );

        let last = connection
            .query(
                "SELECT 1; DROP TABLE IF EXISTS no_such_163",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(last.messages.len(), 1, "{:?}", last.messages);
        assert!(last.messages[0].starts_with("Note (1051)"));
    }

    /// The cancel's `KILL QUERY` is a login of its own, so it runs the
    /// command too.
    fn live_a_password_command_signs_in_the_session_and_its_cancel(engine: Engine) {
        let config = live_config(engine);
        let command = format!("printf %s '{}'", config.password.replace('\'', r"'\''"));
        let connection = Connection::open(
            &ServerConfig {
                sslmode: SslMode::Disable,
                password: String::new(),
                password_command: Some(crate::db::PasswordCommand {
                    command,
                    expires: 0,
                }),
                ..config
            },
            engine,
        )
        .expect("the command's password should sign in");
        connection
            .cancel()
            .expect("the cancel's login should sign in too");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_mariadb_lists_a_sequence_in_its_own_kind() {
        // In the database the login owns, which cannot create another.
        let connection = live(Engine::MariaDb);
        let drop = |connection: &Connection| {
            connection
                .query(
                    "DROP SEQUENCE IF EXISTS dbdelve_sequence_probe_ticket",
                    Fetch::default(),
                )
                .expect("the probe sequence should drop");
        };
        drop(&connection);
        connection
            .query(
                "CREATE SEQUENCE dbdelve_sequence_probe_ticket",
                Fetch::default(),
            )
            .expect("the probe sequence should be created");

        let second = super::super::Connection::MySql(live(Engine::MariaDb))
            .routines()
            .expect("the second half should read");
        let listed: Vec<_> = second
            .schemas
            .iter()
            .filter(|schema| schema.name == "dbdelve_dev")
            .flat_map(|schema| schema.sequences.clone())
            .filter(|name| name == "dbdelve_sequence_probe_ticket")
            .collect();

        drop(&connection);
        assert_eq!(listed, vec!["dbdelve_sequence_probe_ticket".to_string()]);
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_mariadb_shows_a_sequences_create_statement() {
        let connection = live(Engine::MariaDb);
        let drop = |connection: &Connection| {
            connection
                .query(
                    "DROP SEQUENCE IF EXISTS dbdelve_sequence_probe_ticket",
                    Fetch::default(),
                )
                .expect("the probe sequence should drop");
        };
        drop(&connection);
        connection
            .query(
                "CREATE SEQUENCE dbdelve_sequence_probe_ticket START WITH 5 INCREMENT BY 2",
                Fetch::default(),
            )
            .expect("the probe sequence should be created");
        connection
            .query(
                "SELECT NEXTVAL(dbdelve_sequence_probe_ticket)",
                Fetch::default(),
            )
            .expect("the counter moves");
        let database = live_config(Engine::MariaDb).database;
        let ddl = connection.sequence_ddl(&database, "dbdelve_sequence_probe_ticket");
        drop(&connection);
        let ddl = ddl.expect("the definition is read");
        assert!(ddl.starts_with("CREATE SEQUENCE"), "{ddl}");
        assert!(ddl.contains("dbdelve_sequence_probe_ticket"), "{ddl}");
        assert!(ddl.ends_with(';'), "{ddl}");
        assert!(
            ddl.to_lowercase().contains("start with 5"),
            "the statement carries the start and not the counter: {ddl}"
        );
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_mysql_shows_no_sequence_definition() {
        let connection = live(Engine::MySql);
        assert!(connection.sequence_ddl("dbdelve_dev", "anything").is_err());
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_mysql_lists_no_sequence_and_sends_no_sequence_statement() {
        let connection = live(Engine::MySql);
        let second = super::super::Connection::MySql(live(Engine::MySql))
            .routines()
            .expect("the second half should read");
        assert!(
            second
                .schemas
                .iter()
                .all(|schema| schema.sequences.is_empty())
        );

        // A statement sent to a lost session fails, and a MySQL server answers
        // a `TABLE_TYPE = 'SEQUENCE'` filter with no rows anyway, so only a
        // dead session tells a statement never sent from one that found nothing.
        assert!(
            connection
                .query("KILL CONNECTION_ID()", Fetch::default())
                .is_err()
        );
        assert!(connection.is_lost());
        let catalog = connection
            .sequences()
            .expect("a MySQL connection sends no sequence statement");
        assert!(catalog.schemas.is_empty());
    }

    /// Each `live_` body above, run once against each server the driver
    /// speaks to.
    macro_rules! on_both_servers {
        ($($name:ident),* $(,)?) => {
            mod on_mysql {
                use crate::db::Engine;
                $(
                    #[test]
                    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
                    fn $name() {
                        super::$name(Engine::MySql)
                    }
                )*
            }
            mod on_mariadb {
                use crate::db::Engine;
                $(
                    #[test]
                    #[ignore = "requires the repository development database configured through dbdelve_MARIADB_URL"]
                    fn $name() {
                        super::$name(Engine::MariaDb)
                    }
                )*
            }
        };
    }

    fn live_the_ddl_of_a_table_and_a_view_is_the_servers_own(engine: Engine) {
        let connection = live(engine);
        let table = connection
            .ddl("dbdelve_dev", "accounts")
            .expect("the table's DDL should load");
        let view = connection
            .ddl("dbdelve_dev", "account_overview")
            .expect("the view's DDL should load");

        assert!(table.starts_with("CREATE TABLE `accounts` ("), "{table}");
        assert!(table.contains("`email`"), "{table}");
        assert!(table.ends_with(';'), "{table}");
        assert!(
            view.starts_with("CREATE ") && view.contains("VIEW `"),
            "{view}"
        );
    }

    /// A one-step verified `UPDATE` through the server's own session, on a row
    /// of `conformance` of this test's alone.
    fn live_a_same_value_update_counts_its_matched_row_in_any_server_language(engine: Engine) {
        use crate::db::{ApplyOutcome, CancelToken, StepKind, conformance::verified_steps};

        let connection = live_profile(engine);
        let table = engine.qualified("dbdelve_dev", "conformance");
        let run = |sql: &str| {
            connection
                .query(sql, &CancelToken::default(), Fetch::default())
                .unwrap_or_else(|error| panic!("{engine:?}: `{sql}` failed: {error}"))
        };
        let delete = format!("DELETE FROM {table} WHERE id = 921");
        run(&delete);
        run(&format!(
            "INSERT INTO {table} (id, label) VALUES (921, {})",
            engine.quote_literal("same")
        ));
        let row = run(&format!("SELECT * FROM {table} WHERE id = 921"));
        let before: Vec<FetchedCell> = row
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| FetchedCell {
                column: column.name.clone(),
                text: row.rows[0].get(index).map(str::to_string),
                cell_type: None,
                column_type: column.data_type.clone(),
            })
            .collect();
        let write = |id: u32| {
            format!(
                "UPDATE {table} SET {} = {} WHERE {} = '{id}'",
                engine.quote_identifier("label"),
                engine.quote_literal("same"),
                engine.quote_identifier("id"),
            )
        };
        let steps = verified_steps(
            engine,
            &write(921),
            vec![(StepKind::Update, before.clone())],
        );
        // Every re-read runs before the first write, so both find the row; the
        // `DELETE` before the update then takes it, and the update's count of
        // none is what fails the apply.
        let delete_first = crate::sql::draft_batch(
            engine,
            &[crate::sql::DraftDelete {
                schema: "dbdelve_dev".into(),
                table: "conformance".into(),
                keys: vec![("id".into(), "921".into())],
                null_keys: Vec::new(),
                types: vec![("id".into(), "int".into())],
            }],
            &[],
            &[],
        )
        .expect("the delete is written");
        let missing = [
            verified_steps(engine, &delete_first, vec![(StepKind::Delete, before)]).remove(0),
            steps[0].clone(),
        ];
        for locale in ["en_US", "ru_RU", "ja_JP"] {
            run(&format!("SET SESSION lc_messages = '{locale}'"));
            let outcome = connection.apply_verified(&steps, &CancelToken::default());
            assert!(
                matches!(outcome, ApplyOutcome::Applied { affected: 1, .. }),
                "{engine:?} in {locale}: {outcome:?}"
            );
            let outcome = connection.apply_verified(&missing, &CancelToken::default());
            assert_eq!(
                outcome,
                ApplyOutcome::Miscounted {
                    step: 1,
                    count: Some(0)
                },
                "{engine:?} in {locale}"
            );
        }
        run("SET SESSION lc_messages = 'en_US'");
        run(&delete);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_a_same_value_update_counts_its_matched_row_in_any_server_language_on_mysql() {
        live_a_same_value_update_counts_its_matched_row_in_any_server_language(Engine::MySql);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MARIADB_URL"]
    fn live_a_same_value_update_counts_its_matched_row_in_any_server_language_on_mariadb() {
        live_a_same_value_update_counts_its_matched_row_in_any_server_language(Engine::MariaDb);
    }

    /// MySQL refuses a trigger that writes its own table, so this one only
    /// notes the delete: any trigger on a table the draft wrote first refuses
    /// a later keyless step.
    fn live_a_keyless_edit_after_a_write_a_trigger_follows_is_refused(engine: Engine) {
        crate::db::conformance::keyless_edit_after_a_compacting_delete(
            &live_profile(engine),
            engine,
            "dbdelve_dev",
            &[
                "CREATE TABLE dbdelve_dev.queue (pos INT, name VARCHAR(10)) ENGINE = InnoDB",
                "CREATE TRIGGER dbdelve_dev.queue_noted AFTER DELETE ON dbdelve_dev.queue \
                 FOR EACH ROW SET @dbdelve_deleted = OLD.pos",
            ],
        );
    }

    /// A table moved onto an engine with no transactions after its grid loaded
    /// is refused inside the apply's transaction, under its metadata lock, even
    /// for an insert alone; nothing is written and no transaction is left open.
    fn live_an_apply_to_a_table_moved_off_innodb_is_refused_and_closed(engine: Engine) {
        use crate::db::{ApplyOutcome, ApplyStage, CancelToken, StepKind};

        let connection = live_profile(engine);
        let run = |sql: &str| {
            connection
                .query(sql, &CancelToken::default(), Fetch::default())
                .unwrap_or_else(|error| panic!("{engine:?}: `{sql}` failed: {error}"))
        };
        run("DROP TABLE IF EXISTS dbdelve_test_moved_engine");
        run(
            "CREATE TABLE dbdelve_test_moved_engine (id INT PRIMARY KEY, label TEXT) \
             ENGINE = InnoDB",
        );
        let insert = format!(
            "INSERT INTO {} ({}, {}) VALUES ('1', 'new')",
            engine.qualified("dbdelve_dev", "dbdelve_test_moved_engine"),
            engine.quote_identifier("id"),
            engine.quote_identifier("label"),
        );
        let steps = [VerifiedStep {
            sql: crate::sql::generated_writes(engine, &insert)
                .and_then(|mut statements| statements.pop())
                .unwrap_or_else(|| panic!("{engine:?}: the gate refuses `{insert}`")),
            kind: StepKind::Insert,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        }];
        run("ALTER TABLE dbdelve_test_moved_engine ENGINE = MyISAM");

        let outcome = connection.apply_verified(&steps, &CancelToken::default());

        assert!(
            matches!(
                &outcome,
                ApplyOutcome::Failed { stage: ApplyStage::Begin, error }
                    if error.message.contains("MyISAM")
            ),
            "{engine:?}: {outcome:?}"
        );
        // Refused with 1568 inside a transaction.
        run("SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
        let rows = run("SELECT count(*) FROM dbdelve_test_moved_engine");
        run("DROP TABLE dbdelve_test_moved_engine");
        assert_eq!(rows.rows[0].get(0), Some("0"), "{engine:?}");
    }

    #[test]
    fn a_table_is_opened_for_its_metadata_lock_by_a_read_of_no_rows() {
        assert_eq!(
            metadata_lock_sql(Engine::MySql, "shop", "order`s"),
            "SELECT 1 FROM `shop`.`order``s` LIMIT 0"
        );
    }

    /// An apply with `autocommit` off, after a write of the user's in it, is
    /// refused before its `BEGIN`, which would commit that write; the user's
    /// transaction is theirs to end, and `ROLLBACK` still undoes the write.
    fn live_an_apply_with_autocommit_off_is_refused_and_leaves_the_users_write_open(
        engine: Engine,
    ) {
        use crate::db::{ApplyOutcome, ApplyStage, CancelToken, StepKind};

        let connection = live_profile(engine);
        let table = engine.qualified("dbdelve_dev", "conformance");
        let run = |sql: &str| {
            connection
                .query(sql, &CancelToken::default(), Fetch::default())
                .unwrap_or_else(|error| panic!("{engine:?}: `{sql}` failed: {error}"))
        };
        let delete = format!("DELETE FROM {table} WHERE id IN (931, 932)");
        run(&delete);
        run(&format!(
            "INSERT INTO {table} (id, label) VALUES (931, 'old')"
        ));
        let insert = format!(
            "INSERT INTO {table} ({}, {}) VALUES ('932', 'new')",
            engine.quote_identifier("id"),
            engine.quote_identifier("label"),
        );
        let steps = [VerifiedStep {
            sql: crate::sql::generated_writes(engine, &insert)
                .and_then(|mut statements| statements.pop())
                .unwrap_or_else(|| panic!("{engine:?}: the gate refuses `{insert}`")),
            kind: StepKind::Insert,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        }];
        run("SET autocommit = 0");
        run(&format!(
            "UPDATE {table} SET label = 'users' WHERE id = 931"
        ));
        let outcome = connection.apply_verified(&steps, &CancelToken::default());
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Begin,
                error: crate::db::in_a_transaction(),
            },
            "{engine:?}"
        );
        run("ROLLBACK");
        run("SET autocommit = 1");
        let label = run(&format!("SELECT label FROM {table} WHERE id = 931"));
        assert_eq!(
            label.rows[0].get(0),
            Some("old"),
            "{engine:?}: nothing committed"
        );
        let added = run(&format!("SELECT id FROM {table} WHERE id = 932"));
        assert!(added.rows.is_empty(), "{engine:?}: the apply wrote nothing");
        run(&delete);
    }

    /// Drops scratch tables when a test ends, panicking or not.
    struct Scratch<'a>(&'a Connection, &'a [&'a str]);

    impl Scratch<'_> {
        fn drop_all(&self) {
            for name in self.1 {
                let _ = self.0.query(
                    &format!("DROP TABLE IF EXISTS {name}; DROP VIEW IF EXISTS {name}"),
                    Fetch::default(),
                );
            }
        }
    }

    impl Drop for Scratch<'_> {
        fn drop(&mut self) {
            self.drop_all();
        }
    }

    /// One column of each listed type, with a value that shows how it renders.
    const TYPE_SAMPLES: &[(&str, &str)] = &[
        ("tinyint", "-5"),
        ("tinyint unsigned", "250"),
        ("smallint", "-300"),
        ("mediumint", "-8388608"),
        ("int", "42"),
        ("int unsigned", "4000000000"),
        ("bigint", "9007199254740993"),
        ("bigint unsigned", "18446744073709551615"),
        ("decimal(10,2)", "-12.50"),
        ("double", "0.30000000000000004"),
        ("char(3)", "'ab'"),
        ("varchar(32)", "'h''e llo, \\\\ {a: 1}'"),
        ("varchar(32)", "''"),
        ("tinytext", "'tiny'"),
        ("text", "'tab\\there\\nnewline \u{fc}n\u{ef}'"),
        ("mediumtext", "'medium'"),
        ("longtext", "'long'"),
        ("date", "'2024-02-29'"),
        ("time(6)", "'12:34:56.123456'"),
        ("time", "'-838:59:59'"),
        ("datetime(6)", "'2024-02-29 12:34:56.123456'"),
        ("timestamp(6) NULL", "'2024-06-01 00:00:00.5'"),
        ("year", "2024"),
        ("enum('a','b')", "'b'"),
        ("set('x','y')", "'x,y'"),
    ];

    /// Types left off the list because `=` against the text the grid renders for
    /// them finds nothing, on the engines it holds on: a `FLOAT` is compared to
    /// the text as a double, which it is not, a `JSON` value is never equal to a
    /// string, and the byte types render as `0x…` and are written back as that
    /// text. MariaDB's `JSON` is a `LONGTEXT` the catalog query tells by its
    /// check; the text equals itself there, and the type is left off all the
    /// same.
    const UNMATCHED_SAMPLES: &[(&str, &str, &[Engine])] = &[
        ("float", "0.1", &[Engine::MySql, Engine::MariaDb]),
        ("json", "'{\"a\": [1,   2]}'", &[Engine::MySql]),
        ("bit(8)", "b'101'", &[Engine::MySql, Engine::MariaDb]),
        ("binary(4)", "0x6162", &[Engine::MySql, Engine::MariaDb]),
        (
            "varbinary(8)",
            "0xdeadbeef",
            &[Engine::MySql, Engine::MariaDb],
        ),
        ("tinyblob", "0x00ff", &[Engine::MySql, Engine::MariaDb]),
        ("blob", "0x00ff", &[Engine::MySql, Engine::MariaDb]),
        ("mediumblob", "0x00ff", &[Engine::MySql, Engine::MariaDb]),
        ("longblob", "0x00ff", &[Engine::MySql, Engine::MariaDb]),
    ];

    /// Types left off the list that nothing was matched with: the grid is
    /// read-only and says which column is why.
    const UNLISTED_SAMPLES: &[(&str, &str)] = &[("geometry", "ST_GeomFromText('POINT(1 2)')")];

    fn live_every_listed_type_matches_its_row_by_its_rendered_text(engine: Engine) {
        let connection = live(engine);
        let scratch = Scratch(
            &connection,
            &[
                "dbdelve_keyless_types",
                "dbdelve_keyless_probe",
                "dbdelve_keyless_view",
            ],
        );
        scratch.drop_all();
        let run = |sql: &str| connection.query(sql, Fetch::default());
        // A `timestamp` is matched by its value only under a fixed-offset zone.
        run("SET SESSION time_zone = '+00:00'").expect("the session zone is set");
        let columns: Vec<String> = TYPE_SAMPLES
            .iter()
            .enumerate()
            .map(|(index, (declared, _))| format!("c{index} {declared}"))
            .collect();
        let values: Vec<&str> = TYPE_SAMPLES.iter().map(|(_, value)| *value).collect();
        run(&format!(
            "CREATE TABLE dbdelve_keyless_types ({})",
            columns.join(", ")
        ))
        .expect("the scratch table is built");
        run(&format!(
            "INSERT INTO dbdelve_keyless_types VALUES ({})",
            values.join(", ")
        ))
        .expect("the scratch row is inserted");

        let read = run("SELECT * FROM dbdelve_keyless_types").expect("the row reads");
        let edit = read.edit.clone().unwrap_or_else(|| {
            panic!(
                "every column is of a listed type: {:?}",
                read.read_only_reason
            )
        });
        assert_eq!(
            edit.row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![] }
        );
        assert_eq!(edit.keys.len(), TYPE_SAMPLES.len());
        let count = |column: &str, literal: &str| {
            run(&format!(
                "SELECT COUNT(*) FROM dbdelve_keyless_types WHERE {} = {literal}",
                engine.quote_identifier(column)
            ))
            .map(|result| result.rows[0].get(0).unwrap_or("NULL").to_string())
            .map_err(|error| error.message)
        };
        for (index, (declared, value)) in TYPE_SAMPLES.iter().enumerate() {
            let column = &read.columns[index];
            let rendered = read.rows[0].get(index).expect("a value");
            // The literal the keyless WHERE writes for it.
            let literal = engine.quote_value(rendered, column.data_type.as_deref());
            assert_eq!(
                count(&column.name, &literal).as_deref(),
                Ok("1"),
                "{declared}: {value} -> {rendered}"
            );
        }

        for (declared, value, finds_nothing) in UNMATCHED_SAMPLES {
            let scratch = Scratch(&connection, &["dbdelve_keyless_probe"]);
            scratch.drop_all();
            run(&format!(
                "CREATE TABLE dbdelve_keyless_probe (c {declared})"
            ))
            .expect("the scratch table is built");
            run(&format!(
                "INSERT INTO dbdelve_keyless_probe VALUES ({value})"
            ))
            .expect("the scratch row is inserted");
            let read = run("SELECT * FROM dbdelve_keyless_probe").expect("the row reads");
            assert!(read.edit.is_none(), "{declared}");
            let kind = declared.split('(').next().unwrap_or(declared);
            assert!(
                read.read_only_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(&format!("c ({kind})"))),
                "{declared}: {:?}",
                read.read_only_reason
            );
            if finds_nothing.contains(&engine) {
                let rendered = read.rows[0].get(0).expect("a value");
                let counted = run(&format!(
                    "SELECT COUNT(*) FROM dbdelve_keyless_probe WHERE c = {}",
                    engine.quote_literal(rendered)
                ))
                .map(|result| result.rows[0].get(0).unwrap_or("NULL").to_string())
                .map_err(|error| error.message);
                assert_ne!(counted.as_deref(), Ok("1"), "{declared}: {rendered}");
            }
        }

        for (declared, value) in UNLISTED_SAMPLES {
            let scratch = Scratch(&connection, &["dbdelve_keyless_probe"]);
            scratch.drop_all();
            run(&format!(
                "CREATE TABLE dbdelve_keyless_probe (c {declared})"
            ))
            .expect("the scratch table is built");
            run(&format!(
                "INSERT INTO dbdelve_keyless_probe VALUES ({value})"
            ))
            .expect("the scratch row is inserted");
            let read = run("SELECT * FROM dbdelve_keyless_probe").expect("the row reads");
            assert!(read.edit.is_none(), "{declared}");
            let kind = declared.split('(').next().unwrap_or(declared);
            assert!(
                read.read_only_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(&format!("c ({kind})"))),
                "{declared}: {:?}",
                read.read_only_reason
            );
        }

        // A column the server computes is matched and never set; a default it
        // computes is an ordinary column. A unique index is no primary key.
        scratch.drop_all();
        run("CREATE TABLE dbdelve_keyless_probe (
                a int NOT NULL UNIQUE,
                g int AS (a + 1) STORED,
                v int AS (a + 2) VIRTUAL,
                d datetime DEFAULT CURRENT_TIMESTAMP,
                x int DEFAULT (3)
            )")
        .expect("the table is built");
        run("INSERT INTO dbdelve_keyless_probe (a, x) VALUES (1, 4)").expect("a row");
        let computed = run("SELECT * FROM dbdelve_keyless_probe").expect("the row reads");
        assert_eq!(
            computed
                .edit
                .expect("a unique index alone is no key")
                .row_match,
            crate::db::RowMatch::EveryColumn { fixed: vec![1, 2] },
            "{:?}",
            computed.read_only_reason
        );
        scratch.drop_all();

        // A view over a table with no primary key names no row of its own.
        run("CREATE TABLE dbdelve_keyless_probe (c int)").expect("the table is built");
        run("CREATE VIEW dbdelve_keyless_view AS SELECT c FROM dbdelve_keyless_probe")
            .expect("the view is built");
        let viewed = run("SELECT * FROM dbdelve_keyless_view").expect("the view reads");
        assert!(viewed.edit.is_none());
        assert_eq!(viewed.read_only_reason, None);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MYSQL_URL"]
    fn live_every_listed_type_matches_its_row_by_its_rendered_text_on_mysql() {
        live_every_listed_type_matches_its_row_by_its_rendered_text(Engine::MySql);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_MARIADB_URL"]
    fn live_every_listed_type_matches_its_row_by_its_rendered_text_on_mariadb() {
        live_every_listed_type_matches_its_row_by_its_rendered_text(Engine::MariaDb);
    }

    fn conformance_subject(engine: Engine) -> crate::db::conformance::Subject {
        use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
        Subject {
            connection: live_profile(engine),
            target: crate::db::Target::built_in(engine),
            schema: "dbdelve_dev",
            relation: "conformance",
            key: "id",
            columns: &COLUMNS,
            read: sql_read(engine, "dbdelve_dev"),
            endless_rows: None,
            endless: LIVE_ENDLESS_SELECT,
            runaway: LIVE_SLOW_SELECT,
            read_only_refusal: "READ ONLY",
            fixture: Some(Fixture {
                types: &[
                    "int",
                    "varchar",
                    "bigint",
                    "decimal",
                    "double",
                    "tinyint",
                    "date",
                    "datetime",
                    "varbinary",
                ],
                renders: &[],
                find: Box::new(move |key| sql_find(engine, "dbdelve_dev", key)),
            }),
            _guard: None,
        }
    }

    fn conformance_on_mysql() -> crate::db::conformance::Subject {
        conformance_subject(Engine::MySql)
    }

    fn conformance_on_mariadb() -> crate::db::conformance::Subject {
        conformance_subject(Engine::MariaDb)
    }

    crate::db::conformance::suite!(
        live_conformance_on_mysql,
        "requires a local MySQL server configured through dbdelve_MYSQL_URL",
        conformance_on_mysql:
        connects,
        query_round_trip,
        catalog_round_trip,
        structure_round_trip,
        types_render_as_expected,
        cancel_stops_a_long_query,
        row_limit_stops_an_eligible_read,
        row_limit_drains_an_ineligible_read,
        capped_read_keeps_the_users_transaction,
        read_only_session_refuses_a_write,
        generated_writes_round_trip,
        generated_draft_round_trip,
        verified_draft_round_trip,
        verified_keyless_round_trip,
    );

    crate::db::conformance::suite!(
        live_conformance_on_mariadb,
        "requires a local MariaDB server configured through dbdelve_MARIADB_URL",
        conformance_on_mariadb:
        connects,
        query_round_trip,
        catalog_round_trip,
        structure_round_trip,
        types_render_as_expected,
        cancel_stops_a_long_query,
        row_limit_stops_an_eligible_read,
        row_limit_drains_an_ineligible_read,
        capped_read_keeps_the_users_transaction,
        read_only_session_refuses_a_write,
        generated_writes_round_trip,
        generated_draft_round_trip,
        verified_draft_round_trip,
        verified_keyless_round_trip,
    );

    #[test]
    #[ignore = "requires a local MySQL server configured through dbdelve_MYSQL_URL"]
    fn live_binary_edits_round_trip_on_mysql() {
        crate::db::conformance::binary_edits_round_trip(
            &live_profile(Engine::MySql),
            crate::db::Target::built_in(Engine::MySql),
            "dbdelve_dev",
            "varbinary(16)",
        );
    }

    #[test]
    #[ignore = "requires a local MariaDB server configured through dbdelve_MARIADB_URL"]
    fn live_binary_edits_round_trip_on_mariadb() {
        crate::db::conformance::binary_edits_round_trip(
            &live_profile(Engine::MariaDb),
            crate::db::Target::built_in(Engine::MariaDb),
            "dbdelve_dev",
            "blob",
        );
    }

    on_both_servers!(
        live_an_apply_with_autocommit_off_is_refused_and_leaves_the_users_write_open,
        live_a_password_command_signs_in_the_session_and_its_cancel,
        live_reads_run_at_once_take_the_time_of_one,
        live_a_cancel_stops_every_read_run_at_once,
        live_a_row_limit_keeps_the_first_rows_and_counts_the_rest,
        live_a_fed_query_hands_its_typed_rows_to_the_feed,
        live_a_read_past_the_limit_is_stopped_rather_than_drained,
        live_a_read_inside_the_users_transaction_is_stopped_and_the_transaction_kept,
        live_a_killed_session_is_lost_and_a_failed_statement_is_not,
        live_a_cancel_stops_a_running_statement_without_closing_the_session,
        live_a_statement_timeout_bounds_a_select_and_a_write_only_on_mariadb,
        live_only_the_modes_that_tolerate_an_unchecked_certificate_connect,
        live_the_development_database_is_fully_seeded,
        live_the_databases_list_flags_the_one_connected_to,
        live_a_blank_database_connects_to_none,
        live_query_round_trip,
        live_a_write_reports_the_rows_it_affected,
        live_a_statements_warnings_arrive_as_messages,
        live_a_multi_statement_submission_counts_the_warnings_it_cannot_list,
        live_a_multi_statement_selection_keeps_one_result_shape,
        live_a_column_is_tagged_with_the_type_the_server_would_name,
        live_catalog_round_trip,
        live_the_selected_database_is_listed_even_when_the_server_owns_it,
        live_an_empty_database_is_listed,
        live_structure_round_trip,
        live_a_single_table_select_is_editable_by_its_primary_key,
        live_an_aliased_or_computed_column_reports_what_the_table_calls_it,
        live_a_join_an_aggregate_or_a_missing_key_is_not_editable,
        live_a_table_without_transactions_is_not_editable,
        live_an_apply_to_a_table_moved_off_innodb_is_refused_and_closed,
        live_a_keyless_edit_after_a_write_a_trigger_follows_is_refused,
        live_the_foreign_key_fixtures_are_seeded,
        live_a_composite_foreign_key_arrives_as_one_key_per_column_in_key_order,
        live_a_foreign_key_across_databases_names_the_database_it_references,
        live_a_table_referencing_nothing_reports_no_foreign_keys,
        live_the_rendered_foreign_key_ddl_survives_beside_the_structured_form,
        live_the_ddl_of_a_table_and_a_view_is_the_servers_own,
        live_a_schema_graph_reads_the_seeds_keys_back,
        live_a_unique_index_on_a_prefix_makes_its_column_unique,
        live_a_cancel_on_its_own_connection_stops_a_catalog_statement,
        live_a_cancel_before_a_schema_graph_read_drops_it_unsent,
    );
}

//! The Oracle boundary, on Oracle's own driver.
//!
//! `oracledb` speaks TNS in pure Rust, so there is no Instant Client to find,
//! ship or version, and it is blocking, like `mysql` and `rusqlite`: a query
//! runs on `workspace::spawn_blocking`, and nothing here is a future.
//!
//! The driver has no way to stop a call from another thread, so the crate is
//! vendored with an interrupt (`dbdelve patch` in `vendor/oracledb`). On a
//! plain TCP session it sends the marker the driver already sends when a call
//! timeout expires, and the session, with whatever transaction it holds,
//! survives. Over TLS a second writer would corrupt the record layer, so the
//! socket is shut down and the session is lost: [`Connection::cancel`] says
//! which it was.
//!
//! Oracle has no autocommit, and DBDelve does not add one: a `DML` the user
//! runs stays in their transaction until they run `COMMIT`, and the result
//! says so. A session dropped with work uncommitted rolls it back.
//!
//! The catalog is the `ALL_*` views, which are ordinary SQL, so the queries
//! below alias their columns, quoted to keep their case, to the names the
//! shared assemblers in `mod.rs` read. Their `ORACLE_MAINTAINED` column is
//! what tells a user's schema from the dictionary's, which needs 12.1.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oracledb::{ErrorKind, InterruptHandle, Interrupted, Metadata, Row as OracleRow};

use super::tunnel::{Tunnel, tunnelled};
use super::{
    Catalog, Column, DbError, Fetch, QueryResult, Row, RowBuilder, ServerConfig, Stopped,
    Structure, assemble_catalog, assemble_databases, assemble_structure, plain_error,
};
use crate::tls::SslMode;

pub const DEFAULT_PORT: u16 = 1521;

/// Without this a host that resolves but drops packets pins the UI in
/// "Connecting…" for minutes with no cancel, as it does on every other engine.
const CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// What Oracle answers a call with when it was stopped by an interrupt.
const CANCELLED: usize = 1013;

const DATABASES_SQL: &str = "
SELECT SYS_CONTEXT('USERENV', 'SERVICE_NAME') AS \"name\", 1 AS \"is_current\"
FROM DUAL";

// A materialized view's container table is listed as a table of its own, and a
// dropped table sits in the recycle bin under a `BIN$` name until it is purged.
const RELATIONS_SQL: &str = "
SELECT o.owner AS \"schema_name\",
       o.object_name AS \"relation_name\",
       CASE o.object_type
           WHEN 'TABLE' THEN 'table'
           WHEN 'VIEW' THEN 'view'
           WHEN 'MATERIALIZED VIEW' THEN 'materialized_view'
       END AS \"relation_kind\"
FROM all_objects o
JOIN all_users u ON u.username = o.owner
WHERE o.object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW')
  AND u.oracle_maintained = 'N'
  AND o.object_name NOT LIKE 'BIN$%'
  AND o.secondary = 'N'
  AND NOT (o.object_type = 'TABLE' AND EXISTS (
      SELECT 1 FROM all_mviews m
      WHERE m.owner = o.owner AND m.mview_name = o.object_name))
ORDER BY o.owner, o.object_name";

// The body is not read here: it is `ALL_SOURCE` rows, one a line, and the
// catalog is loaded every time a connection opens.
const ROUTINES_SQL: &str = "
SELECT o.owner AS \"schema_name\",
       o.object_name AS \"routine_name\",
       LOWER(o.object_type) AS \"routine_kind\",
       '' AS \"identity_arguments\",
       '' AS \"result_type\",
       'PL/SQL' AS \"language\",
       '' AS \"definition\"
FROM all_objects o
JOIN all_users u ON u.username = o.owner
WHERE o.object_type IN ('FUNCTION', 'PROCEDURE')
  AND u.oracle_maintained = 'N'
ORDER BY o.owner, o.object_name";

// `DATA_DEFAULT` is a `LONG`, which a `CASE` or a concatenation refuses, so a
// column's default is not read here; an identity or a virtual column says so
// instead, which is what the user needs to know before an insert.
const STRUCTURE_COLUMNS_SQL: &str = "
SELECT column_name AS \"column_name\",
       CASE
           WHEN data_type IN ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR')
               THEN data_type || '(' || char_length || ')'
           WHEN data_type = 'RAW' THEN 'RAW(' || data_length || ')'
           WHEN data_type = 'NUMBER' AND data_precision IS NOT NULL
               THEN 'NUMBER(' || data_precision
                    || CASE WHEN NVL(data_scale, 0) <> 0 THEN ',' || data_scale END || ')'
           ELSE data_type
       END AS \"data_type\",
       CASE nullable WHEN 'Y' THEN 'yes' ELSE 'no' END AS \"nullable\",
       CASE
           WHEN identity_column = 'YES' THEN 'GENERATED AS IDENTITY'
           WHEN virtual_column = 'YES' THEN 'GENERATED ALWAYS AS VIRTUAL'
           ELSE ''
       END AS \"column_default\"
FROM all_tab_cols
WHERE owner = {schema}
  AND table_name = {relation}
  AND hidden_column = 'NO'
ORDER BY column_id";

const STRUCTURE_INDEXES_SQL: &str = "
SELECT i.index_name AS \"object_name\",
       CASE
           WHEN EXISTS (SELECT 1 FROM all_constraints c
                        WHERE c.owner = i.table_owner AND c.table_name = i.table_name
                          AND c.constraint_type = 'P' AND c.index_name = i.index_name)
               THEN 'primary'
           WHEN i.uniqueness = 'UNIQUE' THEN 'unique'
           ELSE 'plain'
       END AS \"index_kind\",
       (SELECT LISTAGG('\"' || ic.column_name || '\"', CHR(31))
               WITHIN GROUP (ORDER BY ic.column_position)
        FROM all_ind_columns ic
        WHERE ic.index_owner = i.owner AND ic.index_name = i.index_name) AS \"index_columns\",
       'CREATE ' || CASE WHEN i.uniqueness = 'UNIQUE' THEN 'UNIQUE ' END
           || 'INDEX \"' || i.owner || '\".\"' || i.index_name || '\" ON \"'
           || i.table_owner || '\".\"' || i.table_name || '\" ('
           || (SELECT LISTAGG('\"' || ic.column_name || '\"', ', ')
                      WITHIN GROUP (ORDER BY ic.column_position)
               FROM all_ind_columns ic
               WHERE ic.index_owner = i.owner AND ic.index_name = i.index_name)
           || ')' AS \"definition\"
FROM all_indexes i
WHERE i.table_owner = {schema}
  AND i.table_name = {relation}
ORDER BY i.index_name";

// Check constraints are left out: a `NOT NULL` is one, and `SEARCH_CONDITION`
// is a `LONG`, so they would be noise that cannot be read here anyway.
const STRUCTURE_CONSTRAINTS_SQL: &str = "
SELECT c.constraint_name AS \"object_name\",
       CASE c.constraint_type WHEN 'P' THEN 'PRIMARY KEY' WHEN 'U' THEN 'UNIQUE' ELSE 'FOREIGN KEY' END
           || ' (' || (SELECT LISTAGG('\"' || cc.column_name || '\"', ', ')
                              WITHIN GROUP (ORDER BY cc.position)
                       FROM all_cons_columns cc
                       WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name) || ')'
           || CASE WHEN c.constraint_type = 'R' THEN
                  (SELECT ' REFERENCES \"' || r.owner || '\".\"' || r.table_name || '\"'
                   FROM all_constraints r
                   WHERE r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name)
              END AS \"definition\"
FROM all_constraints c
WHERE c.owner = {schema}
  AND c.table_name = {relation}
  AND c.constraint_type IN ('P', 'U', 'R')
ORDER BY c.constraint_type, c.constraint_name";

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise, as
/// they do on every other engine here. Revisit only if concurrent statements
/// per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    connection: Arc<Mutex<oracledb::Connection>>,
    /// Taken in `open`, between calls, because the connection is locked for as
    /// long as one runs and a Cancel has to reach it then.
    interrupt: Arc<InterruptHandle>,
    /// Set once a Cancel had to shut the socket down.
    lost: Arc<AtomicBool>,
    server: ServerConfig,
    /// Held so ssh runs as long as any clone does.
    tunnel: Option<Arc<Tunnel>>,
}

impl Connection {
    pub fn open(server: &ServerConfig) -> Result<Self, DbError> {
        // Before the tunnel, which may have cost a hardware-key touch.
        if server.has_tunnel()
            && matches!(
                server.sslmode,
                SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
            )
        {
            return Err(unreachable_through_a_tunnel(server));
        }
        tunnelled(server, DEFAULT_PORT, |tunnel| {
            let dial = tunnel.as_deref().map(Tunnel::dial).transpose()?;
            let (connection, interrupt) = connect(server, dial.map(|dial| dial.to_string()))?;
            Ok(Self {
                connection: Arc::new(Mutex::new(connection)),
                interrupt: Arc::new(interrupt),
                lost: Arc::default(),
                server: server.clone(),
                tunnel,
            })
        })
    }

    /// A second session, for a statement to run on beside this one's.
    pub fn alongside(&self) -> Result<Self, DbError> {
        let dial = self.tunnel.as_deref().map(Tunnel::dial).transpose()?;
        let (connection, interrupt) = connect(&self.server, dial.map(|dial| dial.to_string()))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            interrupt: Arc::new(interrupt),
            lost: Arc::default(),
            ..self.clone()
        })
    }

    /// Whether the session behind a failed statement is gone: a Cancel shut its
    /// socket, or the driver says the line is dead.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
            || self
                .connection
                .lock()
                .map_or(true, |connection| connection.ping().is_err())
    }

    /// Stops the call in flight, from a thread of its own: the one running it
    /// holds the connection. Whether the session survives depends on the
    /// transport, and an engine tells the user so through [`Self::is_lost`].
    pub fn cancel(&self) -> Result<(), DbError> {
        match self.interrupt.interrupt() {
            Ok(Interrupted::Call) => Ok(()),
            Ok(Interrupted::Session) => {
                self.lost.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => Err(DbError {
                message: format!("Could not ask the server to cancel: {error}"),
                position: None,
            }),
        }
    }

    /// Run one statement verbatim, keeping what `fetch` asks for of its rows;
    /// see [`super::Connection::query`].
    ///
    /// The SQL is never rewritten. The splitter that cuts a buffer into
    /// statements leaves each without its `;`, which a SQL statement must not
    /// have here and a PL/SQL block must, so a block gets back the one it was
    /// cut from.
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        let connection = self.connection.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        let held = fetch.hold()?;
        let result = self.run(&connection, sql, fetch);
        drop(held);
        result
    }

    fn run(
        &self,
        connection: &oracledb::Connection,
        sql: &str,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let mut statement = connection
            .statement(sql)
            .and_then(oracledb::StatementBuilder::build)
            .map_err(|error| self.failed(&error))?;
        // `is_plsql` is read off the text, before anything is sent.
        let block;
        if statement.is_plsql() && !sql.trim_end().ends_with(';') {
            block = format!("{sql};");
            statement = connection
                .statement(&block)
                .and_then(oracledb::StatementBuilder::build)
                .map_err(|error| self.failed(&error))?;
        }

        let mut result = QueryResult::default();
        if !statement.is_query() {
            let dml = statement.is_dml() || statement.is_dml_returning();
            let ran = statement
                .execute(&[])
                .map_err(|error| self.failed(&error))?;
            result.rows_affected = Some(ran.rows_affected());
            if dml {
                result.notice =
                    Some("Not committed: run COMMIT to keep it, or ROLLBACK to undo it.".into());
            }
            result.elapsed = started.elapsed();
            return Ok(result);
        }

        let cursor = statement.query(&[]).map_err(|error| self.failed(&error))?;
        let described: Vec<Metadata> = cursor.columns().to_vec();
        let columns: Vec<Column> = described
            .iter()
            .map(|column| Column {
                name: column.name().to_string(),
                data_type: Some(type_name(column)),
            })
            .collect();
        if let Some(feed) = fetch.feed {
            feed.begin(columns.clone());
        }

        let mut builder = RowBuilder::default();
        let mut rows = Vec::new();
        let mut bytes = 0;
        // This set's rows, kept or not, and kept: a fed row is not in `rows`.
        let (mut returned, mut kept) = (0usize, 0usize);
        let mut filled = None;
        for row in cursor {
            let row = row.map_err(|error| self.failed(&error))?;
            returned += 1;
            if fetch.limit.is_some_and(|limit| kept >= limit) {
                // One past the limit proves there were more. A statement that
                // only reads loses nothing by being left there; anything else
                // is drained, because its rows are not all it does.
                if fetch.reads_only {
                    result.stopped = Some(Stopped::AtLimit);
                    break;
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
                builder.push(render(&row, index, column).as_deref());
            }
            let cells = builder.finish()?;
            bytes += cells.text_len();
            match fetch.feed {
                Some(feed) => feed.push(cells),
                None => rows.push(cells),
            }
        }

        result.rows_affected = Some(returned as u64);
        result.capped_from = (result.stopped.is_none() && returned > kept).then_some(returned);
        result.columns = columns;
        result.rows = rows;
        result.bytes = bytes;
        // Timed to the last kept row when rows past the limit came after it:
        // draining the statement is not the wait for these.
        result.elapsed = match filled {
            Some(filled) if returned > kept => filled,
            _ => started.elapsed(),
        };
        Ok(result)
    }

    /// A statement the catalog runs, with no feed and no limit.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.query(sql, Fetch::default())
    }

    pub fn databases(&self) -> Result<super::Databases, DbError> {
        assemble_databases(&self.internal_query(DATABASES_SQL)?)
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        assemble_catalog(self.internal_query(RELATIONS_SQL)?, QueryResult::default())
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        let routines = emptied(
            self.internal_query(ROUTINES_SQL)?,
            &["identity_arguments", "result_type", "definition"],
        )?;
        assemble_catalog(QueryResult::default(), routines)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let ask = |sql: &str| self.internal_query(&structure_sql(sql, schema, relation));
        assemble_structure(
            emptied(ask(STRUCTURE_COLUMNS_SQL)?, &["column_default"])?,
            ask(STRUCTURE_INDEXES_SQL)?,
            ask(STRUCTURE_CONSTRAINTS_SQL)?,
        )
    }

    /// `DBMS_METADATA`'s text is a `CLOB`, read a slice at a time because a
    /// SQL `VARCHAR2` holds 4000 bytes and a slice of 1000 characters is that
    /// at most.
    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        const SLICE: usize = 1000;
        let mut ddl = String::new();
        loop {
            let sql = format!(
                "SELECT DBMS_LOB.SUBSTR(DBMS_METADATA.GET_DDL('TABLE', {}, {}), {SLICE}, {}) AS \"ddl\" FROM DUAL",
                literal(relation),
                literal(schema),
                ddl.chars().count() + 1,
            );
            let result = self.internal_query(&sql)?;
            let Some(slice) = result.rows.first().and_then(|row| row.get(0)) else {
                break;
            };
            if slice.is_empty() {
                break;
            }
            ddl.push_str(slice);
        }
        Ok(ddl.trim().to_string())
    }

    /// A failed call as the user is told of it. A dead line is remembered, so
    /// [`Self::is_lost`] needs no round trip to say so.
    fn failed(&self, error: &oracledb::Error) -> DbError {
        if matches!(error.kind(), ErrorKind::DeadConnection) {
            self.lost.store(true, Ordering::Relaxed);
        }
        query_error(error)
    }
}

/// Oracle reads `''` as NULL, so a catalog column meant to be empty arrives as
/// one, and the shared assemblers refuse a NULL there. The named columns get
/// the empty string back.
fn emptied(mut result: QueryResult, columns: &[&str]) -> Result<QueryResult, DbError> {
    let wanted: Vec<usize> = columns
        .iter()
        .filter_map(|name| {
            result
                .columns
                .iter()
                .position(|column| column.name == *name)
        })
        .collect();
    result.rows = result
        .rows
        .iter()
        .map(|row| {
            Row::new(
                row.iter()
                    .enumerate()
                    .map(|(index, cell)| match cell {
                        Some(text) => Some(text.to_string()),
                        None if wanted.contains(&index) => Some(String::new()),
                        None => None,
                    })
                    .collect::<Vec<super::Cell>>(),
            )
        })
        .collect::<Result<_, _>>()?;
    Ok(result)
}

/// `STRUCTURE_*_SQL`'s placeholders, filled with the quoted names.
fn structure_sql(template: &str, schema: &str, relation: &str) -> String {
    template
        .replace("{schema}", &literal(schema))
        .replace("{relation}", &literal(relation))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn query_error(error: &oracledb::Error) -> DbError {
    DbError {
        message: error.to_string(),
        position: None,
    }
}

/// Whether a failure is the interrupt a Cancel sent, which the caller shows as
/// a cancellation rather than as an error.
pub fn is_cancel(message: &str) -> bool {
    message.contains(&format!("ORA-{CANCELLED:05}"))
}

fn unreachable_through_a_tunnel(server: &ServerConfig) -> DbError {
    plain_error(format!(
        "sslmode={} cannot be honoured through an {} tunnel on Oracle: the driver checks \
         the certificate against the address it dials, which is the tunnel's loopback \
         address rather than {}.",
        server.sslmode.as_str(),
        if server.ssm.is_some() { "SSM" } else { "SSH" },
        server.host
    ))
}

/// The type as the structure view and the grid header name it: `NUMBER(10,2)`,
/// `VARCHAR2(40)`, `TIMESTAMP(6)`.
fn type_name(column: &Metadata) -> String {
    column.data_type()
}

/// One cell as text, `None` for a NULL. Chosen by the column's type rather than
/// tried in turn, so a date does not cost a failed conversion on every row.
/// Oracle's `''` is a NULL, so it reads as one here too.
fn render(row: &OracleRow, index: usize, column: &Metadata) -> Option<String> {
    let db_type = column.db_type();
    let text = |row: &OracleRow| row.get::<Option<String>>(index);
    let rendered = if db_type.is_binary_type() {
        row.get::<Option<Vec<u8>>>(index)
            .map(|bytes| bytes.map(|bytes| format!("0x{}", hex::encode_upper(bytes))))
    } else if db_type.is_date_type() {
        row.get::<Option<oracledb::OracleTimestamp>>(index)
            .map(|value| value.map(|value| value.to_string()))
    } else if db_type.is_string_type() {
        text(row)
    } else {
        let name = column.data_type();
        if name.starts_with("INTERVAL DAY") {
            row.get::<Option<oracledb::OracleIntervalDS>>(index)
                .map(|value| value.map(|value| value.to_string()))
        } else if name.starts_with("INTERVAL YEAR") {
            row.get::<Option<oracledb::OracleIntervalYM>>(index)
                .map(|value| value.map(|value| value.to_string()))
        } else if name == "BOOLEAN" {
            row.get::<Option<bool>>(index)
                .map(|value| value.map(|value| value.to_string()))
        } else if name.starts_with("BINARY_") {
            row.get::<Option<f64>>(index)
                .map(|value| value.map(|value| value.to_string()))
        } else {
            text(row)
        }
    };
    match rendered {
        Ok(value) => value.filter(|value| !value.is_empty()),
        // A type this does not read yet (JSON, VECTOR, an object) is named
        // rather than guessed at or failed on: the rest of the row is fine.
        Err(_) => Some(format!("<{}>", column.data_type())),
    }
}

/// Opens one session. `dial` is the address a tunnel listens on, in the host's
/// place.
///
/// Which `sslmode` gets what, and why none is quietly weakened: the driver
/// verifies a certificate whenever it speaks TLS, so `require`, `verify-ca` and
/// `verify-full` all ask for more than libpq's `require` does and never less.
/// `prefer` is honoured as libpq honours it, a TLS attempt and then plaintext,
/// but only when the TLS attempt failed below the login: a refused password is
/// not sent a second time, which would count twice against a lockout. `disable`
/// is plaintext only.
fn connect(
    server: &ServerConfig,
    dial: Option<String>,
) -> Result<(oracledb::Connection, InterruptHandle), DbError> {
    if server.database.trim().is_empty() {
        return Err(plain_error(
            "Oracle connects to a service: put its name in the Database field.".into(),
        ));
    }
    let (host, port) = match &dial {
        Some(dial) => dial
            .rsplit_once(':')
            .map(|(host, port)| (host.to_string(), port.to_string()))
            .ok_or_else(|| plain_error("The tunnel gave no port to dial.".into()))?,
        None => (
            server.host.clone(),
            server.port.unwrap_or(DEFAULT_PORT).to_string(),
        ),
    };
    let attempt = |tls: bool| -> Result<(oracledb::Connection, InterruptHandle), oracledb::Error> {
        let mut config = oracledb::Config::default()
            .set_credentials(&server.user, &server.password)
            .set_connect_string(&format!(
                "{}://{host}:{port}/{}",
                if tls { "tcps" } else { "tcp" },
                server.database
            ))?
            .set_transport_connect_timeout(Some(Duration::from_secs(CONNECT_TIMEOUT_SECONDS)));
        // The wallet directory, where the driver reads the trust anchors and
        // client certificate (`ewallet.pem`) a TCPS connection is verified by.
        if let Some(wallet) = server.root_certificate.as_deref().filter(|_| tls) {
            config = config.set_wallet_location(wallet);
        }
        let connection = oracledb::connect(config)?;
        if server.statement_timeout > 0 {
            connection.set_call_timeout(Some(Duration::from_secs(u64::from(
                server.statement_timeout,
            ))))?;
        }
        let interrupt = connection.interrupt_handle()?;
        Ok((connection, interrupt))
    };
    match server.sslmode {
        SslMode::Disable => attempt(false).map_err(|error| query_error(&error)),
        SslMode::Prefer => match attempt(true) {
            Ok(opened) => Ok(opened),
            Err(error) if error.db_error_code().is_some() => Err(query_error(&error)),
            Err(_) => attempt(false).map_err(|error| query_error(&error)),
        },
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            attempt(true).map_err(|error| query_error(&error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_names_literal_doubles_its_quotes() {
        assert_eq!(literal("O'NEIL"), "'O''NEIL'");
    }

    #[test]
    fn structure_queries_are_filled_with_literals() {
        let sql = structure_sql(
            "WHERE owner = {schema} AND table_name = {relation}",
            "HR",
            "EMP",
        );
        assert_eq!(sql, "WHERE owner = 'HR' AND table_name = 'EMP'");
    }

    /// The server the `live_` tests talk to, from `dbdelve_ORACLE_URL`.
    fn live() -> Connection {
        let url = std::env::var("dbdelve_ORACLE_URL").expect("dbdelve_ORACLE_URL is required");
        let server = crate::db::server_from_url(&url, "Oracle").expect("dbdelve_ORACLE_URL parses");
        Connection::open(&server).expect("connection should open")
    }

    fn cell(result: &QueryResult, row: usize, column: &str) -> Option<String> {
        let index = result.columns.iter().position(|c| c.name == column)?;
        result.rows[row].get(index).map(str::to_string)
    }

    #[test]
    #[ignore]
    fn live_a_query_returns_its_rows_and_types() {
        let result = live()
            .query(
                "SELECT id, name, email, balance FROM accounts ORDER BY id",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(cell(&result, 0, "NAME").as_deref(), Some("Ada Lovelace"));
        // A NULL stays distinguishable from an empty string.
        assert_eq!(cell(&result, 2, "EMAIL"), None);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.data_type.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("NUMBER(10)"),
                Some("VARCHAR2(60)"),
                Some("VARCHAR2(120)"),
                Some("NUMBER(12,2)")
            ]
        );
    }

    #[test]
    #[ignore]
    fn live_every_listed_type_renders() {
        let result = live()
            .query("SELECT * FROM type_gallery ORDER BY id", Fetch::default())
            .unwrap();
        let first = |name: &str| cell(&result, 0, name);
        assert_eq!(first("C_CHAR").as_deref(), Some("ab  "));
        assert_eq!(first("C_NUMBER").as_deref(), Some("3.14159"));
        assert_eq!(first("C_DATE").as_deref(), Some("2024-05-06T00:00:00"));
        assert_eq!(
            first("C_TS").as_deref(),
            Some("2024-05-06T07:08:09.123000000")
        );
        assert_eq!(first("C_RAW").as_deref(), Some("0xDEADBEEF"));
        assert_eq!(first("C_CLOB").as_deref(), Some("a long text"));
        assert_eq!(first("C_BLOB").as_deref(), Some("0x626C6F62"));
        // The second row holds only its key.
        assert_eq!(cell(&result, 1, "C_VARCHAR"), None);
    }

    #[test]
    #[ignore]
    fn live_the_row_limit_keeps_what_it_says_and_names_the_rest() {
        let connection = live();
        let capped = connection
            .query(
                "SELECT id FROM accounts",
                Fetch {
                    limit: Some(2),
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!((capped.rows.len(), capped.capped_from), (2, Some(3)));
        let stopped = connection
            .query(
                "SELECT id FROM accounts",
                Fetch {
                    limit: Some(2),
                    reads_only: true,
                    ..Fetch::default()
                },
            )
            .unwrap();
        assert_eq!(
            (stopped.rows.len(), stopped.stopped),
            (2, Some(Stopped::AtLimit))
        );
    }

    #[test]
    #[ignore]
    fn live_the_catalog_lists_the_seeded_relations_and_no_dictionary() {
        let catalog = live().catalog().unwrap();
        let names: Vec<(&str, &str)> = catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema
                    .relations
                    .iter()
                    .map(|r| (schema.name.as_str(), r.name.as_str()))
            })
            .collect();
        assert!(names.contains(&("DBDELVE", "ACCOUNTS")), "{names:?}");
        assert!(names.contains(&("DBDELVE", "RICH_ACCOUNTS")), "{names:?}");
        assert!(
            names.iter().all(|(schema, _)| *schema != "SYS"),
            "{names:?}"
        );
    }

    #[test]
    #[ignore]
    fn live_structure_names_the_key_the_index_and_the_constraints() {
        let structure = live().structure("DBDELVE", "ACCOUNTS").unwrap();
        assert_eq!(structure.columns.len(), 5);
        assert_eq!(
            structure.columns[0].default.as_deref(),
            Some("GENERATED AS IDENTITY")
        );
        assert!(
            structure
                .indexes
                .iter()
                .any(|i| i.kind == crate::db::IndexKind::Primary)
        );
        assert!(
            structure
                .indexes
                .iter()
                .any(|i| i.name == "ACCOUNTS_EMAIL_UX")
        );
        let orders = live().structure("DBDELVE", "ORDERS").unwrap();
        assert!(
            orders
                .constraints
                .iter()
                .any(|c| c.definition.contains("REFERENCES"))
        );
    }

    #[test]
    #[ignore]
    fn live_ddl_reads_back_in_full() {
        let ddl = live().ddl("DBDELVE", "ACCOUNTS").unwrap();
        assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
        assert!(ddl.contains("\"BALANCE\""), "{ddl}");
    }

    /// Reads, never writes, so it can be pointed at a database that is not the
    /// seeded one: whatever the catalog lists has to describe and render.
    #[test]
    #[ignore]
    fn live_every_listed_relation_can_be_described_and_read() {
        let connection = live();
        let catalog = connection.catalog().unwrap();
        let mut checked = 0;
        for schema in &catalog.schemas {
            for relation in schema.relations.iter().take(5) {
                let structure = connection
                    .structure(&schema.name, &relation.name)
                    .unwrap_or_else(|e| panic!("{}.{}: {e}", schema.name, relation.name));
                let sql = format!(
                    "SELECT * FROM \"{}\".\"{}\"",
                    schema.name.replace('"', "\"\""),
                    relation.name.replace('"', "\"\"")
                );
                let result = connection
                    .query(
                        &sql,
                        Fetch {
                            limit: Some(5),
                            reads_only: true,
                            ..Fetch::default()
                        },
                    )
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
                // What the structure says of a table is what a read of it returns.
                assert_eq!(structure.columns.len(), result.columns.len(), "{sql}");
                checked += 1;
            }
        }
        assert!(checked > 0, "the catalog listed nothing to check");
        println!("checked {checked} relations");
        let routines = connection.routines().unwrap();
        println!("routines in {} schemas", routines.schemas.len());
        let databases = connection.databases().unwrap();
        assert_eq!(databases.names.len(), 1, "{databases:?}");
        let table = catalog
            .schemas
            .iter()
            .flat_map(|schema| schema.relations.iter().map(move |r| (schema, r)))
            .find(|(_, relation)| relation.kind == crate::db::RelationKind::Table)
            .expect("a table to read the DDL of");
        let ddl = connection.ddl(&table.0.name, &table.1.name).unwrap();
        assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
    }

    #[test]
    #[ignore]
    fn live_dml_says_it_is_not_committed() {
        let connection = live();
        let result = connection
            .query(
                "UPDATE accounts SET balance = balance WHERE id = 1",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(result.rows_affected, Some(1));
        assert!(result.notice.is_some());
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    #[test]
    #[ignore]
    fn live_a_plsql_block_runs_without_its_terminator() {
        // The statement splitter drops the final `;`; the block needs it back.
        live()
            .query("BEGIN NULL; END", Fetch::default())
            .expect("a block cut from its terminator runs");
    }

    #[test]
    #[ignore]
    fn live_a_cancel_stops_the_call_and_keeps_the_session_in_the_clear() {
        let connection = live();
        let stopper = connection.clone();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            stopper.cancel().unwrap();
        });
        let error = connection
            .query("BEGIN DBMS_SESSION.SLEEP(30); END", Fetch::default())
            .unwrap_err();
        thread.join().unwrap();
        assert!(is_cancel(&error.message), "{}", error.message);
        assert!(started.elapsed() < Duration::from_secs(10));
        // Over plain TCP the session survives. Over TLS the socket was shut,
        // and the connection says so.
        if !connection.is_lost() {
            let again = connection
                .query("SELECT 1 AS one FROM DUAL", Fetch::default())
                .unwrap();
            assert_eq!(cell(&again, 0, "ONE").as_deref(), Some("1"));
        }
    }

    #[test]
    fn an_interrupted_call_is_told_from_any_other_error() {
        assert!(is_cancel(
            "ORA-01013: user requested cancel of current operation"
        ));
        assert!(!is_cancel("ORA-00942: table or view does not exist"));
    }
}

//! The SQLite boundary.
//!
//! The mirror image of `postgres.rs` in three ways, and each one is a place
//! this file has to do work that one does not. There is no server, so there are
//! no credentials and no TLS. There is no wire protocol formatting values on
//! the way out, so every cell is rendered here rather than arriving as text.
//! And there is no catalog, so every question about a table's shape is a
//! `PRAGMA` — reached through SQLite's table-valued pragma functions, which is
//! what lets the answers come back as ordinary result sets the shared
//! assemblers in `mod.rs` already know how to read.
//!
//! What SQLite gives back for free is the one thing Postgres charges a round
//! trip for. `columns_with_metadata` says which table and which column every
//! result column was read from, which is exactly what in-grid editing needs, so
//! there is no describe step and nothing here can disturb an open transaction.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, InterruptHandle, OpenFlags};
use sqlparser::ast::{ColumnOption, GeneratedExpressionMode, Statement};
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

use super::{
    ApplyOutcome, ApplyStage, Catalog, Column, ColumnDefinition, DbError, EditTarget, Engine,
    Fetch, ForeignKey, Index, IndexKind, NamedDefinition, QueryResult, Reference, RowBuilder,
    RowMatch, SchemaGraph, Sending, Stopped, Structure, VerifiedStep, assemble_catalog,
    assemble_references, assemble_schema_graph, assemble_structure, non_utf8_error, optional_cell,
    percent_decoded, plain_error, required_cell,
};

/// The path out of a `sqlite:` or `file:` URL.
///
/// Everything after the scheme is the path, less an optional `//`, so
/// `sqlite:///tmp/a.db` and `sqlite:/tmp/a.db` both name `/tmp/a.db` and
/// `sqlite:./dev/dbdelve_dev.db` names a relative one. No query string is
/// honoured: SQLite's own URI parameters would be a second way to say things
/// the profile already says, and one of them is `mode=rwc`, which would put
/// back the file creation [`Connection::open`] deliberately refuses.
pub fn path_from_url(url: &str) -> Result<String, String> {
    let path = url_path(url)?;
    if path.is_empty() {
        return Err("Connection URL does not name a database file.".into());
    }
    Ok(path)
}

/// [`path_from_url`] less its refusal of a URL with nothing after the scheme,
/// which DuckDB reads as a database in memory.
pub(super) fn url_path(url: &str) -> Result<String, String> {
    let rest = url
        .split_once(':')
        .map(|(_, rest)| rest)
        .ok_or_else(|| "Connection URL does not name a database file.".to_string())?;
    let path = rest.strip_prefix("//").unwrap_or(rest);
    let mut decoded = percent_decoded(path)?;
    // `sqlite:///C:/a.db` is the standard spelling of a Windows path, and the
    // slash before the drive is the URL's, not the path's. Stripped on every
    // platform: a Unix path starting `/C:/` is not one anyone means.
    if let [b'/', drive, b':', ..] = decoded.as_bytes()
        && drive.is_ascii_alphabetic()
    {
        decoded.remove(0);
    }
    Ok(decoded)
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise. A
/// profile runs one query at a time by design; revisit only if concurrent
/// statements per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    connection: Arc<Mutex<rusqlite::Connection>>,
    /// Taken in `open`, off the connection, before it goes behind the mutex —
    /// see [`super::Connection::cancel`]. `sqlite3_interrupt` is documented
    /// safe to call from another thread, which is why `SQLITE_OPEN_NO_MUTEX`
    /// below is no obstacle to it and stays.
    interrupt: Arc<InterruptHandle>,
    /// The profile's statement timeout. SQLite has no such setting, so this is
    /// a wall-clock timer firing the same interrupt — see `run`.
    statement_timeout: Option<Duration>,
    /// The file, for [`Self::alongside`] to open again; `None` in memory.
    path: Option<String>,
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let path = crate::store::home_expanded(path);
        if !path.exists() {
            return Err(plain_error(format!(
                "No database file at {}",
                path.display()
            )));
        }

        Ok(Self::wrap(open_file(&path)?, statement_timeout))
    }

    fn wrap(connection: rusqlite::Connection, statement_timeout: u32) -> Self {
        Self {
            interrupt: Arc::new(connection.get_interrupt_handle()),
            statement_timeout: (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout))),
            path: connection
                .path()
                .filter(|path| !path.is_empty())
                .map(str::to_string),
            connection: Arc::new(Mutex::new(connection)),
        }
    }

    /// A second connection to the same file, for a statement to run on beside
    /// this one's; see [`super::Connection::query_alongside`].
    pub fn alongside(&self) -> Result<Self, DbError> {
        let path = self.path.as_deref().ok_or_else(|| {
            plain_error("An in-memory database cannot be opened a second time.".into())
        })?;
        let connection = open_file(std::path::Path::new(path))?;
        Ok(Self {
            interrupt: Arc::new(connection.get_interrupt_handle()),
            connection: Arc::new(Mutex::new(connection)),
            ..self.clone()
        })
    }

    /// Interrupting is local and immediate: there is no server to ask, so
    /// unlike the two client-server engines this cannot fail and cannot race
    /// with a statement that already finished — `sqlite3_interrupt` on an idle
    /// connection does nothing.
    pub fn cancel(&self) -> Result<(), DbError> {
        self.interrupt.interrupt();
        Ok(())
    }

    /// Arms the statement timeout, if the profile has one, until the returned
    /// sender is dropped.
    ///
    /// A thread per statement, parked on a channel that never receives: the
    /// timer's only job is to outlive the statement or be dropped by it, and
    /// `recv_timeout` is the stdlib's way to wait for exactly that without
    /// leaving a thread sleeping out the full timeout after a fast query.
    ///
    /// It measures wall clock, not work done, because that is all a timer
    /// outside SQLite can see: a statement parked on a locked database is
    /// stopped as readily as one scanning a table.
    fn deadline(&self) -> Option<Sender<()>> {
        let limit = self.statement_timeout?;
        let interrupt = self.interrupt.clone();
        let (sender, receiver) = channel::<()>();
        std::thread::spawn(move || {
            if receiver.recv_timeout(limit) == Err(RecvTimeoutError::Timeout) {
                interrupt.interrupt();
            }
        });
        Some(sender)
    }

    /// Run one statement verbatim, keeping what `fetch` asks for of its rows;
    /// see [`super::Connection::query`].
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. The
    /// rows are stepped one at a time, so one past the limit is dropped as
    /// soon as it is counted.
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, true, fetch)
    }

    /// dbdelve's own SQL. Its rows are never editable, so it does not pay for the
    /// round trip that reads a primary key.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, false, Fetch::default())
    }

    /// Applies `steps` as one verified sequence on this connection, which it
    /// holds for the whole of it ([`super::run_verified`]): there is one
    /// writer on a SQLite file, so nothing of this session's lands between the
    /// re-reads and the writes. A count is `changes()`, which excludes what a
    /// trigger or a foreign key wrote. The statement timeout is armed once for
    /// the sequence. `proceed` says no once a Cancel has come.
    pub fn apply_verified(
        &self,
        steps: &[VerifiedStep],
        proceed: &dyn Fn() -> bool,
    ) -> ApplyOutcome {
        let _deadline = self.deadline();
        let Ok(connection) = self.connection.lock() else {
            return ApplyOutcome::Failed {
                stage: ApplyStage::Begin,
                error: plain_error(
                    "The connection is unavailable after an earlier internal failure.".into(),
                ),
            };
        };
        let mut send = |sql: &str, sending: Sending| {
            // A transaction of the user's, open on this connection: the
            // apply's `BEGIN` would fail inside it and its `ROLLBACK` end it.
            if sending == Sending::Open && !connection.is_autocommit() {
                return Err(super::in_a_transaction());
            }
            ran(&connection, sql, Fetch::default()).map(|(result, _)| result)
        };
        // A SQLite file has no socket to lose, and renders every cell the same
        // way on both reads, so nothing is skipped and no `COMMIT` error means
        // a lost session.
        super::run_verified(
            Engine::Sqlite,
            steps,
            &mut send,
            proceed,
            &|_| false,
            &|_| false,
        )
    }

    fn run(&self, sql: &str, editable: bool, fetch: Fetch) -> Result<QueryResult, DbError> {
        // Armed before the lock rather than after it, so time spent queued
        // behind another statement on this connection counts against the limit
        // too. Dropped at the end of this call, whichever way it leaves.
        let _deadline = self.deadline();
        let connection = self.connection.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        let held = fetch.hold()?;

        let (mut result, probed) = ran(&connection, sql, fetch)?;

        // The mutex is not reentrant and `edit_target` runs a pragma through it,
        // so the lock has to go before the question is asked.
        drop(held);
        drop(connection);
        if editable {
            (result.edit, result.read_only_reason) = self.edit_target(sql, &probed);
        }

        Ok(result)
    }

    /// Which table these columns can be written back to, if any, and when the
    /// answer is no for a reason the user can act on, that reason.
    ///
    /// Every step is allowed to answer "no": a join, a computed column, a table
    /// without a primary key whose values cannot all be matched, a key the
    /// select omitted. A failure answers "no" too — this runs after the user's
    /// statement already succeeded, and a pragma that refused must not turn
    /// that success into an error.
    fn edit_target(
        &self,
        sql: &str,
        probed: &[ProbedColumn],
    ) -> (Option<EditTarget>, Option<String>) {
        let Some((schema, table)) = sole_table(probed) else {
            return (None, None);
        };
        // ponytail: one pragma per result set, no cache. A map from table to key
        // held on the connection is the upgrade path if the trip shows up in
        // query timings.
        let Ok(key) = self.schema().primary_key(&schema, &table) else {
            return (None, None);
        };
        if !key.is_empty() {
            return (resolve_edit_target(probed, &schema, &table, &key), None);
        }
        // Provenance names a column's table, never which instance of it: the
        // two sides of a self-join each read `t`, and every-column matching
        // would then name a row that is neither. Only a plain read of one
        // table is edited by its values.
        if let Err(why) = crate::sql::table_read_or_why(Engine::Sqlite, sql) {
            return (None, Some(why.reason().into()));
        }
        match self.schema().table_columns(&schema, &table) {
            Ok(columns) => resolve_keyless_target(probed, &schema, &table, &columns),
            Err(_) => (None, None),
        }
    }

    fn schema(&self) -> Schema<impl Fn(&str) -> Result<QueryResult, DbError> + '_> {
        Schema {
            run: |sql: &str| self.internal_query(sql),
            engine: Engine::Sqlite,
        }
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        self.schema().catalog()
    }

    pub fn routines(&self) -> Result<Catalog, DbError> {
        self.schema().routines()
    }

    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        self.schema().references(schema, relation)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        self.schema().structure(schema, relation)
    }

    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        self.schema().ddl(schema, relation)
    }

    pub fn schema_graph(&self, schema: &str) -> Result<SchemaGraph, DbError> {
        self.schema().schema_graph(schema)
    }
}

pub(super) struct Schema<F> {
    pub(super) run: F,
    pub(super) engine: Engine,
}

impl<F: Fn(&str) -> Result<QueryResult, DbError>> Schema<F> {
    fn argument(&self, schema: &str) -> String {
        match self.engine {
            Engine::Sqlite => format!(", {}", self.engine.quote_literal(schema)),
            Engine::Turso => String::new(),
            Engine::Postgres
            | Engine::MySql
            | Engine::MariaDb
            | Engine::DuckDb
            | Engine::SqlServer
            | Engine::Snowflake
            | Engine::ClickHouse
            | Engine::MongoDb
            | Engine::Redis
            | Engine::Generic => unreachable!("only SQLite and Turso read pragmas"),
        }
    }

    fn hidden(&self) -> &'static [&'static str] {
        match self.engine {
            Engine::Sqlite => &[],
            Engine::Turso => &["__turso_internal_"],
            Engine::Postgres
            | Engine::MySql
            | Engine::MariaDb
            | Engine::DuckDb
            | Engine::SqlServer
            | Engine::Snowflake
            | Engine::ClickHouse
            | Engine::MongoDb
            | Engine::Redis
            | Engine::Generic => unreachable!("only SQLite and Turso read pragmas"),
        }
    }

    /// The table's primary key columns, in key order.
    ///
    /// Empty for a table that declares none. Such a table still has a rowid
    /// that would identify a row, but the rowid is not in the result set unless
    /// the user selected it, and a predicate dbdelve cannot read off the grid is
    /// a predicate it must not write.
    fn primary_key(&self, schema: &str, table: &str) -> Result<Vec<String>, DbError> {
        let result = (self.run)(&format!(
            "SELECT name AS column_name
             FROM pragma_table_info({table}{schema})
             WHERE pk > 0
             ORDER BY pk",
            table = self.engine.quote_literal(table),
            schema = self.argument(schema),
        ))?;

        column_of(&result, "column_name")
    }

    /// The columns of an ordinary table, in table order, as `table_xinfo` lists
    /// them: the generated ones too, none of a virtual table's hidden ones.
    /// Empty for a view, a virtual table or a table that is not there, none of
    /// which has rows to name by their values.
    fn table_columns(&self, schema: &str, table: &str) -> Result<Vec<TableColumn>, DbError> {
        let result = (self.run)(&format!(
            "SELECT info.name AS column_name, info.type AS column_type,
                    info.hidden AS column_hidden
             FROM pragma_table_xinfo({table}{schema_argument}) AS info
             WHERE info.hidden IN (0, 2, 3)
               AND EXISTS (SELECT 1 FROM pragma_table_list AS listed
                           WHERE listed.name = {table} AND listed.schema = {schema}
                             AND listed.type = 'table')
             ORDER BY info.cid",
            table = self.engine.quote_literal(table),
            schema = self.engine.quote_literal(schema),
            schema_argument = self.argument(schema),
        ))?;

        result
            .rows
            .iter()
            .map(|row| {
                Ok(TableColumn {
                    name: required_cell(&result, row, "column_name")?.to_string(),
                    declared: required_cell(&result, row, "column_type")?.to_string(),
                    generated: required_cell(&result, row, "column_hidden")? != "0",
                })
            })
            .collect()
    }

    pub(super) fn catalog(&self) -> Result<Catalog, DbError> {
        let schemas = self.schemas()?;
        if schemas.is_empty() {
            return Ok(Catalog::default());
        }

        let relations = (self.run)(&relations_sql(self.engine, &schemas, self.hidden()))?;
        assemble_catalog(relations, QueryResult::default())
    }

    /// SQLite has no stored functions or procedures at all, so an empty list is
    /// the true answer rather than a gap in what dbdelve can see -- and there is
    /// nothing to ask the file for.
    pub(super) fn routines(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    /// `main`, `temp`, and anything `ATTACH`ed — under SQLite's own names for
    /// them, which is what the user typed if they attached it.
    fn schemas(&self) -> Result<Vec<String>, DbError> {
        let result =
            (self.run)("SELECT name AS schema_name FROM pragma_database_list ORDER BY seq")?;

        column_of(&result, "schema_name")
    }

    /// Who points at this relation. SQLite has no catalog to ask from the
    /// referenced side, so every table of the schema is asked what it
    /// references. A key that names no column of the parent references the
    /// parent's primary key, filled in the way [`Connection::foreign_keys`] does.
    pub(super) fn references(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<Reference>, DbError> {
        let mut result = (self.run)(&format!(
            "SELECT {schema_literal} AS source_schema,
                    tables.name AS source_table,
                    keys.\"from\" AS column_name,
                    COALESCE(keys.\"to\", '') AS referenced_column,
                    keys.id AS constraint_name
             FROM {schema_name}.sqlite_master AS tables,
                  pragma_foreign_key_list(tables.name{schema_argument}) AS keys
             WHERE tables.type = 'table'
               AND keys.\"table\" = {relation} COLLATE NOCASE
             ORDER BY tables.name, keys.id, keys.seq",
            schema_literal = self.engine.quote_literal(schema),
            schema_argument = self.argument(schema),
            schema_name = self.engine.quote_identifier(schema),
            relation = self.engine.quote_literal(relation),
        ))?;
        if result.rows.iter().any(|row| row.get(3) == Some("")) {
            let key = self.primary_key(schema, relation)?;
            let only = match key.as_slice() {
                [only] => Some(only.as_str()),
                _ => None,
            };
            for row in &mut result.rows {
                if row.get(3) == Some("") {
                    row.replace(&[(3, only)]);
                }
            }
            result.rows.retain(|row| row.get(3).is_some());
        }
        assemble_references(&result)
    }

    /// Three statements over `sqlite_master` joined to the pragma functions,
    /// each one call however many tables there are. Both engines take the
    /// join; Turso refuses the schema argument, which `argument` leaves off.
    ///
    /// A key's columns are matched to the tables' own spelling of them, since
    /// SQLite folds case in names and the graph does not.
    pub(super) fn schema_graph(&self, schema: &str) -> Result<SchemaGraph, DbError> {
        let master = self.engine.quote_identifier(schema);
        let filter = format!(
            "tables.type = 'table'
               AND tables.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'{}",
            hidden_prefixes(self.hidden(), "tables.name")
        );
        let argument = self.argument(schema);

        let columns = (self.run)(&format!(
            "SELECT tables.name AS table_name,
                    columns.name AS column_name,
                    columns.type AS data_type,
                    CASE
                        WHEN columns.\"notnull\" THEN 'no'
                        -- The rowid alias cannot hold a null, which the pragma
                        -- does not say; see `structure`. `PRIMARY KEY DESC` is
                        -- not an alias: it keeps an index, which marks it.
                        WHEN columns.pk = 1
                             AND upper(columns.type) = 'INTEGER'
                             AND (
                                 SELECT count(*)
                                 FROM pragma_table_info(tables.name{argument})
                                 WHERE pk > 0
                             ) = 1
                             AND NOT EXISTS (
                                 SELECT 1
                                 FROM pragma_index_list(tables.name{argument})
                                 WHERE origin = 'pk'
                             )
                            THEN 'no'
                        ELSE 'yes'
                    END AS nullable
             FROM {master}.sqlite_master AS tables
             LEFT JOIN pragma_table_xinfo(tables.name{argument}) AS columns
                    ON columns.hidden <> 1
             WHERE {filter}
             ORDER BY tables.name, columns.cid"
        ))?;

        // The primary key from the table's own columns, because a rowid alias
        // has no index to list; every other unique key from the indexes, so a
        // `WITHOUT ROWID` primary key is not read twice. A partial index or
        // one over an expression does not make its columns unique.
        let unique_keys = (self.run)(&format!(
            "SELECT table_name, constraint_name, column_name, key_kind
             FROM (
                 SELECT tables.name AS table_name,
                        -- Index names are the user's, so the two kinds are
                        -- prefixed apart: an index may be called anything.
                        'p' AS constraint_name,
                        columns.name AS column_name,
                        'primary' AS key_kind,
                        columns.pk AS key_position
                 FROM {master}.sqlite_master AS tables,
                      pragma_table_info(tables.name{argument}) AS columns
                 WHERE {filter} AND columns.pk > 0
                 UNION ALL
                 SELECT tables.name,
                        'i' || index_list.name,
                        key_column.name,
                        'unique',
                        key_column.seqno
                 FROM {master}.sqlite_master AS tables,
                      pragma_index_list(tables.name{argument}) AS index_list,
                      pragma_index_xinfo(index_list.name{argument}) AS key_column
                 WHERE {filter}
                   AND index_list.\"unique\" = 1
                   AND index_list.origin <> 'pk'
                   AND index_list.partial = 0
                   AND key_column.key = 1
                   AND NOT EXISTS (
                       SELECT 1
                       FROM pragma_index_xinfo(index_list.name{argument})
                       WHERE key = 1 AND name IS NULL
                   )
             )
             ORDER BY table_name, constraint_name, key_position"
        ))?;

        // A key naming no parent column references the parent's primary key,
        // which `pk` numbers in key order, as `seq` does the key. Turso says
        // so with an empty `to` where SQLite says NULL.
        let foreign_keys = (self.run)(&format!(
            "SELECT tables.name AS table_name,
                    CAST(keys.id AS text) AS constraint_name,
                    COALESCE((
                        SELECT name
                        FROM pragma_table_xinfo(tables.name{argument})
                        WHERE name = keys.\"from\" COLLATE NOCASE
                    ), keys.\"from\") AS column_name,
                    {schema_literal} AS referenced_schema,
                    COALESCE((
                        SELECT name
                        FROM {master}.sqlite_master
                        WHERE type = 'table' AND name = keys.\"table\" COLLATE NOCASE
                    ), keys.\"table\") AS referenced_table,
                    COALESCE((
                        SELECT name
                        FROM pragma_table_xinfo(keys.\"table\"{argument})
                        WHERE CASE
                                  WHEN COALESCE(keys.\"to\", '') = '' THEN pk = keys.seq + 1
                                  ELSE name = keys.\"to\" COLLATE NOCASE
                              END
                    ), keys.\"to\", '') AS referenced_column
             FROM {master}.sqlite_master AS tables,
                  pragma_foreign_key_list(tables.name{argument}) AS keys
             WHERE {filter}
             ORDER BY tables.name, keys.id, keys.seq",
            schema_literal = self.engine.quote_literal(schema),
        ))?;
        assemble_schema_graph(schema, &columns, &unique_keys, &foreign_keys)
    }

    pub(super) fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let columns = (self.run)(&format!(
            "SELECT name AS column_name,
                    type AS data_type,
                    CASE
                        WHEN \"notnull\" THEN 'no'
                        -- An `INTEGER PRIMARY KEY` is the rowid under another
                        -- name and cannot hold a null, but the pragma does not
                        -- say so. Every other kind of primary key column in
                        -- SQLite genuinely can, which is the quirk the rest of
                        -- this expression is careful to preserve.
                        WHEN pk = 1
                             AND upper(type) = 'INTEGER'
                             AND (
                                 SELECT count(*)
                                 FROM pragma_table_info({relation}{schema})
                                 WHERE pk > 0
                             ) = 1
                             -- `PRIMARY KEY DESC` is no alias: it keeps an index.
                             AND NOT EXISTS (
                                 SELECT 1
                                 FROM pragma_index_list({relation}{schema})
                                 WHERE origin = 'pk'
                             )
                            THEN 'no'
                        ELSE 'yes'
                    END AS nullable,
                    CASE hidden
                        WHEN 2 THEN 'GENERATED ALWAYS VIRTUAL'
                        WHEN 3 THEN 'GENERATED ALWAYS STORED'
                        ELSE COALESCE(dflt_value, '')
                    END AS column_default
             FROM pragma_table_xinfo({relation}{schema})
             -- A virtual table's hidden columns, which `SELECT *` leaves out
             -- too. 2 and 3 are generated columns.
             WHERE hidden <> 1
             ORDER BY cid",
            relation = self.engine.quote_literal(relation),
            schema = self.argument(schema),
        ))?;

        // The shared assembler reads the columns; SQLite has no catalog of
        // indexes or constraints to hand it, so those two are built below out of
        // pragmas rather than queried as definitions.
        let mut structure =
            assemble_structure(columns, QueryResult::default(), QueryResult::default())?;
        if structure
            .columns
            .iter()
            .any(|column| self.engine.generates(column))
        {
            self.spell_generated(schema, relation, &mut structure.columns)?;
        }
        structure.indexes = self.indexes(schema, relation)?;
        structure.constraints = self.constraints(schema, relation)?;
        structure.foreign_keys = self.foreign_keys(schema, relation)?;
        Ok(structure)
    }

    /// SQLite keeps a generated column's expression nowhere but in the
    /// table's `CREATE`, so it is read from there. One that does not parse
    /// keeps the default without its expression, which still marks the column
    /// as generated.
    fn spell_generated(
        &self,
        schema: &str,
        table: &str,
        columns: &mut [ColumnDefinition],
    ) -> Result<(), DbError> {
        let listed = (self.run)(&format!(
            "SELECT sql FROM {}.sqlite_master
             WHERE type = 'table' AND name = {} COLLATE NOCASE",
            self.engine.quote_identifier(schema),
            self.engine.quote_literal(table),
        ))?;
        let Some(created) = column_of(&listed, "sql")?.pop() else {
            return Ok(());
        };
        let Ok(statements) = Parser::parse_sql(&SQLiteDialect {}, &created) else {
            return Ok(());
        };
        let [Statement::CreateTable(created)] = statements.as_slice() else {
            return Ok(());
        };
        for definition in &created.columns {
            let Some((expression, mode)) =
                definition
                    .options
                    .iter()
                    .find_map(|option| match &option.option {
                        ColumnOption::Generated {
                            generation_expr: Some(expression),
                            generation_expr_mode,
                            ..
                        } => Some((expression, generation_expr_mode)),
                        _ => None,
                    })
            else {
                continue;
            };
            let Some(column) = columns
                .iter_mut()
                .find(|column| column.name.eq_ignore_ascii_case(&definition.name.value))
            else {
                continue;
            };
            // Without a word for it, SQLite stores nothing and computes on read.
            let mode = match mode {
                Some(GeneratedExpressionMode::Stored) => "STORED",
                Some(GeneratedExpressionMode::Virtual) | None => "VIRTUAL",
            };
            column.default = Some(format!("GENERATED ALWAYS AS ({expression}) {mode}"));
        }
        Ok(())
    }

    /// The relation's own `CREATE` first, then its indexes and triggers in
    /// the order they were made. An index SQLite made for a constraint has no
    /// text, and is declared by the table's.
    pub(super) fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        let listed = (self.run)(&format!(
            "SELECT sql FROM {}.sqlite_master
             WHERE tbl_name = {} COLLATE NOCASE AND sql IS NOT NULL
             ORDER BY type NOT IN ('table', 'view'), rowid",
            self.engine.quote_identifier(schema),
            self.engine.quote_literal(relation),
        ))?;
        let statements = column_of(&listed, "sql")?;
        if statements.is_empty() {
            return Err(plain_error(format!("{schema} has no relation {relation}.")));
        }
        Ok(statements.join(";\n") + ";")
    }

    /// An index the user wrote has its `CREATE INDEX` in `sqlite_master`. One
    /// SQLite created to enforce a `UNIQUE` or `PRIMARY KEY` has no row there at
    /// all, so its definition is reconstructed from the columns it covers.
    fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<Index>, DbError> {
        let listed = (self.run)(&format!(
            "SELECT index_list.name AS object_name,
                    index_list.\"unique\" AS is_unique,
                    index_list.origin AS origin,
                    index_list.partial AS partial,
                    COALESCE(sqlite_master.sql, '') AS definition,
                    key_column.name AS column_name,
                    key_column.\"desc\" AS descending
             FROM pragma_index_list({relation}{schema}) AS index_list
             JOIN pragma_index_xinfo(index_list.name{schema}) AS key_column
             LEFT JOIN {schema_identifier}.sqlite_master
                    ON sqlite_master.type = 'index'
                   AND sqlite_master.name = index_list.name
             WHERE key_column.key = 1
             ORDER BY index_list.name, key_column.seqno",
            relation = self.engine.quote_literal(relation),
            schema = self.argument(schema),
            schema_identifier = self.engine.quote_identifier(schema),
        ))?;

        let cell = |row, column| required_cell(&listed, row, column);
        let mut indexes = Vec::new();
        // One row per key column, grouped by `object_name`, the first.
        for rows in listed.rows.chunk_by(|a, b| a.get(0) == b.get(0)) {
            let first = &rows[0];
            let sql = cell(first, "definition")?;
            let (written, predicate) = written_index(sql);
            let keys = rows
                .iter()
                .map(|row| {
                    let descending = cell(row, "descending")? == "1";
                    Ok((optional_cell(&listed, row, "column_name"), descending))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let columns = keys
                .iter()
                .enumerate()
                .map(|(at, key)| match key {
                    (Some(name), true) => format!("{name} DESC"),
                    (Some(name), false) => name.to_string(),
                    // An expression has no name, so it is shown as written.
                    (None, _) => written.get(at).unwrap_or(&"(expression)").to_string(),
                })
                .collect();
            let unique = cell(first, "is_unique")? != "0";
            let partial = cell(first, "partial")? == "1";
            let definition = match sql {
                "" => {
                    let covered: Vec<String> = keys
                        .iter()
                        .filter_map(|(name, _)| name.map(str::to_string))
                        .collect();
                    format!(
                        "{} ({})",
                        if unique { "UNIQUE" } else { "INDEX" },
                        quoted_list(&covered)
                    )
                }
                sql => sql.to_string(),
            };
            indexes.push(Index {
                name: cell(first, "object_name")?.to_string(),
                kind: match (cell(first, "origin")?, unique) {
                    ("pk", _) => IndexKind::Primary,
                    (_, true) => IndexKind::Unique,
                    (_, false) => IndexKind::Plain,
                },
                columns,
                predicate: predicate.filter(|_| partial).map(str::to_string),
                definition,
            });
        }

        Ok(indexes)
    }

    /// The primary key and the foreign keys.
    ///
    /// `CHECK` constraints are deliberately absent: SQLite keeps them only
    /// inside the `CREATE TABLE` text, and recovering them means parsing DDL to
    /// show it back, which is a worse trade than not showing it.
    fn constraints(&self, schema: &str, relation: &str) -> Result<Vec<NamedDefinition>, DbError> {
        let mut constraints = Vec::new();

        let key = self.primary_key(schema, relation)?;
        if !key.is_empty() {
            constraints.push(NamedDefinition {
                name: "PRIMARY KEY".into(),
                definition: format!("PRIMARY KEY ({})", quoted_list(&key)),
            });
        }

        let listed = (self.run)(&format!(
            "SELECT id AS constraint_id,
                    \"table\" AS target_relation,
                    \"from\" AS source_column,
                    COALESCE(\"to\", '') AS target_column
             FROM pragma_foreign_key_list({relation}{schema})
             ORDER BY id, seq",
            relation = self.engine.quote_literal(relation),
            schema = self.argument(schema),
        ))?;

        // One constraint spans one row per column, so the rows are grouped here
        // rather than with `group_concat`, whose ordering clause is newer than
        // the oldest SQLite this could be pointed at.
        let mut current: Option<(String, KeyGroup)> = None;
        for row in &listed.rows {
            let id = required_cell(&listed, row, "constraint_id")?.to_string();
            let target = required_cell(&listed, row, "target_relation")?.to_string();
            let source_column = required_cell(&listed, row, "source_column")?.to_string();
            let target_column = required_cell(&listed, row, "target_column")?.to_string();

            match &mut current {
                Some((seen, key)) if *seen == id => {
                    key.sources.push(source_column);
                    if !target_column.is_empty() {
                        key.targets.push(target_column);
                    }
                }
                _ => {
                    if let Some((_, key)) = current.take() {
                        constraints.push(key.into());
                    }
                    current = Some((
                        id,
                        KeyGroup {
                            target,
                            sources: vec![source_column],
                            targets: Vec::from_iter(
                                (!target_column.is_empty()).then_some(target_column),
                            ),
                        },
                    ));
                }
            }
        }
        if let Some((_, key)) = current {
            constraints.push(key.into());
        }

        Ok(constraints)
    }

    /// The same pragma as [`Connection::constraints`], kept as fields: one
    /// entry per column of every foreign key, in key order.
    fn foreign_keys(&self, schema: &str, relation: &str) -> Result<Vec<ForeignKey>, DbError> {
        let listed = (self.run)(&format!(
            "SELECT id AS constraint_id,
                    \"table\" AS target_relation,
                    \"from\" AS source_column,
                    COALESCE(\"to\", '') AS target_column
             FROM pragma_foreign_key_list({relation}{schema})
             ORDER BY id, seq",
            relation = self.engine.quote_literal(relation),
            schema = self.argument(schema),
        ))?;

        let mut listed_keys = Vec::with_capacity(listed.rows.len());
        for row in &listed.rows {
            listed_keys.push((
                required_cell(&listed, row, "constraint_id")?.to_string(),
                required_cell(&listed, row, "target_relation")?.to_string(),
                required_cell(&listed, row, "source_column")?.to_string(),
                required_cell(&listed, row, "target_column")?.to_string(),
            ));
        }

        let mut parent_keys: HashMap<String, Vec<String>> = HashMap::new();
        let mut keys = Vec::new();
        for group in listed_keys.chunk_by(|a, b| a.0 == b.0) {
            let target = &group[0].1;
            let mut columns = Vec::with_capacity(group.len());
            for (position, (_, _, source, target_column)) in group.iter().enumerate() {
                let referenced_column = if target_column.is_empty() {
                    // A null `to` means the key references the parent's primary
                    // key without naming its columns, so the gap is filled from
                    // that key -- positionally, which is the correspondence the
                    // pragma's own `seq` order gives both sides.
                    if !parent_keys.contains_key(target) {
                        parent_keys.insert(target.clone(), self.primary_key(schema, target)?);
                    }
                    match parent_keys[target].get(position) {
                        Some(column) => column.clone(),
                        // A parent whose key cannot be stated gets no arrow at
                        // all: a `WHERE` aimed at a column that does not exist
                        // is worse than no navigation.
                        None => break,
                    }
                } else {
                    target_column.clone()
                };

                columns.push(ForeignKey {
                    column: source.clone(),
                    // A SQLite foreign key may not reference an attached
                    // database, so the parent of every key is in the database
                    // the key itself is in -- which is why the pragma reports no
                    // schema for one.
                    referenced_schema: schema.to_string(),
                    referenced_table: target.clone(),
                    referenced_column,
                });
            }

            if columns.len() == group.len() {
                keys.extend(columns);
            }
        }

        Ok(keys)
    }
}

/// One foreign key's rendering, gathered across the rows the pragma spreads it
/// over. Named for the grouping it does rather than for the key, because
/// [`ForeignKey`] is now the key as fields and this is the DDL text beside it.
struct KeyGroup {
    target: String,
    sources: Vec<String>,
    /// Empty when the key references the target's primary key without naming
    /// its columns, which SQLite reports as a null `to`.
    targets: Vec<String>,
}

impl From<KeyGroup> for NamedDefinition {
    fn from(key: KeyGroup) -> Self {
        let references = if key.targets.is_empty() {
            Engine::Sqlite.quote_identifier(&key.target)
        } else {
            format!(
                "{} ({})",
                Engine::Sqlite.quote_identifier(&key.target),
                quoted_list(&key.targets)
            )
        };

        Self {
            // SQLite does not report a constraint's name, so the columns it
            // constrains are what distinguishes one from another on a table
            // with more than one.
            name: format!("FOREIGN KEY ({})", quoted_list(&key.sources)),
            definition: format!(
                "FOREIGN KEY ({}) REFERENCES {references}",
                quoted_list(&key.sources)
            ),
        }
    }
}

/// One column as SQLite describes it: where the value came from.
///
/// All three are absent together for a computed column. The plumbing between
/// the description and [`resolve_edit_target`] stops in this module.
struct ProbedColumn {
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    /// A row read held a BLOB here. SQLite stores one in a column of any
    /// declared type, and its rendering, `x'…'`, is also a text such a column
    /// can hold, so no predicate spelled from it names that row alone.
    blob: bool,
}

/// The one table every column that came from a table came from.
///
/// A result set spanning two of them is a join, and a row of it is not a row of
/// either table, so there is nothing to write back to. A result set spanning
/// none is entirely computed. Two tables of the same name in different attached
/// databases are two tables, which is why the schema is half of the answer.
fn sole_table(probed: &[ProbedColumn]) -> Option<(String, String)> {
    let mut named = probed
        .iter()
        .filter_map(|column| Some((column.schema.clone()?, column.table.clone()?)));
    let first = named.next()?;
    named.all(|table| table == first).then_some(first)
}

/// Whether no two result columns read the same table column.
///
/// Two result columns reading the same table column are the two sides of a
/// self-join, and no driver reports the alias that tells them apart. The key
/// is located by position, so it would resolve to whichever side came first
/// and write the edit at the other row's key; the same shape also generates
/// one SET clause per side for the same column. Refusing the whole result set
/// is one answer to both.
fn reads_each_column_once(probed: &[ProbedColumn]) -> bool {
    let mut origins = HashSet::new();
    probed
        .iter()
        .filter_map(origin_of)
        .all(|column| origins.insert(column))
}

/// The real column name behind a result column. The caller has already
/// established that the columns carrying a table all carry the same one, so
/// carrying a table at all means carrying this one.
fn origin_of(column: &ProbedColumn) -> Option<String> {
    column.table.as_ref().and(column.column.clone())
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
    if !reads_each_column_once(probed) {
        return None;
    }
    let keys = key
        .iter()
        .map(|name| {
            probed
                .iter()
                .position(|column| origin_of(column).as_deref() == Some(name.as_str()))
        })
        .collect::<Option<Vec<usize>>>()?;
    if keys.is_empty() {
        return None;
    }

    Some(EditTarget {
        schema: schema.to_string(),
        table: table.to_string(),
        columns: probed.iter().map(origin_of).collect(),
        keys,
        ..Default::default()
    })
}

/// One column of a table as `table_xinfo` describes it.
struct TableColumn {
    name: String,
    /// The type as the table declares it, possibly empty.
    declared: String,
    /// A generated column: matched, never set.
    generated: bool,
}

/// Whether `=` against a literal finds a column's values as the grid shows
/// them. Fail closed: only the affinities the apply was probed on are listed.
///
/// ponytail: INTEGER, TEXT and REAL, by SQLite's own rules for a declared
/// type; NUMERIC affinity (`numeric`, `decimal`, `date`, `boolean`) and a
/// column with no declared type are not, so a keyless table holding one is
/// read-only until its matching is probed. Nor is BLOB: a blob renders as
/// `x'…'`, which the writer quotes as that text, and a text is never equal to
/// a blob.
fn is_matchable(declared: &str) -> bool {
    let declared = declared.to_ascii_uppercase();
    !declared.is_empty()
        && ["INT", "CHAR", "CLOB", "TEXT", "REAL", "FLOA", "DOUB"]
            .iter()
            .any(|word| declared.contains(word))
}

/// The target of a result set over a table with no primary key: every column
/// of the table names the row, a NULL as `IS NULL`, the generated columns
/// matched but never set. Or the reason it cannot be, when the user can do
/// something about it.
///
/// Never a unique key and never the rowid (neither is in the result set the
/// grid reads its values from), and never some of the columns: a column left
/// out of the `WHERE` matches rows the user is not looking at, so a column that
/// cannot be matched by its value, or that the result omits, makes the whole
/// result read-only.
fn resolve_keyless_target(
    probed: &[ProbedColumn],
    schema: &str,
    table: &str,
    described: &[TableColumn],
) -> (Option<EditTarget>, Option<String>) {
    // A view, a virtual table or a table that is gone describes no columns.
    if described.is_empty() || !reads_each_column_once(probed) {
        return (None, None);
    }
    if probed.iter().any(|column| origin_of(column).is_none()) {
        return (None, None);
    }
    let refusal = |what: String| {
        Some(format!(
            "A table without a primary key is edited by matching every column, and {what}."
        ))
    };
    if let Some(column) = probed.iter().find(|column| column.blob) {
        return (
            None,
            refusal(format!(
                "{} holds a BLOB, which cannot be matched by its value",
                origin_of(column).unwrap_or_default()
            )),
        );
    }
    if let Some(column) = described
        .iter()
        .find(|column| !is_matchable(&column.declared))
    {
        let declared = match column.declared.as_str() {
            "" => "no declared type",
            declared => declared,
        };
        return (
            None,
            refusal(format!(
                "{} ({declared}) cannot be matched by its value",
                column.name
            )),
        );
    }
    let mut keys = Vec::new();
    let mut fixed = Vec::new();
    for column in described {
        let Some(at) = probed
            .iter()
            .position(|probed| origin_of(probed).as_deref() == Some(column.name.as_str()))
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
        columns: probed.iter().map(origin_of).collect(),
        keys,
        row_match: RowMatch::EveryColumn { fixed },
        ..Default::default()
    };
    (Some(target), None)
}

/// One `sqlite_master` scan per schema, aliased to the column names
/// [`assemble_catalog`] reads rather than to SQLite's own.
fn hidden_prefixes(hidden: &[&str], column: &str) -> String {
    hidden
        .iter()
        .map(|prefix| {
            format!(
                "\n                   AND {column} NOT LIKE '{}%' ESCAPE '\\'",
                prefix.replace('_', "\\_")
            )
        })
        .collect()
}

fn relations_sql(engine: Engine, schemas: &[String], hidden: &[&str]) -> String {
    let hidden = hidden_prefixes(hidden, "name");
    let scans: Vec<String> = schemas
        .iter()
        .map(|schema| {
            format!(
                "SELECT {schema_literal} AS schema_name,
                        name AS relation_name,
                        CASE type
                            WHEN 'table' THEN 'table'
                            WHEN 'view' THEN 'view'
                        END AS relation_kind
                 FROM {schema_identifier}.sqlite_master
                 WHERE type IN ('table', 'view')
                   AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'{hidden}",
                schema_literal = engine.quote_literal(schema),
                schema_identifier = engine.quote_identifier(schema),
            )
        })
        .collect();

    format!(
        "{}\nORDER BY schema_name, relation_name",
        scans.join("\nUNION ALL\n")
    )
}

/// Every row's value for one column, for the pragmas that return a single
/// column and would otherwise each grow their own loop.
fn column_of(result: &QueryResult, column: &str) -> Result<Vec<String>, DbError> {
    result
        .rows
        .iter()
        .map(|row| required_cell(result, row, column).map(str::to_string))
        .collect()
}

/// The key list and the `WHERE` condition of a `CREATE INDEX` as it was
/// written. SQLite keeps neither in a catalog: `pragma_index_xinfo` gives an
/// expression no name, and a partial index's condition is only in this text.
/// A comment is stepped over like a quoted string, so a `)` or `,` inside one
/// ends nothing, and one before or after a key is left out of it.
fn written_index(sql: &str) -> (Vec<&str>, Option<&str>) {
    let (mut depth, mut start, mut end, mut at) = (0, 0, 0, 0);
    let mut keys = Vec::new();
    while let Some(c) = sql[at..].chars().next() {
        let rest = &sql[at..];
        if let Some(length) = comment_length(rest) {
            at += length;
            continue;
        }
        let step = match c {
            '\'' | '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                rest[1..].find(close).map_or(rest.len(), |close| close + 2)
            }
            '(' => {
                depth += 1;
                if depth == 1 {
                    start = at + 1;
                }
                1
            }
            ',' if depth == 1 => {
                keys.push(past_comments(&sql[start..end.max(start)]));
                start = at + 1;
                1
            }
            ')' if depth == 1 => {
                keys.push(past_comments(&sql[start..end.max(start)]));
                let rest = past_comments(&sql[at + 1..]);
                let predicate = rest
                    .get(..5)
                    .filter(|word| word.eq_ignore_ascii_case("WHERE"))
                    .map(|_| rest[5..].trim());
                return (keys, predicate);
            }
            ')' => {
                depth -= 1;
                1
            }
            c => c.len_utf8(),
        };
        if !c.is_whitespace() {
            end = at + step;
        }
        at += step;
    }
    (Vec::new(), None)
}

/// `text` from its first character that is neither blank nor in a comment.
fn past_comments(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        match comment_length(text) {
            Some(length) => text = &text[length..],
            None => return text,
        }
    }
}

/// How long the comment `text` starts with is, an unclosed one running to
/// the end.
fn comment_length(text: &str) -> Option<usize> {
    if let Some(rest) = text.strip_prefix("/*") {
        Some(rest.find("*/").map_or(text.len(), |close| close + 4))
    } else if text.starts_with("--") {
        Some(text.find('\n').map_or(text.len(), |close| close + 1))
    } else {
        None
    }
}

fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| Engine::Sqlite.quote_identifier(name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A value as text.
///
/// Postgres has the server do this and hands dbdelve the result; here it is
/// dbdelve's decision, so each one is made to be reversible — what the grid shows
/// is something SQLite would accept back.
fn render<'a>(
    value: ValueRef<'a>,
    columns: &[Column],
    index: usize,
) -> Result<Option<Cow<'a, str>>, DbError> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Integer(integer) => Some(integer.to_string().into()),
        ValueRef::Real(real) => Some(render_real(real).into()),
        // A database whose text was written as something other than UTF-8 can
        // return bytes that are not text at all. Postgres raises the same error
        // for the same reason, naming the column rather than the row.
        ValueRef::Text(bytes) => Some(
            std::str::from_utf8(bytes)
                .map_err(|_| non_utf8_error(columns, index))?
                .into(),
        ),
        // SQLite's own literal syntax for a blob, rather than bare hex, so a
        // value copied out of the grid is a value that can be pasted into a
        // statement.
        ValueRef::Blob(bytes) => Some(format!("x'{}'", hex::encode_upper(bytes)).into()),
    })
}

/// A real keeps its point even when it is whole. Rust renders `1.0` as `1`,
/// which in a SQLite grid is the rendering of an integer, and the difference
/// between the two storage classes is one the user is entitled to see.
pub(super) fn render_real(real: f64) -> String {
    let rendered = real.to_string();
    match real.is_finite() && !rendered.contains(['.', 'e', 'E']) {
        true => format!("{rendered}.0"),
        false => rendered,
    }
}

/// `submission` is what the user ran, which is not what SQLite was parsing: a
/// statement after the first is prepared from a tail of it, and the offset in
/// the error is into that tail. The error carries the tail, so the prefix to add
/// back is what the two lengths differ by — and if it does not turn out to be a
/// tail, there is no offset rather than a wrong one.
fn query_error(error: &rusqlite::Error, submission: &str) -> DbError {
    match error {
        rusqlite::Error::SqlInputError {
            msg, sql, offset, ..
        } => DbError {
            message: msg.clone(),
            position: usize::try_from(*offset)
                .ok()
                .filter(|offset| submission.ends_with(sql.as_str()) && *offset <= sql.len())
                .map(|offset| submission.len() - sql.len() + offset),
        },
        other => DbError {
            message: describe(other),
            position: None,
        },
    }
}

/// Deliberately no `SQLITE_OPEN_CREATE`. With it, a mistyped path is an empty
/// database that opens successfully and then reports an empty catalog, which
/// reads as "this database has nothing in it" rather than "this is not the file
/// you meant". dbdelve would also have littered a file onto the user's disk to
/// tell them so.
///
/// `SQLITE_OPEN_URI` is off for the same reason: the path came out of the URL
/// already, and leaving URI parsing on would make a path containing `?` mean
/// something other than itself.
fn open_file(path: &std::path::Path) -> Result<rusqlite::Connection, DbError> {
    rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        plain_error(format!(
            "Cannot open {}: {}",
            path.display(),
            describe(&error)
        ))
    })
}

/// One submission run on a session already held: the statements, stepped as
/// SQLite splits them, and what `fetch` asks of the last one's rows, with the
/// columns that statement was read from. Everything [`Connection::run`] does
/// under its lock, which is not reentrant: a sequence that holds the lock for
/// itself ([`Connection::apply_verified`]) sends each statement through this.
fn ran(
    connection: &rusqlite::Connection,
    sql: &str,
    fetch: Fetch,
) -> Result<(QueryResult, Vec<ProbedColumn>), DbError> {
    let mut result = QueryResult::default();
    let mut probed = Vec::new();
    // How long the kept rows took, once there were as many as the limit.
    let mut filled = None;
    let mut past_limit = false;

    // Whether this submission is the one that opened a transaction, asked
    // before it runs: a transaction the user began in an earlier run is
    // theirs to finish, not dbdelve's to discard on their next typo.
    let outside_a_transaction = connection.is_autocommit();

    let outcome = (|| -> Result<(), DbError> {
        // SQLite splits the submission itself, using the tail its own
        // parser reports. dbdelve's tree-sitter grammar could have done it,
        // but then a statement SQLite accepts and the grammar does not
        // would become a statement dbdelve refuses to send — and what the
        // user types goes to the engine verbatim.
        let mut batch = Batch::new(connection, sql);

        // Timed from here, not from the call: one connection serialises a
        // profile's queries, and time spent waiting behind the catalog load
        // is not time spent on this statement.
        let started = Instant::now();
        let mut builder = RowBuilder::default();
        while let Some(mut statement) = batch.next().map_err(|error| query_error(&error, sql))? {
            // Both borrow the statement, and running it needs it mutably,
            // so everything the description says is taken first.
            let columns: Vec<Column> = statement
                .columns()
                .iter()
                .map(|column| Column {
                    name: column.name().to_string(),
                    // Lowercased because Postgres names its types in lower
                    // case and both engines' tags appear in the same grid;
                    // SQLite's is whatever case the `CREATE TABLE` used.
                    data_type: column.decl_type().map(str::to_lowercase),
                })
                .collect();
            let mut described: Vec<ProbedColumn> = statement
                .columns_with_metadata()
                .iter()
                .map(|column| ProbedColumn {
                    schema: column.database_name().map(str::to_string),
                    table: column.table_name().map(str::to_string),
                    column: column.origin_name().map(str::to_string),
                    blob: false,
                })
                .collect();

            // No columns is a command rather than a query, and only a
            // command has rows to have affected.
            if columns.is_empty() {
                statement
                    .execute([])
                    .map_err(|error| query_error(&error, sql))?;
                result.rows_affected = Some(connection.changes());
                continue;
            }

            // One selection can carry several statements, and the grid shows
            // one result set, so each new description starts the kept set
            // over and the last statement wins. The declared types came
            // with the description, so the rows are shown by type from the
            // first.
            if let Some(feed) = fetch.feed {
                feed.begin(columns.clone());
            }
            let mut rows = Vec::new();
            let mut bytes = 0;
            // This set's rows, kept or not, and kept: a fed row is not in
            // `rows` to be counted.
            let (mut returned, mut kept) = (0, 0);
            filled = None;
            let mut stepped = statement
                .query([])
                .map_err(|error| query_error(&error, sql))?;
            while let Some(row) = stepped.next().map_err(|error| query_error(&error, sql))? {
                returned += 1;
                if fetch.limit.is_some_and(|limit| kept >= limit) {
                    // There is no server to tell: a read is stopped by
                    // stepping it no further. Dropping `stepped` resets the
                    // statement, which ends the read transaction it opened
                    // and leaves one the user began as it was.
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
                for index in 0..columns.len() {
                    let value = row
                        .get_ref(index)
                        .map_err(|error| query_error(&error, sql))?;
                    if matches!(value, ValueRef::Blob(_))
                        && let Some(column) = described.get_mut(index)
                    {
                        column.blob = true;
                    }
                    let cell = render(value, &columns, index)?;
                    builder.push(cell.as_deref());
                }
                let cells = builder.finish()?;
                bytes += cells.text_len();
                match fetch.feed {
                    Some(feed) => feed.push(cells),
                    None => rows.push(cells),
                }
            }

            // A query's count is the rows it returned. `changes()` would
            // report whatever the statement before it modified, since
            // SQLite does not reset it for a select.
            result.rows_affected = Some(returned as u64);
            result.capped_from = (result.stopped.is_none() && returned > kept).then_some(returned);
            past_limit = returned > kept;
            result.columns = columns;
            result.rows = rows;
            result.bytes = bytes;
            probed = described;
            if result.stopped.is_some() {
                break;
            }
        }
        // Timed to the last kept row when rows past the limit came after
        // it: draining the statement is not the wait for these.
        result.elapsed = match filled {
            Some(filled) if past_limit => filled,
            _ => started.elapsed(),
        };
        Ok(())
    })();

    if let Err(error) = outcome {
        return Err(rolled_back(connection, outside_a_transaction, error));
    }

    Ok((result, probed))
}

/// End the transaction a failed batch left open, and say so in the error.
///
/// SQLite stops the batch at the failing statement, so the `COMMIT` dbdelve wrote
/// into the text never runs and the transaction stays open on a connection that
/// outlives the statement: the refresh that follows reads the uncommitted rows
/// back as though the apply had succeeded, and the next batch's `BEGIN` fails
/// inside it. The brackets are there to make the batch all-or-nothing; without
/// this they produce a third state instead, and one rendered as success.
///
/// Only a transaction *this* submission opened is rolled back, so one the user
/// began in an earlier run is theirs to finish. Which state the data is in is
/// the part the user cannot see for themselves, so the notice carries it rather
/// than the log.
fn rolled_back(
    connection: &rusqlite::Connection,
    outside_a_transaction: bool,
    error: DbError,
) -> DbError {
    if !outside_a_transaction || connection.is_autocommit() {
        return error;
    }

    let outcome = match connection.execute_batch("ROLLBACK") {
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

/// Prefer SQLite's own message. The driver wraps it in a variant name that adds
/// nothing a user would read.
fn describe(error: &rusqlite::Error) -> String {
    match error {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
pub(super) mod graph_fixture {
    use super::*;

    /// Tables written to trip the graph read: names that differ in case from
    /// what refers to them, a key that names no parent column, a key into the
    /// table itself, and unique indexes that make nothing unique.
    pub const GRAPH_FIXTURE: &str = "
        CREATE TABLE parent (
            id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE, a INT, b INT, UNIQUE (a, b)
        );
        CREATE TABLE node (id INTEGER PRIMARY KEY, parent_id INT REFERENCES node, label TEXT);
        CREATE TABLE child (
            x INT, y INT, parent_code TEXT, note TEXT,
            FOREIGN KEY (x, y) REFERENCES PARENT (A, B),
            FOREIGN KEY (parent_code) REFERENCES parent (code)
        );
        CREATE UNIQUE INDEX child_noted ON child (note) WHERE note IS NOT NULL;
        CREATE UNIQUE INDEX child_lowered ON child (lower(parent_code));
        CREATE TABLE lines (
            order_id INT NOT NULL, n INT NOT NULL, PRIMARY KEY (order_id, n)
        );
        CREATE VIEW seen AS SELECT 1 AS one;
    ";

    /// A rowid alias in its one disguise that is not one, and an index named
    /// like the primary key.
    pub const EDGE_FIXTURE: &str = "
        CREATE TABLE desc_key (id INTEGER PRIMARY KEY DESC);
        CREATE TABLE named (id INTEGER PRIMARY KEY, code TEXT);
        CREATE UNIQUE INDEX \"PRIMARY KEY\" ON named (code);
    ";

    pub fn assert_edge_fixture(graph: &SchemaGraph) {
        use crate::db::graph::seed;

        assert!(seed::column(graph, "desc_key", "id").nullable);
        let id = seed::column(graph, "named", "id");
        assert!(id.primary && id.unique);
        assert!(seed::column(graph, "named", "code").unique);
    }

    pub fn assert_graph_fixture(graph: &SchemaGraph) {
        use crate::db::graph::{KeyTarget, seed};

        let mut tables: Vec<&str> = graph.tables.iter().map(|t| t.name.as_ref()).collect();
        tables.sort();
        assert_eq!(tables, ["child", "lines", "node", "parent"]);

        let key_of = |table: &str| {
            graph
                .keys
                .iter()
                .filter(|key| key.table == seed::table_index(graph, table))
                .collect::<Vec<_>>()
        };
        let child = key_of("child");
        assert_eq!(child.len(), 2);
        let [composite, single] = if child[0].columns.len() == 2 {
            [child[0], child[1]]
        } else {
            [child[1], child[0]]
        };
        let parent = seed::table_index(graph, "parent");
        assert_eq!(
            seed::names(graph, composite.table, &composite.columns),
            ["x", "y"]
        );
        let KeyTarget::Table { table, columns } = &composite.referenced else {
            panic!("{:?}", composite.referenced);
        };
        assert_eq!(*table, parent);
        assert_eq!(seed::names(graph, parent, columns), ["a", "b"]);
        assert_eq!(
            seed::names(graph, single.table, &single.columns),
            ["parent_code"]
        );
        let KeyTarget::Table { columns, .. } = &single.referenced else {
            panic!("{:?}", single.referenced);
        };
        assert_eq!(seed::names(graph, parent, columns), ["code"]);

        let [own] = key_of("node")[..] else {
            panic!("node references itself once");
        };
        let KeyTarget::Table { table, columns } = &own.referenced else {
            panic!("{:?}", own.referenced);
        };
        assert_eq!(*table, own.table);
        assert_eq!(seed::names(graph, *table, columns), ["id"]);

        let id = seed::column(graph, "parent", "id");
        assert!(id.primary && id.unique && !id.nullable);
        assert!(seed::column(graph, "parent", "code").unique);
        assert!(!seed::column(graph, "parent", "a").unique);
        assert!(!seed::column(graph, "child", "note").unique);
        assert!(!seed::column(graph, "child", "parent_code").unique);
        let order = seed::column(graph, "lines", "order_id");
        assert!(order.primary && !order.unique && !order.nullable);
        assert!(seed::column(graph, "child", "x").foreign);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, RelationKind};

    /// A connection over a database built in memory. Needs nothing external, so
    /// unlike the `live_` tests below these run everywhere.
    fn memory(setup: &str) -> Connection {
        memory_with_timeout(setup, 0)
    }

    fn memory_with_timeout(setup: &str, statement_timeout: u32) -> Connection {
        let connection = rusqlite::Connection::open_in_memory().expect("in-memory should open");
        connection.execute_batch(setup).expect("setup should apply");
        Connection::wrap(connection, statement_timeout)
    }

    /// A step of a verified apply as `sql::draft_batch` writes it, over `birds`.
    fn step(
        kind: crate::db::StepKind,
        sql: &str,
        id: &str,
        before: &[(&str, Option<&str>)],
    ) -> crate::db::VerifiedStep {
        crate::db::VerifiedStep {
            sql: sql.into(),
            kind,
            reread: Some(format!(
                r#"SELECT * FROM "main"."birds" WHERE "id" = '{id}'"#
            )),
            before: before
                .iter()
                .map(|(column, text)| crate::db::FetchedCell {
                    column: (*column).into(),
                    text: text.map(Into::into),
                    cell_type: None,
                    column_type: None,
                })
                .collect(),
            read_back: None,
            by_key: true,
        }
    }

    fn insert(id: &str, name: &str) -> crate::db::VerifiedStep {
        crate::db::VerifiedStep {
            sql: format!(r#"INSERT INTO "main"."birds" ("id", "name") VALUES ('{id}', '{name}')"#),
            kind: crate::db::StepKind::Insert,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        }
    }

    /// A transaction the user opened on the connection, whatever the workspace
    /// read off their statements, is seen by the apply before it sends its
    /// own `BEGIN`, and left open: nothing is written and nothing ended.
    #[test]
    fn an_apply_inside_the_users_transaction_is_refused_and_leaves_it_open() {
        use crate::db::{ApplyOutcome, ApplyStage, StepKind};

        let connection = memory(
            "create table birds (id integer primary key, name text not null); \
             insert into birds values (1, 'wren');",
        );
        connection
            .query("BEGIN", Fetch::default())
            .expect("the user's BEGIN");
        let update = step(
            StepKind::Update,
            r#"UPDATE "main"."birds" SET "name" = 'robin' WHERE "id" = '1'"#,
            "1",
            &[("id", Some("1")), ("name", Some("wren"))],
        );
        assert_eq!(
            connection.apply_verified(&[update], &|| true),
            ApplyOutcome::Failed {
                stage: ApplyStage::Begin,
                error: crate::db::in_a_transaction(),
            }
        );
        let inner = connection.connection.lock().unwrap();
        assert!(
            !inner.is_autocommit(),
            "the user's transaction is still open"
        );
        drop(inner);
        assert_eq!(name_of(&connection, 1).as_deref(), Some("wren"));
    }

    fn name_of(connection: &Connection, id: i64) -> Option<String> {
        connection
            .connection
            .lock()
            .unwrap()
            .query_row("select name from birds where id = ?1", [id], |row| {
                row.get(0)
            })
            .ok()
    }

    #[test]
    fn an_update_or_insert_must_count_one_and_a_zero_delete_must_find_its_row_gone() {
        use crate::db::{ApplyOutcome, StepKind};

        let birds = "create table birds (id integer primary key, name text not null, label text); \
                     insert into birds values (1, 'wren', 'a'), (2, 'tit', 'a'), (3, 'jay', 'b');";
        let delete = |id: &str, name: &str| {
            step(
                StepKind::Delete,
                &format!(r#"DELETE FROM "main"."birds" WHERE "id" = '{id}'"#),
                id,
                &[("id", Some(id)), ("name", Some(name))],
            )
        };
        let update = |id: &str, name: &str| {
            step(
                StepKind::Update,
                &format!(r#"UPDATE "main"."birds" SET "name" = 'robin' WHERE "id" = '{id}'"#),
                id,
                &[("id", Some(id)), ("name", Some(name))],
            )
        };
        let apply = |connection: &Connection, steps: &[crate::db::VerifiedStep]| {
            connection.apply_verified(steps, &|| true)
        };
        let rows_of = |connection: &Connection| -> i64 {
            connection
                .connection
                .lock()
                .unwrap()
                .query_row("select count(*) from birds", [], |row| row.get(0))
                .unwrap()
        };

        // An UPDATE a trigger skipped counts none: the whole sequence, the
        // DELETE before it included, is rolled back.
        let connection = memory(&format!(
            "{birds} create trigger skips before update on birds when old.id = 2 \
             begin select raise(ignore); end;"
        ));
        assert_eq!(
            apply(&connection, &[delete("1", "wren"), update("2", "tit")]),
            ApplyOutcome::Miscounted {
                step: 1,
                count: Some(0)
            }
        );
        assert_eq!(rows_of(&connection), 3);
        assert_eq!(name_of(&connection, 2).as_deref(), Some("tit"));
        assert!(connection.connection.lock().unwrap().is_autocommit());

        // An UPDATE that reaches two rows counts two, and both keep their values.
        let connection = memory(birds);
        let mut both = update("1", "wren");
        both.sql = r#"UPDATE "main"."birds" SET "name" = 'robin' WHERE "label" = 'a'"#.into();
        assert_eq!(
            apply(&connection, &[both]),
            ApplyOutcome::Miscounted {
                step: 0,
                count: Some(2)
            }
        );
        assert_eq!(name_of(&connection, 1).as_deref(), Some("wren"));
        assert_eq!(name_of(&connection, 2).as_deref(), Some("tit"));

        // One that counts one applies.
        let connection = memory(birds);
        assert_eq!(
            apply(&connection, &[update("1", "wren")]),
            ApplyOutcome::Applied {
                affected: 1,
                read_back: None
            }
        );
        assert_eq!(name_of(&connection, 1).as_deref(), Some("robin"));

        // A DELETE that counts none is a row an earlier DELETE took with it,
        // which the second read of its row finds gone.
        let connection = memory(&format!(
            "{birds} create trigger cascades after delete on birds when old.id = 1 \
             begin delete from birds where id = 2; end;"
        ));
        assert_eq!(
            apply(&connection, &[delete("1", "wren"), delete("2", "tit")]),
            ApplyOutcome::Applied {
                affected: 1,
                read_back: None
            }
        );
        assert_eq!(rows_of(&connection), 1);

        // One that counts none with its row still there was skipped, and is
        // refused.
        let connection = memory(&format!(
            "{birds} create trigger keeps before delete on birds when old.id = 2 \
             begin select raise(ignore); end;"
        ));
        assert_eq!(
            apply(&connection, &[delete("2", "tit")]),
            ApplyOutcome::Miscounted {
                step: 0,
                count: Some(0)
            }
        );
        assert_eq!(rows_of(&connection), 3);

        // An INSERT counts one, or is refused when a trigger skipped it.
        let connection = memory(&format!(
            "{birds} create trigger skips before insert on birds when new.id = 5 \
             begin select raise(ignore); end;"
        ));
        assert_eq!(
            apply(&connection, &[insert("4", "owl")]),
            ApplyOutcome::Applied {
                affected: 1,
                read_back: None
            }
        );
        assert_eq!(
            apply(&connection, &[delete("1", "wren"), insert("5", "kite")]),
            ApplyOutcome::Miscounted {
                step: 1,
                count: Some(0)
            }
        );
        assert_eq!(rows_of(&connection), 4, "the DELETE before it is undone");
        assert_eq!(name_of(&connection, 5), None);
    }

    #[test]
    fn a_verified_step_reads_its_row_back_inside_the_transaction() {
        use crate::db::{ApplyOutcome, StepKind};

        let setup = "create table birds (id integer primary key, name text not null); \
                     insert into birds values (1, 'wren');";
        let mut update = step(
            StepKind::Update,
            r#"UPDATE "main"."birds" SET "name" = 'robin' WHERE "id" = '1'"#,
            "1",
            &[("id", Some("1")), ("name", Some("wren"))],
        );
        update.read_back = update.reread.clone();
        let named = |outcome: ApplyOutcome| {
            let ApplyOutcome::Applied {
                affected,
                read_back: Some(read),
            } = outcome
            else {
                panic!("applied with its row read back, not {outcome:?}");
            };
            let name = read.columns.iter().position(|c| c.name == "name").unwrap();
            assert_eq!(read.rows.len(), 1);
            (affected, read.rows[0].get(name).map(str::to_owned))
        };

        let connection = memory(setup);
        assert_eq!(
            named(connection.apply_verified(std::slice::from_ref(&update), &|| true)),
            (1, Some("robin".to_string()))
        );
        assert!(connection.connection.lock().unwrap().is_autocommit());
    }

    #[test]
    fn a_verified_sequence_reads_every_row_before_it_writes_any() {
        use crate::db::{ApplyOutcome, Drift, StepKind};

        // The delete of row 1 rewrites row 2 behind it: a row read after that
        // write would be called changed, which is the draft's own cascade and
        // not a drift.
        let setup = "create table birds (id integer primary key, name text not null); \
                     insert into birds values (1, 'wren'), (2, 'tit'); \
                     create trigger cascades after delete on birds \
                     begin update birds set name = 'cascaded' where id = 2; end;";
        let delete = r#"DELETE FROM "main"."birds" WHERE "id" = '1'"#;
        let update = r#"UPDATE "main"."birds" SET "name" = 'robin' WHERE "id" = '2'"#;
        // The manifest of row 2 lists its columns in another order than the table.
        let steps = |second: &str| {
            [
                step(
                    StepKind::Delete,
                    delete,
                    "1",
                    &[("id", Some("1")), ("name", Some("wren"))],
                ),
                step(
                    StepKind::Update,
                    update,
                    "2",
                    &[("name", Some(second)), ("id", Some("2"))],
                ),
            ]
        };

        let connection = memory(setup);
        assert_eq!(
            connection.apply_verified(&steps("tit"), &|| true),
            ApplyOutcome::Applied {
                affected: 2,
                read_back: None
            }
        );
        assert_eq!(name_of(&connection, 1), None);
        assert_eq!(name_of(&connection, 2).as_deref(), Some("robin"));

        // The second row was changed after it was read: nothing is written, the
        // first row stays, and the session is left outside a transaction.
        let connection = memory(setup);
        assert_eq!(
            connection.apply_verified(&steps("tit, changed"), &|| true),
            ApplyOutcome::Drifted(vec![(1, Drift::Changed)])
        );
        assert_eq!(name_of(&connection, 1).as_deref(), Some("wren"));
        assert_eq!(name_of(&connection, 2).as_deref(), Some("tit"));
        assert!(connection.connection.lock().unwrap().is_autocommit());

        // Every drifted row is named, not the first alone.
        let connection = memory(setup);
        let mut both = steps("tit, changed");
        both[0].before[1].text = Some("wren, changed".into());
        assert_eq!(
            connection.apply_verified(&both, &|| true),
            ApplyOutcome::Drifted(vec![(0, Drift::Changed), (1, Drift::Changed)])
        );

        // A row another session deleted is gone, not changed.
        let connection = memory(setup);
        connection
            .connection
            .lock()
            .unwrap()
            .execute_batch("delete from birds where id = 2")
            .unwrap();
        assert_eq!(
            connection.apply_verified(&steps("tit"), &|| true),
            ApplyOutcome::Drifted(vec![(1, Drift::Gone)])
        );
        assert_eq!(name_of(&connection, 1).as_deref(), Some("wren"));
    }

    /// Runs until something stops it. The recursion has no termination
    /// condition, which is the shape of the runaway both of these are about.
    const FOREVER: &str = "
WITH RECURSIVE forever(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM forever)
SELECT count(*) FROM forever
";

    #[test]
    fn a_cancel_reaches_a_statement_run_alongside_on_the_profiles_own_connection() {
        // An in-memory database cannot be opened again, so the statement falls
        // back to the connection it was asked on.
        let connection = crate::db::Connection::Sqlite(memory(ACCOUNTS));
        let cancel = crate::db::CancelToken::alongside();
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                connection.cancel(&cancel).expect("cancelling cannot fail");
            });
            connection.query_alongside(FOREVER, &cancel, Fetch::default(), false)
        })
        .expect_err("the statement should be interrupted");
        assert!(
            crate::db::is_cancel(crate::db::Target::built_in(Engine::Sqlite), &error),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_cancel_that_found_a_statement_on_a_shared_session_is_sent_before_the_statement_lets_go() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let connection = crate::db::Connection::Sqlite(memory(""));
        let cancel = crate::db::CancelToken::alongside();
        let sent = AtomicBool::new(false);
        std::thread::scope(|scope| {
            crate::db::running_on(&cancel, &connection, |held, let_go| {
                assert!(held());
                let (found, finding) = std::sync::mpsc::channel();
                let (cancel, sent) = (&cancel, &sent);
                let sender = scope.spawn(move || {
                    cancel.stop_alongside(|alongside| {
                        found
                            .send(alongside.len())
                            .expect("the statement is listening");
                        // Long enough for a statement that did not wait to
                        // have let go and its session to have been taken.
                        std::thread::sleep(Duration::from_millis(100));
                        sent.store(true, Ordering::SeqCst);
                    })
                });
                assert_eq!(
                    finding.recv().ok(),
                    Some(1),
                    "the Cancel found the statement"
                );
                let_go();
                assert!(
                    sent.load(Ordering::SeqCst),
                    "the Cancel reached the session while the statement still held it"
                );
                assert!(sender.join().is_ok());
            });
        });
        assert_eq!(
            cancel.stop_alongside(|alongside| alongside.len()),
            Some(0),
            "a later Cancel reaches nothing"
        );
    }

    #[test]
    fn a_cancel_while_the_fallback_waits_drops_it_and_spares_the_statement_ahead() {
        let connection = crate::db::Connection::Sqlite(memory(""));
        let cancel = crate::db::CancelToken::alongside();
        let counted = "
WITH RECURSIVE counting(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counting WHERE n < 10000000)
SELECT count(*) FROM counting
";
        let (ahead, lane) = std::thread::scope(|scope| {
            let ahead = scope.spawn(|| {
                connection.query(
                    counted,
                    &crate::db::CancelToken::default(),
                    Fetch::default(),
                )
            });
            std::thread::sleep(Duration::from_millis(100));
            let lane = scope
                .spawn(|| connection.query_alongside("SELECT 1", &cancel, Fetch::default(), false));
            std::thread::sleep(Duration::from_millis(100));
            connection.cancel(&cancel).expect("cancelling cannot fail");
            (ahead.join().unwrap(), lane.join().unwrap())
        });
        let ahead = ahead.expect("the statement ahead should not be stopped");
        assert_eq!(ahead.rows, vec![vec![Some("10000000".to_string())]]);
        let lane = lane.expect_err("the waiting statement should be dropped");
        assert_eq!(lane.message, "Cancelled before it was sent.");
    }

    #[test]
    fn a_schema_graph_reads_keys_through_the_pragma_functions() {
        let graph = memory(graph_fixture::GRAPH_FIXTURE)
            .schema_graph("main")
            .expect("the graph should read");
        graph_fixture::assert_graph_fixture(&graph);
    }

    #[test]
    fn a_without_rowid_primary_key_is_read_once() {
        let graph = memory(
            "CREATE TABLE kept (a INT NOT NULL, b INT NOT NULL, PRIMARY KEY (a, b)) WITHOUT ROWID;
             CREATE TABLE alone (a INT NOT NULL PRIMARY KEY) WITHOUT ROWID;",
        )
        .schema_graph("main")
        .expect("the graph should read");
        let kept = crate::db::graph::seed::column(&graph, "kept", "a");
        assert!(kept.primary && !kept.unique);
        assert!(crate::db::graph::seed::column(&graph, "alone", "a").unique);
    }

    #[test]
    fn a_graph_reads_a_descending_integer_key_as_nullable_and_a_key_beside_an_index_named_alike() {
        let graph = memory(graph_fixture::EDGE_FIXTURE)
            .schema_graph("main")
            .expect("the graph should read");
        graph_fixture::assert_edge_fixture(&graph);
    }

    #[test]
    fn a_key_on_a_generated_column_survives_a_difference_in_case() {
        let graph = memory(
            "CREATE TABLE owner (
                 id INTEGER PRIMARY KEY, base TEXT,
                 code TEXT GENERATED ALWAYS AS (lower(base)) VIRTUAL UNIQUE
             );
             CREATE TABLE owned (owner_code TEXT REFERENCES owner (CODE));",
        )
        .schema_graph("main")
        .expect("the graph should read");
        assert_eq!(graph.keys.len(), 1);
    }

    #[test]
    fn a_cancel_from_another_thread_stops_a_statement_and_leaves_the_connection_usable() {
        let connection = memory(ACCOUNTS);
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel().expect("cancelling cannot fail");
        });

        let error = connection
            .query(FOREVER, Fetch::default())
            .expect_err("the statement should be interrupted");
        assert!(error.message.contains("interrupt"), "{}", error.message);

        // The interrupt ends the statement, not the connection. A cancel that
        // left the profile dead would be worse than the runaway.
        let rows = connection
            .query("SELECT count(*) FROM accounts", Fetch::default())
            .expect("the connection should still work")
            .rows;
        assert_eq!(rows, vec![vec![Some("2".to_string())]]);
    }

    #[test]
    fn a_statement_timeout_stops_a_runaway_on_its_own() {
        let connection = memory_with_timeout(ACCOUNTS, 1);
        let error = connection
            .query(FOREVER, Fetch::default())
            .expect_err("the statement should time out");
        assert!(error.message.contains("interrupt"), "{}", error.message);
        assert!(connection.query("SELECT 1", Fetch::default()).is_ok());
    }

    /// Every whole number from 1, for as long as it is stepped.
    const ENDLESS: &str = "
WITH RECURSIVE counting(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counting)
SELECT n FROM counting
";

    fn limited(limit: usize, reads_only: bool) -> Fetch<'static> {
        Fetch {
            limit: Some(limit),
            reads_only,
            ..Fetch::default()
        }
    }

    #[test]
    fn a_row_limit_keeps_the_first_rows_and_counts_the_rest() {
        let connection = memory("");
        let ten = "
WITH RECURSIVE counting(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counting WHERE n < 10)
SELECT n FROM counting
";

        let capped = connection
            .query(ten, limited(3, false))
            .expect("query should succeed");
        assert_eq!(
            capped.rows,
            vec![
                vec![Some("1".to_string())],
                vec![Some("2".to_string())],
                vec![Some("3".to_string())],
            ]
        );
        assert_eq!(capped.capped_from, Some(10));
        assert_eq!(capped.total_rows(), 10);
        assert_eq!(capped.rows_affected, Some(10));
        assert_eq!(capped.stopped, None);

        let whole = connection
            .query(ten, limited(10, true))
            .expect("query should succeed");
        assert_eq!(whole.rows.len(), 10);
        assert_eq!(whole.capped_from, None);
        assert_eq!(whole.stopped, None);
    }

    #[test]
    fn a_fed_query_hands_its_typed_rows_to_the_feed() {
        let connection = memory(ACCOUNTS);
        let feed = crate::db::Feed::default();
        let result = connection
            .query(
                "SELECT id, name FROM accounts ORDER BY id",
                Fetch {
                    limit: Some(1),
                    feed: Some(&feed),
                    ..Fetch::default()
                },
            )
            .expect("query should succeed");
        let fed = feed.take();

        assert!(result.rows.is_empty());
        assert_eq!(result.capped_from, Some(2));
        assert_eq!(fed.rows, vec![vec![Some("1".into()), Some("Ada".into())]]);
        assert!(fed.filled.is_some());
        let fed_types: Vec<_> = fed
            .columns
            .iter()
            .map(|column| column.data_type.as_deref())
            .collect();
        assert_eq!(fed_types, vec![Some("integer"), Some("text")]);
    }

    #[test]
    fn a_read_past_the_limit_is_stopped_and_lets_go_of_the_database() {
        let path = std::env::temp_dir().join(format!("dbdelve-stopped-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let writer = rusqlite::Connection::open(&path).expect("the file should open");
        writer.execute_batch(ACCOUNTS).unwrap();
        // The timeout turns a read that was never stopped into a failure
        // rather than a test that never ends.
        let connection = Connection::wrap(rusqlite::Connection::open(&path).unwrap(), 10);

        let started = Instant::now();
        let result = connection
            .query(ENDLESS, limited(3, true))
            .expect("a stopped read is not an error");

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result.stopped, Some(Stopped::AtLimit));
        assert_eq!(result.capped_from, None);
        assert_eq!(result.rows.len(), 3);
        // A read left open would still hold its shared lock, and this would
        // be refused as busy.
        writer
            .execute("INSERT INTO accounts (id, name) VALUES (3, 'Edsger')", [])
            .expect("the stopped read should have let go of the database");
        drop(connection);
        drop(writer);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_read_inside_the_users_transaction_is_stopped_and_the_transaction_kept() {
        let connection = memory_with_timeout(ACCOUNTS, 10);
        connection.query("BEGIN", Fetch::default()).unwrap();
        connection
            .query(
                "INSERT INTO accounts (id, name) VALUES (3, 'Edsger')",
                Fetch::default(),
            )
            .unwrap();

        let result = connection.query(ENDLESS, limited(3, true)).unwrap();
        assert_eq!(result.stopped, Some(Stopped::AtLimit));

        let count = "SELECT count(*) FROM accounts";
        let inside = connection.query(count, Fetch::default()).unwrap();
        assert_eq!(inside.rows, vec![vec![Some("3".to_string())]]);
        connection.query("ROLLBACK", Fetch::default()).unwrap();
        let after = connection.query(count, Fetch::default()).unwrap();
        assert_eq!(after.rows, vec![vec![Some("2".to_string())]]);
    }

    #[test]
    fn a_write_past_the_limit_runs_to_the_end() {
        let connection = memory("CREATE TABLE counted (n INTEGER)");
        let result = connection
            .query(
                "
INSERT INTO counted
WITH RECURSIVE counting(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counting WHERE n < 100)
SELECT n FROM counting
RETURNING n
",
                limited(3, false),
            )
            .expect("the insert should succeed");

        assert_eq!(result.stopped, None);
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.capped_from, Some(100));
        let count = connection
            .query("SELECT count(*) FROM counted", Fetch::default())
            .unwrap();
        assert_eq!(count.rows, vec![vec![Some("100".to_string())]]);
    }

    /// The database the `live_` tests talk to, seeded from
    /// `dev/sqlite/001-dbdelve-demo.sql`.
    fn live() -> Connection {
        let path = std::env::var("dbdelve_SQLITE_PATH").expect("dbdelve_SQLITE_PATH is required");
        Connection::open(&path, 0).expect("connection should open")
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
                schema: table.map(|_| "main".to_string()),
                table: table.map(str::to_string),
                column: column.map(str::to_string),
                blob: false,
            })
            .collect()
    }

    const ACCOUNTS: &str = "
        CREATE TABLE accounts (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT
        );
        INSERT INTO accounts (id, name, email) VALUES (1, 'Ada', 'ada@example.test');
        INSERT INTO accounts (id, name, email) VALUES (2, 'Grace', NULL);
    ";

    #[test]
    fn the_ddl_is_the_table_then_what_was_made_on_it_and_not_its_neighbours() {
        let connection = memory(&format!(
            "{ACCOUNTS}
             CREATE INDEX accounts_email ON accounts (email);
             CREATE TRIGGER accounts_touch AFTER INSERT ON Accounts BEGIN SELECT 1; END;
             CREATE TABLE other (id INTEGER);
             CREATE INDEX other_id ON other (id);"
        ));

        assert_eq!(
            connection
                .ddl("main", "accounts")
                .expect("the DDL should load"),
            "CREATE TABLE accounts (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT
        );
CREATE INDEX accounts_email ON accounts (email);
CREATE TRIGGER accounts_touch AFTER INSERT ON Accounts BEGIN SELECT 1; END;"
        );
    }

    #[test]
    fn a_url_names_a_file_however_many_slashes_it_uses() {
        for (url, expected) in [
            ("sqlite:///tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite:/tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite://./dev/dbdelve_dev.db", "./dev/dbdelve_dev.db"),
            ("sqlite:./dev/dbdelve_dev.db", "./dev/dbdelve_dev.db"),
            ("file:///tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite:///tmp/my%20dbdelve.db", "/tmp/my dbdelve.db"),
            ("sqlite:///C:/Users/me/dbdelve.db", "C:/Users/me/dbdelve.db"),
            ("sqlite://C:/Users/me/dbdelve.db", "C:/Users/me/dbdelve.db"),
            ("file:///c:/dbdelve.db", "c:/dbdelve.db"),
        ] {
            assert_eq!(path_from_url(url).unwrap(), expected, "{url}");
        }
    }

    #[test]
    fn a_url_naming_no_file_says_so() {
        for url in ["sqlite://", "sqlite:", "sqlite:///"] {
            // The last one decodes to "/", which is a directory and not a
            // database -- but that is `open`'s answer to give, not the parser's.
            let parsed = path_from_url(url);
            assert!(
                parsed.is_err() || parsed.as_deref() == Ok("/"),
                "{url} parsed as {parsed:?}"
            );
        }
        assert!(path_from_url("sqlite:///tmp/%zz.db").is_err());
    }

    #[test]
    fn opening_a_path_that_is_not_there_names_it_and_creates_nothing() {
        let path = std::env::temp_dir().join("dbdelve-absent-database.db");
        let _ = std::fs::remove_file(&path);

        let Err(error) = Connection::open(path.to_str().unwrap(), 0) else {
            panic!("opening a path that is not there must fail");
        };

        assert!(error.message.contains("dbdelve-absent-database.db"));
        assert!(!path.exists(), "opening must not create the file");
    }

    #[test]
    fn every_storage_class_renders_as_something_sqlite_would_accept_back() {
        let connection = memory("");
        let result = connection
            .query(
                "SELECT NULL AS empty,
                        42 AS whole,
                        1.0 AS rounded,
                        2.5 AS fractional,
                        'text' AS words,
                        x'00FF' AS bytes",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(
            result.rows[0],
            vec![
                None,
                Some("42".into()),
                Some("1.0".into()),
                Some("2.5".into()),
                Some("text".into()),
                Some("x'00FF'".into()),
            ]
        );
        // NULL is not counted, and the blob is counted as what it renders to.
        assert_eq!(result.bytes, 2 + 3 + 3 + 4 + 7);
    }

    #[test]
    fn a_declared_type_is_a_tag_and_an_expression_has_none() {
        let connection = memory(ACCOUNTS);
        let result = connection
            .query(
                "SELECT id, name, id + 1 AS next FROM accounts",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "name", "next"]);
        // Lowercased: the DDL said INTEGER, and Postgres would have said int4.
        assert_eq!(types(&result), vec![Some("integer"), Some("text"), None]);
    }

    #[test]
    fn a_multi_statement_selection_keeps_one_result_shape() {
        let connection = memory(ACCOUNTS);

        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d", Fetch::default())
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["d"]);
        assert_eq!(result.rows, vec![vec![Some("4".into())]]);

        // A valid empty result still has to carry its headers.
        let empty = connection
            .query("SELECT id FROM accounts WHERE 0", Fetch::default())
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id"]);
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn a_query_counts_its_rows_and_a_command_counts_what_it_changed() {
        let connection = memory(ACCOUNTS);

        assert_eq!(
            connection
                .query("SELECT id FROM accounts", Fetch::default())
                .unwrap()
                .rows_affected,
            Some(2)
        );
        assert_eq!(
            connection
                .query(
                    "UPDATE accounts SET name = 'Ada L' WHERE id = 1",
                    Fetch::default()
                )
                .unwrap()
                .rows_affected,
            Some(1)
        );
    }

    #[test]
    fn a_statement_the_engine_refuses_reports_the_engines_own_words() {
        let error = memory(ACCOUNTS)
            .query("SELECT * FROM no_such_relation", Fetch::default())
            .unwrap_err();

        assert!(error.message.contains("no_such_relation"), "{error}");
        assert_eq!(error.position, None);
    }

    #[test]
    fn a_bracketed_batch_that_fails_part_way_rolls_back_and_says_so() {
        // The brackets make the batch all-or-nothing; without the rollback they
        // make a third state instead — the write uncommitted but visible to the
        // next statement on the same connection, which is the refresh that
        // renders the failure as a success.
        let connection = memory(ACCOUNTS);
        let error = connection
            .query(
                "BEGIN;\n                 UPDATE accounts SET name = 'Changed' WHERE id = 1;\n                 UPDATE no_such_relation SET name = 'Changed';\n                 COMMIT;", Fetch::default())
            .unwrap_err();

        assert!(error.message.contains("no_such_relation"), "{error}");
        assert!(error.message.contains("rolled back"), "{error}");
        assert!(
            connection
                .connection
                .lock()
                .expect("the connection should not be poisoned")
                .is_autocommit()
        );

        let after = connection
            .query("SELECT name FROM accounts WHERE id = 1", Fetch::default())
            .expect("the connection should still be usable");
        assert_eq!(after.rows, vec![vec![Some("Ada".to_string())]]);
    }

    #[test]
    fn a_single_table_select_is_editable_by_its_primary_key() {
        let result = memory(ACCOUNTS)
            .query("SELECT name, id FROM accounts", Fetch::default())
            .expect("query should succeed");
        let edit = result.edit.expect("accounts has a primary key");

        assert_eq!(edit.schema, "main");
        assert_eq!(edit.table, "accounts");
        assert_eq!(
            edit.columns,
            vec![Some("name".to_string()), Some("id".to_string())]
        );
        assert_eq!(edit.keys, vec![1]);
    }

    #[test]
    fn an_aliased_or_computed_column_reports_what_the_table_calls_it() {
        let result = memory(ACCOUNTS)
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
    fn a_join_an_aggregate_or_a_missing_key_is_not_editable() {
        let connection = memory(&format!(
            "{ACCOUNTS}
             CREATE TABLE locations (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO locations VALUES (1, 'London');"
        ));

        for sql in [
            "SELECT accounts.id, locations.name
             FROM accounts JOIN locations ON locations.id = accounts.id",
            "SELECT name, count(*) FROM accounts GROUP BY name",
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

    #[test]
    fn a_self_join_is_not_editable() {
        // Both sides report the same origin table and column, so `sole_table`
        // sees one table and the key resolves to the first side -- an edit
        // typed on b.name would be written at a's id.
        let result = memory(ACCOUNTS)
            .query(
                "SELECT a.id, a.name, b.name
             FROM accounts a JOIN accounts b ON b.id = a.id",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert!(result.edit.is_none());
    }

    #[test]
    fn a_table_without_a_primary_key_is_matched_by_every_column_or_read_only_with_a_reason() {
        use crate::db::RowMatch;

        let connection = memory(
            "create table sightings (bird text, place text, seen integer, weight real);
             create table pictured (bird text, photo blob);
             create table tagged (bird text, place text,
                 label text generated always as (bird || place) virtual);
             create table measured (bird text, amount numeric);
             create table untyped (bird text, anything);",
        );
        let edit = |sql: &str| connection.query(sql, Fetch::default()).expect("a read");

        let sightings = edit("SELECT * FROM sightings");
        let target = sightings.edit.expect("every column is of a matchable type");
        assert_eq!(target.keys, vec![0, 1, 2, 3]);
        assert_eq!(target.row_match, RowMatch::EveryColumn { fixed: vec![] });
        assert_eq!(sightings.read_only_reason, None);

        let tagged = edit("SELECT * FROM tagged")
            .edit
            .expect("a generated column is matched");
        assert_eq!(tagged.keys, vec![0, 1, 2]);
        assert_eq!(tagged.row_match, RowMatch::EveryColumn { fixed: vec![2] });

        for (sql, column) in [
            ("SELECT * FROM measured", "amount (numeric)"),
            ("SELECT bird FROM measured", "amount (numeric)"),
            ("SELECT * FROM untyped", "anything (no declared type)"),
            ("SELECT * FROM pictured", "photo (BLOB)"),
        ] {
            let read = edit(sql);
            assert!(read.edit.is_none(), "{sql}");
            assert_eq!(
                read.read_only_reason,
                Some(format!(
                    "A table without a primary key is edited by matching every column, \
                     and {column} cannot be matched by its value."
                )),
                "{sql}"
            );
        }

        // A column the result leaves out would be left out of the WHERE.
        let partial = edit("SELECT bird FROM sightings");
        assert!(partial.edit.is_none());
        assert_eq!(
            partial.read_only_reason.as_deref(),
            Some(
                "A table without a primary key is edited by matching every column, \
                 and place is not in the result."
            )
        );
        // A disjoint self-join reads each column once, from two rows: the
        // origins cannot tell the instances apart, so the statement must.
        let disjoint = edit(
            "SELECT x.bird, y.place, x.seen, y.weight FROM sightings x JOIN sightings y \
             ON x.bird = 'a' AND y.bird = 'b'",
        );
        assert!(disjoint.edit.is_none());
        assert_eq!(
            disjoint.read_only_reason.as_deref(),
            Some(crate::sql::Unread::Join.reason())
        );
        let reordered = edit("SELECT weight, bird, seen, place FROM sightings");
        assert!(reordered.edit.is_some(), "a reordered projection is edited");
        // Computed beside real columns, and a self-join, are no table's rows.
        assert!(edit("SELECT *, 1 AS one FROM sightings").edit.is_none());
        assert!(
            edit("SELECT a.bird, a.place, a.seen, a.weight, b.bird FROM sightings a, sightings b")
                .edit
                .is_none()
        );

        let keyed = memory(ACCOUNTS)
            .query("SELECT * FROM accounts", Fetch::default())
            .expect("a read");
        assert_eq!(keyed.edit.expect("keyed").row_match, RowMatch::Key);
        assert_eq!(keyed.read_only_reason, None);
    }

    /// A BLOB stored in a column declared as text renders as `x'…'`, which is
    /// also a text the column can hold: the predicate spelled from it would
    /// name that text row, so a result holding one is not edited by value.
    #[test]
    fn a_blob_in_a_text_column_leaves_a_keyless_result_read_only() {
        let connection = memory(
            "create table marks (c text, label text);
             insert into marks values (x'68', 'old'), ('x''68''', 'old');",
        );
        let blob = connection
            .query(
                "SELECT c, label FROM marks WHERE typeof(c) = 'blob'",
                Fetch::default(),
            )
            .expect("a read");
        assert_eq!(blob.rows[0].get(0), Some("x'68'"));
        assert!(blob.edit.is_none());
        assert_eq!(
            blob.read_only_reason.as_deref(),
            Some(
                "A table without a primary key is edited by matching every column, \
                 and c holds a BLOB, which cannot be matched by its value."
            )
        );
        let text = connection
            .query(
                "SELECT c, label FROM marks WHERE typeof(c) = 'text'",
                Fetch::default(),
            )
            .expect("a read");
        assert!(text.edit.is_some(), "the text row is still edited");
    }

    /// Why a blob makes a keyless table read-only: the every-column predicate
    /// the writer spells from the row as read finds no row by its blob.
    #[test]
    fn a_blob_is_not_found_by_the_predicate_written_from_its_rendering() {
        let connection = memory(
            "create table pictured (bird text, photo blob);
             insert into pictured values ('wren', x'68');",
        );
        let read = connection
            .query("SELECT bird, photo FROM pictured", Fetch::default())
            .expect("a read");
        let rendered: Vec<&str> = (0..2)
            .map(|index| read.rows[0].get(index).expect("a value"))
            .collect();
        assert_eq!(rendered, ["wren", "x'68'"]);
        let types = [
            ("bird".to_string(), "text".to_string()),
            ("photo".to_string(), "blob".to_string()),
        ];
        let delete = crate::sql::delete_row_matching(
            Engine::Sqlite,
            "main",
            "pictured",
            &[("bird", rendered[0]), ("photo", rendered[1])],
            &[],
            &types,
        )
        .expect("the predicate is written");
        assert!(crate::sql::is_generated_write_on(Engine::Sqlite, &delete));

        let deleted = connection
            .query(&delete, Fetch::default())
            .expect("the delete runs");
        assert_eq!(deleted.rows_affected, Some(0), "{delete}");
    }

    #[test]
    fn a_composite_primary_key_reports_every_column_it_is_made_of() {
        let edit = memory(
            "CREATE TABLE composite (
                left_id INTEGER,
                right_id INTEGER,
                label TEXT,
                PRIMARY KEY (left_id, right_id)
            );",
        )
        .query(
            "SELECT label, right_id, left_id FROM composite",
            Fetch::default(),
        )
        .expect("query should succeed")
        .edit
        .expect("both key columns are in the result set");

        assert_eq!(edit.table, "composite");
        // Result positions, in the key's own column order.
        assert_eq!(edit.keys, vec![2, 1]);
    }

    #[test]
    fn half_a_composite_key_is_not_enough() {
        assert!(
            memory(
                "CREATE TABLE composite (
                    left_id INTEGER,
                    right_id INTEGER,
                    label TEXT,
                    PRIMARY KEY (left_id, right_id)
                );",
            )
            .query("SELECT label, left_id FROM composite", Fetch::default())
            .expect("query should succeed")
            .edit
            .is_none()
        );
    }

    #[test]
    fn sole_table_needs_one_table_and_at_least_one() {
        assert_eq!(
            sole_table(&probed(&[(Some("accounts"), Some("id")), (None, None),])),
            Some(("main".into(), "accounts".into()))
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
    fn the_catalog_lists_tables_and_views_and_no_routines() {
        let catalog = memory(&format!(
            "{ACCOUNTS} CREATE VIEW account_overview AS SELECT id, name FROM accounts;"
        ))
        .catalog()
        .expect("catalog should load");

        let main = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "main")
            .expect("main should exist");

        assert!(
            main.relations
                .iter()
                .any(|relation| relation.name == "accounts" && relation.kind == RelationKind::Table)
        );
        assert!(
            main.relations
                .iter()
                .any(|relation| relation.name == "account_overview"
                    && relation.kind == RelationKind::View)
        );
        // SQLite has none, and that is an answer rather than a gap.
        assert!(main.routines.is_empty());
    }

    #[test]
    fn the_catalog_hides_sqlites_own_tables() {
        let catalog = memory("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT);")
            .catalog()
            .expect("catalog should load");

        assert!(
            !catalog.schemas.iter().any(|schema| schema
                .relations
                .iter()
                .any(|relation| relation.name.starts_with("sqlite_"))),
            "AUTOINCREMENT creates sqlite_sequence, which is not the user's table"
        );
    }

    #[test]
    fn structure_reads_columns_nullability_and_defaults() {
        let structure = memory(
            "CREATE TABLE t (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL DEFAULT 'none',
                note TEXT
            );",
        )
        .structure("main", "t")
        .expect("structure should load");

        assert_eq!(
            structure.columns,
            vec![
                // Not nullable, though the pragma reports it as such: this is
                // the rowid alias, and the query knows it.
                ColumnDefinition {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    nullable: false,
                    default: None,
                },
                ColumnDefinition {
                    name: "label".into(),
                    data_type: "TEXT".into(),
                    nullable: false,
                    default: Some("'none'".into()),
                },
                ColumnDefinition {
                    name: "note".into(),
                    data_type: "TEXT".into(),
                    nullable: true,
                    default: None,
                },
            ]
        );
    }

    #[test]
    fn a_descending_integer_primary_key_is_not_a_rowid_alias_and_stays_nullable() {
        let structure = memory("CREATE TABLE t (id INTEGER PRIMARY KEY DESC);")
            .structure("main", "t")
            .expect("structure should load");
        assert!(structure.columns[0].nullable);
    }

    #[test]
    fn a_generated_column_is_listed_with_its_expression_and_left_out_of_inserts() {
        let structure = memory(
            "CREATE TABLE test (
                id INTEGER PRIMARY KEY NOT NULL,
                id_doubled INTEGER AS (
                    id * 2
                ) STORED,
                label TEXT GENERATED ALWAYS AS ('#' || id),
                body TEXT
            );",
        )
        .structure("main", "test")
        .expect("structure should load");

        let defaults: Vec<_> = structure
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.default.as_deref()))
            .collect();
        assert_eq!(
            defaults,
            [
                ("id", None),
                ("id_doubled", Some("GENERATED ALWAYS AS (id * 2) STORED")),
                ("label", Some("GENERATED ALWAYS AS ('#' || id) VIRTUAL")),
                ("body", None),
            ]
        );
        assert_eq!(
            crate::sql::insert_template(Engine::Sqlite, "main", "test", &structure.columns)
                .as_deref(),
            Some("INSERT INTO \"main\".\"test\" (\"id\", \"body\") VALUES (?, ?);")
        );
    }

    #[test]
    fn a_written_index_is_read_past_its_quotes_and_comments() {
        assert_eq!(
            written_index(
                "CREATE INDEX i ON t (/* ) */ lower(a), \"b)\" -- ,\n) WHERE a IS NOT NULL"
            ),
            (vec!["lower(a)", "\"b)\""], Some("a IS NOT NULL"))
        );
        assert_eq!(
            written_index("CREATE INDEX i ON t(a) /* why */ WHERE a > 0"),
            (vec!["a"], Some("a > 0"))
        );
        assert_eq!(
            written_index("CREATE INDEX i ON t(a, 'x,y')"),
            (vec!["a", "'x,y'"], None)
        );
    }

    #[test]
    fn structure_reconstructs_what_sqlite_will_not_state() {
        let structure = memory(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (
                 a INTEGER,
                 b TEXT,
                 parent_id INTEGER REFERENCES parent(id),
                 PRIMARY KEY (a, b)
             );
             CREATE INDEX child_b ON child(b);
             CREATE UNIQUE INDEX child_lower ON child (lower(b), \"a\" DESC)
                 WHERE coalesce(b, 'x, (y') <> '';",
        )
        .structure("main", "child")
        .expect("structure should load");

        // `a` is INTEGER and part of the primary key, but the key has two
        // columns, so it is not the rowid and SQLite really will store a null
        // in it.
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "a" && column.nullable)
        );

        // The index the user wrote keeps its own DDL.
        assert!(
            structure.indexes.iter().any(
                |index| index.name == "child_b" && index.definition.starts_with("CREATE INDEX")
            )
        );
        // The one SQLite made for the primary key has no DDL anywhere, so it is
        // rebuilt from the columns it covers.
        assert!(
            structure
                .indexes
                .iter()
                .any(|index| index.name.starts_with("sqlite_autoindex")
                    && index.kind == IndexKind::Primary
                    && index.columns == ["a", "b"]
                    && index.definition == "UNIQUE (\"a\", \"b\")"),
            "{:?}",
            structure.indexes
        );
        // An expression and a partial index's condition are read out of the
        // `CREATE INDEX`, which is the only place SQLite keeps either.
        let lower = structure
            .indexes
            .iter()
            .find(|index| index.name == "child_lower")
            .expect("child_lower is listed");
        assert_eq!(lower.kind, IndexKind::Unique);
        assert_eq!(lower.columns, ["lower(b)", "a DESC"]);
        assert_eq!(
            lower.predicate.as_deref(),
            Some("coalesce(b, 'x, (y') <> ''")
        );

        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition == "PRIMARY KEY (\"a\", \"b\")")
        );
        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition
                    == "FOREIGN KEY (\"parent_id\") REFERENCES \"parent\" (\"id\")"),
            "{:?}",
            structure.constraints
        );
    }

    #[test]
    fn a_relation_lists_the_single_column_keys_that_point_at_it() {
        let connection = memory(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, UNIQUE (a, b));
             CREATE TABLE named (parent_id INTEGER REFERENCES parent (id));
             CREATE TABLE implicit (parent_id INTEGER REFERENCES parent);
             CREATE TABLE pair (x INTEGER, y TEXT, FOREIGN KEY (x, y) REFERENCES parent (a, b));
             CREATE TABLE unrelated (id INTEGER PRIMARY KEY);",
        );

        let reference = |table: &str| Reference {
            schema: "main".into(),
            table: table.into(),
            column: "parent_id".into(),
            referenced_column: "id".into(),
        };
        assert_eq!(
            connection.references("main", "parent").expect("references"),
            vec![reference("implicit"), reference("named")]
        );
        assert!(
            connection
                .references("main", "unrelated")
                .expect("references")
                .is_empty()
        );
    }

    #[test]
    fn a_key_that_does_not_name_its_target_borrows_the_parents_primary_key() {
        let connection = memory(
            "CREATE TABLE parent (a INTEGER, b TEXT, PRIMARY KEY (a, b));
             CREATE TABLE child (
                 x INTEGER,
                 y TEXT,
                 FOREIGN KEY (x, y) REFERENCES parent
             );",
        );

        assert_eq!(
            connection
                .structure("main", "child")
                .expect("structure should load")
                .foreign_keys,
            vec![
                ForeignKey {
                    column: "x".into(),
                    referenced_schema: "main".into(),
                    referenced_table: "parent".into(),
                    referenced_column: "a".into(),
                },
                ForeignKey {
                    column: "y".into(),
                    referenced_schema: "main".into(),
                    referenced_table: "parent".into(),
                    referenced_column: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn a_key_whose_parent_states_no_key_is_skipped_rather_than_guessed() {
        let connection = memory(
            "CREATE TABLE parent (id INTEGER);
             CREATE TABLE child (parent_id INTEGER REFERENCES parent);",
        );

        assert!(
            connection
                .structure("main", "child")
                .expect("structure should load")
                .foreign_keys
                .is_empty()
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_the_development_database_is_fully_seeded() {
        // The seed is applied by a tool outside this test suite, and a seed that
        // half-applied still leaves something to connect to -- the MySQL
        // container reports itself healthy either way. So the volume table is
        // checked by count rather than assumed.
        let result = live()
            .query(
                "SELECT count(*) AS rows_seeded FROM measurements",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(result.rows[0].get(0), Some("5000"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_reads_run_at_once_take_the_time_of_one() {
        super::super::at_once::reads_overlap(
            &super::super::Connection::Sqlite(live()),
            Engine::Sqlite,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3000000)
             SELECT count(*) FROM n",
            "INSERT INTO accounts DEFAULT VALUES",
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_cancel_stops_every_read_run_at_once() {
        super::super::at_once::a_cancel_stops_them_all(
            &super::super::Connection::Sqlite(live()),
            Engine::Sqlite,
            FOREVER,
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_query_round_trip() {
        let result = live()
            .query(
                "SELECT id, name FROM accounts ORDER BY id",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "name"]);
        assert!(!result.rows.is_empty());
        assert_eq!(types(&result), vec![Some("integer"), Some("text")]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_catalog_round_trip() {
        let catalog = live().catalog().expect("catalog should load");
        let main = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "main")
            .expect("main schema should exist");

        assert!(
            main.relations
                .iter()
                .any(|relation| relation.name == "accounts" && relation.kind == RelationKind::Table)
        );
        assert!(
            main.relations
                .iter()
                .any(|relation| relation.name == "account_overview"
                    && relation.kind == RelationKind::View)
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_structure_round_trip() {
        let structure = live()
            .structure("main", "accounts")
            .expect("structure should load");

        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "id" && !column.nullable)
        );
        assert!(
            structure
                .columns
                .iter()
                .any(|column| column.name == "email")
        );
        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition.starts_with("PRIMARY KEY"))
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_single_table_select_is_editable_by_its_primary_key() {
        let edit = live()
            .query("SELECT name, id FROM accounts", Fetch::default())
            .expect("query should succeed")
            .edit
            .expect("accounts has a primary key");

        assert_eq!(edit.schema, "main");
        assert_eq!(edit.table, "accounts");
        assert_eq!(edit.keys, vec![1]);
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_the_foreign_key_fixtures_are_seeded() {
        // The foreign-key tests have nothing to read unless the reseed that
        // added `orders` and `order_items` has actually been applied. There is
        // no cross-schema count to check here: a SQLite foreign key cannot
        // reference an attached database.
        let result = live()
            .query(
                "SELECT count(*) AS rows_seeded FROM order_items",
                Fetch::default(),
            )
            .expect("query should succeed");

        assert_eq!(result.rows[0].get(0), Some("3"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_single_column_foreign_key_names_its_parent() {
        let structure = live()
            .structure("main", "orders")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![ForeignKey {
                column: "account_id".into(),
                referenced_schema: "main".into(),
                referenced_table: "accounts".into(),
                referenced_column: "id".into(),
            }]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_composite_foreign_key_is_one_entry_per_column_in_key_order() {
        let structure = live()
            .structure("main", "order_items")
            .expect("structure should load");

        assert_eq!(
            structure.foreign_keys,
            vec![
                ForeignKey {
                    column: "order_account_id".into(),
                    referenced_schema: "main".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "account_id".into(),
                },
                ForeignKey {
                    column: "order_number".into(),
                    referenced_schema: "main".into(),
                    referenced_table: "orders".into(),
                    referenced_column: "number".into(),
                },
            ]
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_schema_graph_reads_the_seeds_keys_back() {
        use crate::db::graph::seed;

        let graph = live().schema_graph("main").expect("the graph should read");

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
            "NUMERIC"
        );
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_a_cancel_on_its_own_connection_stops_a_catalog_statement() {
        // `schema_graph` is three `internal_query` calls, which is what is
        // stopped here: no catalog read of a local file runs long enough to
        // interrupt.
        let side = live().alongside().expect("a side connection should open");
        let canceller = side.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel().expect("cancelling cannot fail");
        });

        let error = side
            .internal_query(FOREVER)
            .expect_err("the statement should be interrupted");
        assert!(error.message.contains("interrupt"), "{}", error.message);
        assert!(side.schema_graph("main").is_ok());
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_the_rendered_foreign_key_survives_beside_the_structured_one() {
        let structure = live()
            .structure("main", "order_items")
            .expect("structure should load");

        assert!(
            structure
                .constraints
                .iter()
                .any(|constraint| constraint.definition
                    == "FOREIGN KEY (\"order_account_id\", \"order_number\") \
                    REFERENCES \"orders\" (\"account_id\", \"number\")"),
            "{:?}",
            structure.constraints
        );
    }

    fn conformance_subject() -> crate::db::conformance::Subject {
        use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
        let path = std::env::var("dbdelve_SQLITE_PATH").expect("dbdelve_SQLITE_PATH is required");
        Subject {
            connection: crate::db::Connection::open(crate::db::ConnectionConfig::Sqlite {
                path,
                statement_timeout: 0,
            })
            .expect("connection should open"),
            target: crate::db::Target::built_in(Engine::Sqlite),
            schema: "main",
            relation: "conformance",
            key: "id",
            columns: &COLUMNS,
            read: sql_read(Engine::Sqlite, "main"),
            endless_rows: None,
            endless: "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
                      SELECT i FROM n",
            runaway: FOREVER,
            read_only_refusal: "",
            fixture: Some(Fixture {
                types: &[
                    "integer", "text", "integer", "numeric", "real", "boolean", "date", "datetime",
                    "blob",
                ],
                renders: &[
                    ("fraction", 0, "12.5"),
                    ("bytes", 0, "x'00FF'"),
                    ("bytes", 1, "x'DEADBEEF'"),
                ],
                find: Box::new(|key| sql_find(Engine::Sqlite, "main", key)),
            }),
            _guard: None,
        }
    }

    crate::db::conformance::suite!(
        live_conformance,
        "requires the SQLite development database configured through dbdelve_SQLITE_PATH",
        conformance_subject:
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
    fn binary_edits_round_trip() {
        crate::db::conformance::binary_edits_round_trip(
            &crate::db::Connection::Sqlite(memory("")),
            crate::db::Target::built_in(Engine::Sqlite),
            "main",
            "blob",
        );
    }
}

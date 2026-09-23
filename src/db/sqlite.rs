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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, InterruptHandle, OpenFlags};

use super::{
    Catalog, Cell, Column, DbError, EditTarget, Engine, ForeignKey, NamedDefinition, QueryResult,
    Structure, assemble_catalog, assemble_structure, non_utf8_error, percent_decoded, plain_error,
    required_cell,
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
    let rest = url
        .split_once(':')
        .map(|(_, rest)| rest)
        .ok_or_else(|| "Connection URL does not name a database file.".to_string())?;
    let path = rest.strip_prefix("//").unwrap_or(rest);
    let decoded = percent_decoded(path)?;

    if decoded.is_empty() {
        return Err("Connection URL does not name a database file.".into());
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
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        // Deliberately no `SQLITE_OPEN_CREATE`. With it, a mistyped path is an
        // empty database that opens successfully and then reports an empty
        // catalog, which reads as "this database has nothing in it" rather than
        // "this is not the file you meant". dbdelve would also have littered a
        // file onto the user's disk to tell them so.
        //
        // `SQLITE_OPEN_URI` is off for the same reason: the path came out of the
        // URL already, and leaving URI parsing on would make a path containing
        // `?` mean something other than itself.
        if !Path::new(path).exists() {
            return Err(plain_error(format!("No database file at {path}")));
        }

        let connection = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| plain_error(format!("Cannot open {path}: {}", describe(&error))))?;

        Ok(Self::wrap(connection, statement_timeout))
    }

    fn wrap(connection: rusqlite::Connection, statement_timeout: u32) -> Self {
        Self {
            interrupt: Arc::new(connection.get_interrupt_handle()),
            statement_timeout: (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout))),
            connection: Arc::new(Mutex::new(connection)),
        }
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
        // Armed before the lock rather than after it, so time spent queued
        // behind another statement on this connection counts against the limit
        // too. Dropped at the end of this call, whichever way it leaves.
        let _deadline = self.deadline();
        let connection = self.connection.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;

        let mut result = QueryResult::default();
        let mut probed = Vec::new();

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
            let mut batch = Batch::new(&connection, sql);

            // Timed from here, not from the call: one connection serialises a
            // profile's queries, and time spent waiting behind the catalog load
            // is not time spent on this statement.
            let started = Instant::now();
            while let Some(mut statement) =
                batch.next().map_err(|error| query_error(&error, sql))?
            {
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
                let described: Vec<ProbedColumn> = statement
                    .columns_with_metadata()
                    .iter()
                    .map(|column| ProbedColumn {
                        schema: column.database_name().map(str::to_string),
                        table: column.table_name().map(str::to_string),
                        column: column.origin_name().map(str::to_string),
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
                // over and the last statement wins.
                let mut rows = Vec::new();
                let mut bytes = 0;
                let mut returned = statement
                    .query([])
                    .map_err(|error| query_error(&error, sql))?;
                while let Some(row) = returned.next().map_err(|error| query_error(&error, sql))? {
                    let cells: Vec<Cell> = (0..columns.len())
                        .map(|index| {
                            let value = row
                                .get_ref(index)
                                .map_err(|error| query_error(&error, sql))?;
                            render(value, &columns, index)
                        })
                        .collect::<Result<_, _>>()?;

                    bytes += cells
                        .iter()
                        .filter_map(|cell| cell.as_ref().map(String::len))
                        .sum::<usize>();
                    rows.push(cells);
                }

                // A query's count is the rows it returned. `changes()` would
                // report whatever the statement before it modified, since
                // SQLite does not reset it for a select.
                result.rows_affected = Some(rows.len() as u64);
                result.columns = columns;
                result.rows = rows;
                result.bytes = bytes;
                probed = described;
            }
            result.elapsed = started.elapsed();
            Ok(())
        })();

        if let Err(error) = outcome {
            return Err(rolled_back(&connection, outside_a_transaction, error));
        }

        // The mutex is not reentrant and `edit_target` runs a pragma through it,
        // so the lock has to go before the question is asked.
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
    /// pragma that refused must not turn that success into an error.
    fn edit_target(&self, probed: &[ProbedColumn]) -> Option<EditTarget> {
        let (schema, table) = sole_table(probed)?;
        // ponytail: one pragma per result set, no cache. A map from table to key
        // held on the connection is the upgrade path if the trip shows up in
        // query timings.
        let key = self.primary_key(&schema, &table).ok()?;
        resolve_edit_target(probed, &schema, &table, &key)
    }

    /// The table's primary key columns, in key order.
    ///
    /// Empty for a table that declares none. Such a table still has a rowid
    /// that would identify a row, but the rowid is not in the result set unless
    /// the user selected it, and a predicate dbdelve cannot read off the grid is
    /// a predicate it must not write.
    fn primary_key(&self, schema: &str, table: &str) -> Result<Vec<String>, DbError> {
        let result = self.internal_query(&format!(
            "SELECT name AS column_name
             FROM pragma_table_info({table}, {schema})
             WHERE pk > 0
             ORDER BY pk",
            table = Engine::Sqlite.quote_literal(table),
            schema = Engine::Sqlite.quote_literal(schema),
        ))?;

        column_of(&result, "column_name")
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let schemas = self.schemas()?;
        if schemas.is_empty() {
            return Ok(Catalog::default());
        }

        let relations = self.internal_query(&relations_sql(&schemas))?;
        assemble_catalog(relations, QueryResult::default())
    }

    /// SQLite has no stored functions or procedures at all, so an empty list is
    /// the true answer rather than a gap in what dbdelve can see -- and there is
    /// nothing to ask the file for.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    /// `main`, `temp`, and anything `ATTACH`ed — under SQLite's own names for
    /// them, which is what the user typed if they attached it.
    fn schemas(&self) -> Result<Vec<String>, DbError> {
        let result = self
            .internal_query("SELECT name AS schema_name FROM pragma_database_list ORDER BY seq")?;

        column_of(&result, "schema_name")
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let columns = self.internal_query(&format!(
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
                                 FROM pragma_table_info({relation}, {schema})
                                 WHERE pk > 0
                             ) = 1
                            THEN 'no'
                        ELSE 'yes'
                    END AS nullable,
                    COALESCE(dflt_value, '') AS column_default
             FROM pragma_table_info({relation}, {schema})
             ORDER BY cid",
            relation = Engine::Sqlite.quote_literal(relation),
            schema = Engine::Sqlite.quote_literal(schema),
        ))?;

        // The shared assembler reads the columns; SQLite has no catalog of
        // indexes or constraints to hand it, so those two are built below out of
        // pragmas rather than queried as definitions.
        let mut structure =
            assemble_structure(columns, QueryResult::default(), QueryResult::default())?;
        structure.indexes = self.indexes(schema, relation)?;
        structure.constraints = self.constraints(schema, relation)?;
        structure.foreign_keys = self.foreign_keys(schema, relation)?;
        Ok(structure)
    }

    /// An index the user wrote has its `CREATE INDEX` in `sqlite_master`. One
    /// SQLite created to enforce a `UNIQUE` or `PRIMARY KEY` has no row there at
    /// all, so its definition is reconstructed from the columns it covers.
    fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<NamedDefinition>, DbError> {
        let listed = self.internal_query(&format!(
            "SELECT index_list.name AS object_name,
                    index_list.\"unique\" AS is_unique,
                    COALESCE(sqlite_master.sql, '') AS definition
             FROM pragma_index_list({relation}, {schema}) AS index_list
             LEFT JOIN {schema_identifier}.sqlite_master
                    ON sqlite_master.type = 'index'
                   AND sqlite_master.name = index_list.name
             ORDER BY index_list.name",
            relation = Engine::Sqlite.quote_literal(relation),
            schema = Engine::Sqlite.quote_literal(schema),
            schema_identifier = Engine::Sqlite.quote_identifier(schema),
        ))?;

        let mut indexes = Vec::new();
        for row in &listed.rows {
            let name = required_cell(&listed, row, "object_name")?.to_string();
            let definition = required_cell(&listed, row, "definition")?;
            let definition = if definition.is_empty() {
                // ponytail: one pragma per constraint-owned index. These are
                // few, and only for a relation the user has opened; batch them
                // into one correlated query if opening a wide table drags.
                let unique = required_cell(&listed, row, "is_unique")? != "0";
                let covered = self.index_columns(schema, &name)?;
                format!(
                    "{} ({})",
                    if unique { "UNIQUE" } else { "INDEX" },
                    quoted_list(&covered)
                )
            } else {
                definition.to_string()
            };
            indexes.push(NamedDefinition { name, definition });
        }

        Ok(indexes)
    }

    fn index_columns(&self, schema: &str, index: &str) -> Result<Vec<String>, DbError> {
        let result = self.internal_query(&format!(
            "SELECT name AS column_name
             FROM pragma_index_info({index}, {schema})
             ORDER BY seqno",
            index = Engine::Sqlite.quote_literal(index),
            schema = Engine::Sqlite.quote_literal(schema),
        ))?;

        column_of(&result, "column_name")
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

        let listed = self.internal_query(&format!(
            "SELECT id AS constraint_id,
                    \"table\" AS target_relation,
                    \"from\" AS source_column,
                    COALESCE(\"to\", '') AS target_column
             FROM pragma_foreign_key_list({relation}, {schema})
             ORDER BY id, seq",
            relation = Engine::Sqlite.quote_literal(relation),
            schema = Engine::Sqlite.quote_literal(schema),
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
        let listed = self.internal_query(&format!(
            "SELECT id AS constraint_id,
                    \"table\" AS target_relation,
                    \"from\" AS source_column,
                    COALESCE(\"to\", '') AS target_column
             FROM pragma_foreign_key_list({relation}, {schema})
             ORDER BY id, seq",
            relation = Engine::Sqlite.quote_literal(relation),
            schema = Engine::Sqlite.quote_literal(schema),
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

/// One `sqlite_master` scan per schema, aliased to the column names
/// [`assemble_catalog`] reads rather than to SQLite's own.
fn relations_sql(schemas: &[String]) -> String {
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
                   AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'",
                schema_literal = Engine::Sqlite.quote_literal(schema),
                schema_identifier = Engine::Sqlite.quote_identifier(schema),
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
fn render(value: ValueRef<'_>, columns: &[Column], index: usize) -> Result<Cell, DbError> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Integer(integer) => Some(integer.to_string()),
        ValueRef::Real(real) => Some(render_real(real)),
        // A database whose text was written as something other than UTF-8 can
        // return bytes that are not text at all. Postgres raises the same error
        // for the same reason, naming the column rather than the row.
        ValueRef::Text(bytes) => Some(
            std::str::from_utf8(bytes)
                .map_err(|_| non_utf8_error(columns, index))?
                .to_string(),
        ),
        // SQLite's own literal syntax for a blob, rather than bare hex, so a
        // value copied out of the grid is a value that can be pasted into a
        // statement.
        ValueRef::Blob(bytes) => Some(format!("x'{}'", hex::encode_upper(bytes))),
    })
}

/// A real keeps its point even when it is whole. Rust renders `1.0` as `1`,
/// which in a SQLite grid is the rendering of an integer, and the difference
/// between the two storage classes is one the user is entitled to see.
fn render_real(real: f64) -> String {
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

    /// Runs until something stops it. The recursion has no termination
    /// condition, which is the shape of the runaway both of these are about.
    const FOREVER: &str = "
WITH RECURSIVE forever(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM forever)
SELECT count(*) FROM forever
";

    #[test]
    fn a_cancel_from_another_thread_stops_a_statement_and_leaves_the_connection_usable() {
        let connection = memory(ACCOUNTS);
        let canceller = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel().expect("cancelling cannot fail");
        });

        let error = connection
            .query(FOREVER)
            .expect_err("the statement should be interrupted");
        assert!(error.message.contains("interrupt"), "{}", error.message);

        // The interrupt ends the statement, not the connection. A cancel that
        // left the profile dead would be worse than the runaway.
        let rows = connection
            .query("SELECT count(*) FROM accounts")
            .expect("the connection should still work")
            .rows;
        assert_eq!(rows, vec![vec![Some("2".to_string())]]);
    }

    #[test]
    fn a_statement_timeout_stops_a_runaway_on_its_own() {
        let connection = memory_with_timeout(ACCOUNTS, 1);
        let error = connection
            .query(FOREVER)
            .expect_err("the statement should time out");
        assert!(error.message.contains("interrupt"), "{}", error.message);
        assert!(connection.query("SELECT 1").is_ok());
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
    fn a_url_names_a_file_however_many_slashes_it_uses() {
        for (url, expected) in [
            ("sqlite:///tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite:/tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite://./dev/dbdelve_dev.db", "./dev/dbdelve_dev.db"),
            ("sqlite:./dev/dbdelve_dev.db", "./dev/dbdelve_dev.db"),
            ("file:///tmp/dbdelve.db", "/tmp/dbdelve.db"),
            ("sqlite:///tmp/my%20dbdelve.db", "/tmp/my dbdelve.db"),
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
            .query("SELECT id, name, id + 1 AS next FROM accounts")
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["id", "name", "next"]);
        // Lowercased: the DDL said INTEGER, and Postgres would have said int4.
        assert_eq!(types(&result), vec![Some("integer"), Some("text"), None]);
    }

    #[test]
    fn a_multi_statement_selection_keeps_one_result_shape() {
        let connection = memory(ACCOUNTS);

        let result = connection
            .query("SELECT 1 AS a, 2 AS b; SELECT 4 AS d")
            .expect("query should succeed");

        assert_eq!(names(&result), vec!["d"]);
        assert_eq!(result.rows, vec![vec![Some("4".into())]]);

        // A valid empty result still has to carry its headers.
        let empty = connection
            .query("SELECT id FROM accounts WHERE 0")
            .expect("query should succeed");
        assert_eq!(names(&empty), vec!["id"]);
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn a_query_counts_its_rows_and_a_command_counts_what_it_changed() {
        let connection = memory(ACCOUNTS);

        assert_eq!(
            connection
                .query("SELECT id FROM accounts")
                .unwrap()
                .rows_affected,
            Some(2)
        );
        assert_eq!(
            connection
                .query("UPDATE accounts SET name = 'Ada L' WHERE id = 1")
                .unwrap()
                .rows_affected,
            Some(1)
        );
    }

    #[test]
    fn a_statement_the_engine_refuses_reports_the_engines_own_words() {
        let error = memory(ACCOUNTS)
            .query("SELECT * FROM no_such_relation")
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
                "BEGIN;\n                 UPDATE accounts SET name = 'Changed' WHERE id = 1;\n                 UPDATE no_such_relation SET name = 'Changed';\n                 COMMIT;",
            )
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
            .query("SELECT name FROM accounts WHERE id = 1")
            .expect("the connection should still be usable");
        assert_eq!(after.rows, vec![vec![Some("Ada".to_string())]]);
    }

    #[test]
    fn a_single_table_select_is_editable_by_its_primary_key() {
        let result = memory(ACCOUNTS)
            .query("SELECT name, id FROM accounts")
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
            .query("SELECT id AS ident, upper(name) AS shouted, name FROM accounts")
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
                    .query(sql)
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
            )
            .expect("query should succeed");

        assert!(result.edit.is_none());
    }

    #[test]
    fn a_table_without_a_primary_key_is_not_editable() {
        // There is a rowid that would identify the row, but it is not in the
        // result set, so there is no predicate dbdelve can read off the grid.
        let result = memory("CREATE TABLE unkeyed (value INTEGER, label TEXT); INSERT INTO unkeyed VALUES (1, 'a');")
            .query("SELECT value, label FROM unkeyed")
            .expect("query should succeed");

        assert!(result.edit.is_none());
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
        .query("SELECT label, right_id, left_id FROM composite")
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
            .query("SELECT label, left_id FROM composite")
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
    fn structure_reconstructs_what_sqlite_will_not_state() {
        let structure = memory(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (
                 a INTEGER,
                 b TEXT,
                 parent_id INTEGER REFERENCES parent(id),
                 PRIMARY KEY (a, b)
             );
             CREATE INDEX child_b ON child(b);",
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
                    && index.definition == "UNIQUE (\"a\", \"b\")"),
            "{:?}",
            structure.indexes
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
            .query("SELECT count(*) AS rows_seeded FROM measurements")
            .expect("query should succeed");

        assert_eq!(result.rows[0][0].as_deref(), Some("5000"));
    }

    #[test]
    #[ignore = "requires the repository development database configured through dbdelve_SQLITE_PATH"]
    fn live_query_round_trip() {
        let result = live()
            .query("SELECT id, name FROM accounts ORDER BY id")
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
            .query("SELECT name, id FROM accounts")
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
            .query("SELECT count(*) AS rows_seeded FROM order_items")
            .expect("query should succeed");

        assert_eq!(result.rows[0][0].as_deref(), Some("3"));
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
}

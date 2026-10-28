//! The DuckDB boundary.
//!
//! Embedded like SQLite: no server, no credentials, a file or nothing at all.
//! What sets it apart is what it reads besides its own tables. A CSV, a
//! Parquet file, a JSON file and -- through `db::xlsx` -- a spreadsheet are
//! each a `FROM` away, which is what makes a profile with no file, held in
//! memory, a useful thing to have open.
//!
//! Results come back as Arrow batches and every cell is rendered by Arrow's
//! own formatter, so a `LIST`, a `STRUCT` or a `DECIMAL(18,3)` reads the way
//! DuckDB's own shell prints it rather than through a match on every type this
//! file would have to keep up with. The few types Arrow has no word for are
//! spelled out first, by [`printable`].
//!
//! DuckDB does not say which table a result column was read from, so a
//! result is editable only when its statement is a plain select of one keyed
//! table, read off the statement and checked against the catalog
//! ([`edit_target`]).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duckdb::arrow::array::{
    Array, ArrayRef, AsArray, FixedSizeListArray, Int64Array, LargeListArray, ListArray, MapArray,
    StringArray, StructArray,
};
use duckdb::arrow::compute::cast;
use duckdb::arrow::datatypes::{DataType, Field, FieldRef};
use duckdb::arrow::error::ArrowError;
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use duckdb::{Config, InterruptHandle};
use num_bigint::BigUint;

use crate::sql::Projected;

use super::{
    Catalog, Column, Databases, DbError, EditTarget, Engine, Fetch, QueryResult, Reference,
    RowBuilder, Stopped, Structure, Unpicked, assemble_catalog, assemble_foreign_keys,
    assemble_references, assemble_structure, plain_error, required_cell, terminated,
};

// Workaround for duckdb-rs#920, fixed upstream by #923 but unreleased past
// 1.20000.0: the bundled build links its extension autoregister archive after
// the libduckdb-sys rlib, so GNU ld (aarch64 Linux) has already passed the
// archive that defines `duckdb_register_static_extensions` when the reference
// shows up. Referencing it here, ahead of the rlib, pulls the loader in. Drop
// this once a release carries #923.
#[used]
static STATIC_EXTENSION_LOADER: unsafe extern "C" fn() -> std::ffi::c_int = {
    unsafe extern "C" {
        fn duckdb_register_static_extensions() -> std::ffi::c_int;
    }
    duckdb_register_static_extensions
};

/// What an empty path means: a database held in memory, gone when the
/// connection closes. The files it reads stay where they are.
pub const IN_MEMORY: &str = ":memory:";

/// DuckDB's words for a statement its interrupt stopped, whether a Cancel or
/// the statement timeout fired it.
pub const INTERRUPTED: &str = "INTERRUPT Error: Interrupted!";

/// How DuckDB refuses a `ROLLBACK` with no transaction to end.
const NO_TRANSACTION: &str = "no transaction is active";

/// The path out of a `duckdb:` URL, read as `sqlite.rs` reads its own.
/// Nothing after the scheme is a database in memory.
pub fn path_from_url(url: &str) -> Result<String, String> {
    let path = super::sqlite::url_path(url)?;
    Ok(match path.is_empty() {
        true => IN_MEMORY.to_string(),
        false => path,
    })
}

#[derive(Clone)]
pub struct Connection {
    connection: Arc<Mutex<duckdb::Connection>>,
    interrupt: Arc<InterruptHandle>,
    /// Never runs a statement, so [`Self::alongside`] can clone it while the
    /// connection above is busy: a second connection to an in-memory database
    /// has to come from the same instance, since a new one would be empty.
    origin: Arc<Mutex<duckdb::Connection>>,
    statement_timeout: Option<Duration>,
    /// Shared by every clone, so a connection opened later lands where the
    /// latest pick says: one cloned from [`Self::origin`] starts where the
    /// instance does, whatever `USE` the others ran.
    pick: Arc<Mutex<Pick>>,
    /// The database and schema a connection of this instance starts on.
    home: Arc<(String, String)>,
    unpicked: Option<Unpicked>,
}

/// The database and schema unqualified names resolve in, None for where the
/// instance starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pick {
    database: Option<String>,
    schema: Option<String>,
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let connection = match path.trim() {
            "" | IN_MEMORY => duckdb::Connection::open_in_memory_with_flags(config()?)
                .map_err(|error| plain_error(error.to_string()))?,
            path => {
                let path = crate::store::home_expanded(path);
                // Not created when missing, for the reason `sqlite.rs` gives:
                // a mistyped path would open as an empty database and read as
                // one with nothing in it.
                if !path.exists() {
                    return Err(plain_error(format!(
                        "No database file at {}",
                        path.display()
                    )));
                }
                duckdb::Connection::open_with_flags(&path, config()?).map_err(|error| {
                    plain_error(format!("Cannot open {}: {error}", path.display()))
                })?
            }
        };
        // Into the database's catalog, so a connection cloned from this one
        // has it too.
        connection
            .register_table_function::<super::xlsx::ReadXlsx>("read_xlsx")
            .map_err(|error| plain_error(error.to_string()))?;
        let origin = connection
            .try_clone()
            .map_err(|error| plain_error(error.to_string()))?;
        let home = run(
            &connection,
            "SELECT current_database(), current_schema()",
            Fetch::default(),
        )?;
        let home = match home.rows.first() {
            Some(row) => (
                row.get(0).unwrap_or_default().to_string(),
                row.get(1).unwrap_or_default().to_string(),
            ),
            None => return Err(plain_error("DuckDB named no current database.".into())),
        };
        Ok(Self {
            interrupt: connection.interrupt_handle(),
            connection: Arc::new(Mutex::new(connection)),
            origin: Arc::new(Mutex::new(origin)),
            statement_timeout: (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout))),
            pick: Arc::default(),
            home: Arc::new(home),
            unpicked: None,
        })
    }

    /// Move onto the picked database and schema, as a profile opens. One
    /// that is no longer there -- an attached database is gone once the
    /// instance closes -- leaves the connection where it would be without
    /// it, saying so in [`Self::unpicked`], rather than failing the open.
    pub fn opened_on(mut self, database: Option<String>, schema: Option<String>) -> Self {
        let asked = Pick { database, schema };
        if asked == Pick::default() {
            return self;
        }
        let error = match self.go_here(&asked) {
            Ok(()) => {
                *self.pick() = asked;
                return self;
            }
            Err(error) => error,
        };
        // A failed `USE` moves nothing, so whatever is not tried again is
        // where the instance starts.
        let database = Pick {
            schema: None,
            ..asked.clone()
        };
        let landed = match asked.schema.is_some()
            && database != Pick::default()
            && self.go_here(&database).is_ok()
        {
            true => database,
            false => Pick::default(),
        };
        self.unpicked = Some(Unpicked {
            database: landed.database != asked.database,
            message: format!(
                "{} The connection opened on {} instead.",
                error.message,
                self.spelled(&landed)
            ),
        });
        *self.pick() = landed;
        self
    }

    /// A second connection to the same database, for a statement to run on
    /// beside this one's; see [`super::Connection::query_alongside`]. It
    /// starts on the picked database and schema, as this one is.
    pub fn alongside(&self) -> Result<Self, DbError> {
        let connection = lock(&self.origin)?
            .try_clone()
            .map_err(|error| plain_error(error.to_string()))?;
        let pick = self.pick().clone();
        if pick != Pick::default() {
            self.go(&connection, &pick)?;
        }
        Ok(Self {
            interrupt: connection.interrupt_handle(),
            connection: Arc::new(Mutex::new(connection)),
            ..self.clone()
        })
    }

    fn pick(&self) -> std::sync::MutexGuard<'_, Pick> {
        self.pick
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// For every connection opened from here on; [`Self::apply_pick`] moves
    /// this one.
    pub fn set_database(&self, database: Option<&str>) {
        *self.pick() = Pick {
            database: database.map(str::to_string),
            schema: None,
        };
    }

    pub fn set_schema(&self, schema: Option<&str>) {
        self.pick().schema = schema.map(str::to_string);
    }

    /// `USE` the database and schema last picked, waiting for a statement
    /// running here to finish. Not undone by a `ROLLBACK`, and refused inside
    /// a transaction a failure aborted.
    pub fn apply_pick(&self) -> Result<(), DbError> {
        let connection = lock(&self.connection)?;
        // Read only once the connection is held: two switches whose threads
        // take it out of order both apply the later one.
        let pick = self.pick().clone();
        self.go(&connection, &pick)
    }

    fn go_here(&self, pick: &Pick) -> Result<(), DbError> {
        self.go(&*lock(&self.connection)?, pick)
    }

    /// `USE` `pick` on `connection`, naming the database every time: a bare
    /// name could be a database's or a schema's. Nothing picked is where the
    /// instance starts.
    fn go(&self, connection: &duckdb::Connection, pick: &Pick) -> Result<(), DbError> {
        let quote = |name: &str| Engine::DuckDb.quote_identifier(name);
        let database = pick.database.as_deref().unwrap_or(&self.home.0);
        let target = match (&pick.database, &pick.schema) {
            (Some(_), None) => quote(database),
            (_, schema) => format!(
                "{}.{}",
                quote(database),
                quote(schema.as_deref().unwrap_or(&self.home.1))
            ),
        };
        connection
            .execute_batch(&format!("USE {target}"))
            .map_err(query_error)
    }

    /// `pick` as the status bar names it.
    fn spelled(&self, pick: &Pick) -> String {
        let database = pick.database.as_deref().unwrap_or(&self.home.0);
        match &pick.schema {
            Some(schema) => format!("{database}.{schema}"),
            None => database.to_string(),
        }
    }

    pub fn current_database(&self) -> &str {
        &self.home.0
    }

    pub fn current_schema(&self) -> &str {
        &self.home.1
    }

    pub fn unpicked(&self) -> Option<&Unpicked> {
        self.unpicked.as_ref()
    }

    /// The databases attached to the instance, less the two DuckDB keeps for
    /// itself (`system` and `temp`).
    pub fn databases(&self) -> Result<Databases, DbError> {
        let listed = self.internal_query(
            "SELECT database_name, current_database() AS current
             FROM duckdb_databases()
             WHERE NOT internal
             ORDER BY database_name",
        )?;
        let names = listed
            .rows
            .iter()
            .map(|row| required_cell(&listed, row, "database_name").map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        let current = match listed.rows.first() {
            Some(row) => Some(required_cell(&listed, row, "current")?.to_string()),
            None => None,
        };
        Ok(Databases { names, current })
    }

    /// Local and immediate, as SQLite's is: an interrupt on an idle connection
    /// does nothing.
    pub fn cancel(&self) -> Result<(), DbError> {
        self.interrupt.interrupt();
        Ok(())
    }

    /// The statement timeout, the way `sqlite.rs` arms its own: a thread that
    /// interrupts unless this is dropped first.
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

    /// Run the submission verbatim, keeping what `fetch` asks for of the last
    /// statement's rows; see [`super::Connection::query`].
    ///
    /// DuckDB runs every statement of a submission but the last as it
    /// prepares it, and the last one streams: its rows arrive a batch at a
    /// time, so a read is stopped past the limit by fetching no further.
    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        // Armed before the lock, so time queued behind another statement on
        // this connection counts against the limit too.
        let _deadline = self.deadline();
        let connection = lock(&self.connection)?;
        fetch.hold()?;
        let mut result =
            run(&connection, sql, fetch).map_err(|error| rolled_back(&connection, sql, error))?;
        if !result.columns.is_empty() {
            result.edit = edit_target(&connection, sql, &result.columns);
        }
        Ok(result)
    }

    /// dbdelve's own SQL, whose rows are never edited.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let _deadline = self.deadline();
        let connection = lock(&self.connection)?;
        run(&connection, sql, Fetch::default())
            .map_err(|error| rolled_back(&connection, sql, error))
    }

    /// The tables and views of the database the connection opened, less the
    /// catalogs DuckDB keeps for itself.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let relations = self.internal_query(
            "SELECT schema_name, table_name AS relation_name, 'table' AS relation_kind,
                    estimated_size AS row_estimate
             FROM duckdb_tables()
             WHERE NOT internal AND database_name = current_database()
             UNION ALL
             SELECT schema_name, view_name, 'view', NULL
             FROM duckdb_views()
             WHERE NOT internal AND database_name = current_database()
             ORDER BY schema_name, relation_name",
        )?;
        assemble_catalog(relations, QueryResult::default())
    }

    /// DuckDB's macros are functions of a kind, but not ones with a body to
    /// show the way a routine tab does; none are listed.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let (schema_literal, relation_literal) = (
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        );
        let columns = self.internal_query(&format!(
            "SELECT column_name, lower(data_type) AS data_type,
                        CASE WHEN is_nullable THEN 'yes' ELSE 'no' END AS nullable,
                        COALESCE(column_default, '') AS column_default
                 FROM duckdb_columns()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                 ORDER BY column_index"
        ))?;
        // `expressions` is a list printed as text, `[a, 'lower(b)']`, which the
        // cast reads back, quoting and all. DuckDB has no partial index.
        let indexes = self.internal_query(&format!(
            "SELECT index_name AS object_name,
                        CASE WHEN is_primary THEN 'primary'
                             WHEN is_unique THEN 'unique'
                             ELSE 'plain' END AS index_kind,
                        array_to_string(expressions::VARCHAR[], chr(31)) AS index_columns,
                        COALESCE(sql, '') AS definition
                 FROM duckdb_indexes()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                 ORDER BY index_name"
        ))?;
        // Written out of the column lists rather than taken from
        // `constraint_text`, which spells a key `PRIMARY KEY(id)`: the
        // Structure view and `Structure::primary_key` read the shape every
        // other engine gives them, `PRIMARY KEY (id)`.
        let constraints = self.internal_query(&format!(
            "SELECT constraint_name AS object_name,
                        constraint_type || ' ('
                            || array_to_string(constraint_column_names, ', ') || ')'
                            || CASE WHEN constraint_type = 'FOREIGN KEY'
                                    THEN ' REFERENCES ' || referenced_table || ' ('
                                         || array_to_string(referenced_column_names, ', ')
                                         || ')'
                                    ELSE '' END
                            AS definition
                 FROM duckdb_constraints()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                   AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')
                 ORDER BY constraint_index"
        ))?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&self.internal_query(&format!(
            "SELECT unnest(constraint_column_names) AS column_name,
                        schema_name AS referenced_schema,
                        referenced_table,
                        unnest(referenced_column_names) AS referenced_column
                 FROM duckdb_constraints()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                   AND constraint_type = 'FOREIGN KEY'"
        ))?)?;
        Ok(structure)
    }

    /// Who points at this relation. A DuckDB foreign key names its parent by
    /// table alone, in its own schema.
    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        let result = self.internal_query(&format!(
            "SELECT schema_name AS source_schema, table_name AS source_table,
                        unnest(constraint_column_names) AS column_name,
                        unnest(referenced_column_names) AS referenced_column,
                        constraint_name
                 FROM duckdb_constraints()
                 WHERE database_name = current_database()
                   AND constraint_type = 'FOREIGN KEY'
                   AND schema_name = {} AND referenced_table = {}",
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        ))?;
        assemble_references(&result)
    }

    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        let (schema_literal, relation_literal) = (
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        );
        let listed = self.internal_query(&format!(
            "SELECT sql FROM duckdb_tables()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                 UNION ALL
                 SELECT sql FROM duckdb_views()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND view_name = {relation_literal}
                 UNION ALL
                 SELECT sql FROM duckdb_indexes()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                   AND sql IS NOT NULL"
        ))?;
        let statements = listed
            .rows
            .iter()
            .map(|row| required_cell(&listed, row, "sql").map(terminated))
            .collect::<Result<Vec<_>, _>>()?;
        if statements.is_empty() {
            return Err(plain_error(format!("{schema} has no relation {relation}.")));
        }
        Ok(statements.join("\n"))
    }
}

/// Arrow's export of a DuckDB type it has no type of its own for: tagged
/// with the type's name rather than squeezed into the nearest Arrow type,
/// where a `UHUGEINT` wraps into a negative `DECIMAL(38,0)` and a `HUGEINT`
/// cannot be told from a decimal. [`printable`] reads the tags.
fn config() -> Result<Config, DbError> {
    Config::default()
        .with("arrow_lossless_conversion", "true")
        .map_err(|error| plain_error(error.to_string()))
}

fn lock(
    connection: &Mutex<duckdb::Connection>,
) -> Result<std::sync::MutexGuard<'_, duckdb::Connection>, DbError> {
    connection.lock().map_err(|_| {
        plain_error("The connection is unavailable after an earlier internal failure.".into())
    })
}

fn run(connection: &duckdb::Connection, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
    let started = Instant::now();
    let mut statement = connection.prepare(sql).map_err(query_error)?;
    let schema = statement
        .stream_arrow([])
        .map_err(query_error)?
        .get_schema();
    let columns: Vec<Column> = schema
        .fields()
        .iter()
        .map(|field| Column {
            name: field.name().clone(),
            data_type: Some(type_name(field)),
        })
        .collect();

    // A command answers with a column of its own: `Count`, the rows a write
    // changed, or `Success`, empty. Which statements those are is the
    // parser's call, since a `SELECT count(*) AS "Count"` has the same shape.
    let command = match schema.fields().as_ref() {
        [field] => matches!(
            (field.name().as_str(), type_name(field).as_str()),
            ("Count", "bigint") | ("Success", "boolean")
        ),
        _ => false,
    };
    if command && !crate::sql::answers_with_rows(Engine::DuckDb, sql) {
        let changed = statement.step().map_err(query_error)?.and_then(|batch| {
            let counts = batch.column(0).as_any().downcast_ref::<Int64Array>()?;
            (!counts.is_empty() && !counts.is_null(0)).then(|| counts.value(0))
        });
        return Ok(QueryResult {
            rows_affected: changed.and_then(|changed| u64::try_from(changed).ok()),
            elapsed: started.elapsed(),
            ..QueryResult::default()
        });
    }

    if let Some(feed) = fetch.feed {
        feed.begin(columns.clone());
    }
    // `NULL` inside a list or a struct, where DuckDB writes it too. A cell
    // that is NULL itself never reaches the formatter.
    let options = FormatOptions::default()
        .with_null("NULL")
        .with_date_format(Some("%Y-%m-%d"))
        .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
        .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"));
    let mut result = QueryResult::default();
    let mut builder = RowBuilder::default();
    let mut cell = String::new();
    // This set's rows, kept or not, and kept: a fed row is not in `rows` to
    // be counted.
    let (mut returned, mut kept) = (0, 0);
    let mut filled = None;
    'batches: while let Some(batch) = statement.step().map_err(query_error)? {
        let arrays = batch
            .columns()
            .iter()
            .zip(schema.fields())
            .map(|(array, field)| printable(array, field, &options))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| plain_error(error.to_string()))?;
        let formatters = arrays
            .iter()
            .map(|array| ArrayFormatter::try_new(array.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| plain_error(error.to_string()))?;
        // Logical, not physical: a bare `NULL` arrives as Arrow's null type,
        // which has no validity buffer and so no physical nulls.
        let nulls: Vec<_> = arrays.iter().map(|array| array.logical_nulls()).collect();
        for row in 0..batch.len() {
            returned += 1;
            if fetch.limit.is_some_and(|limit| kept >= limit) {
                // Dropping the statement at the end of this call ends the
                // read; there is no server to tell.
                if fetch.reads_only {
                    result.stopped = Some(Stopped::AtLimit);
                    break 'batches;
                }
                // Draining, with nothing more to keep: the rest of this batch
                // only needs counting.
                returned += batch.len() - row - 1;
                continue 'batches;
            }
            kept += 1;
            if fetch.limit == Some(kept) {
                filled = Some(started.elapsed());
                if let Some(feed) = fetch.feed {
                    feed.fill();
                }
            }
            for (nulls, formatter) in nulls.iter().zip(&formatters) {
                if nulls.as_ref().is_some_and(|nulls| nulls.is_null(row)) {
                    builder.push(None);
                    continue;
                }
                cell.clear();
                write!(cell, "{}", formatter.value(row))
                    .map_err(|_| plain_error("A value could not be rendered as text.".into()))?;
                builder.push(Some(&cell));
            }
            let cells = builder.finish()?;
            result.bytes += cells.text_len();
            match fetch.feed {
                Some(feed) => feed.push(cells),
                None => result.rows.push(cells),
            }
        }
    }

    // A query's count is the rows it returned.
    result.rows_affected = Some(returned as u64);
    result.capped_from = (result.stopped.is_none() && returned > kept).then_some(returned);
    // Timed to the last kept row when rows past the limit came after it:
    // draining the statement is not the wait for these.
    result.elapsed = match filled {
        Some(filled) if returned > kept => filled,
        _ => started.elapsed(),
    };
    result.columns = columns;
    Ok(result)
}

/// Which table these columns can be written back to, if any.
///
/// DuckDB names no column's table, so the statement says where each column
/// came from, and only a select of one table's own columns
/// ([`crate::sql::table_read`]) answers at all; the catalog says what the
/// table is and which key names its rows. Every step may answer "no", a
/// failed lookup included: the user's statement has already succeeded.
fn edit_target(
    connection: &duckdb::Connection,
    sql: &str,
    columns: &[Column],
) -> Option<EditTarget> {
    let read = crate::sql::table_read(Engine::DuckDb, sql)?;
    let (schema, table) = resolved_table(connection, &read.table)?;
    let lookup = |sql: String| -> Option<Vec<String>> {
        let result = run(connection, &sql, Fetch::default()).ok()?;
        result
            .rows
            .iter()
            .map(|row| row.get(0).map(str::to_string))
            .collect()
    };
    let (schema_literal, table_literal) = (
        Engine::DuckDb.quote_literal(&schema),
        Engine::DuckDb.quote_literal(&table),
    );
    let names = lookup(format!(
        "SELECT column_name FROM duckdb_columns()
         WHERE database_name = current_database()
           AND schema_name = {schema_literal} AND table_name = {table_literal}
         ORDER BY column_index"
    ))?;
    let key = lookup(format!(
        "SELECT unnest(constraint_column_names) FROM duckdb_constraints()
         WHERE database_name = current_database()
           AND schema_name = {schema_literal} AND table_name = {table_literal}
           AND constraint_type = 'PRIMARY KEY'"
    ))?;

    let origins: Vec<Option<String>> = read
        .columns
        .iter()
        .flat_map(|projected| match projected {
            Projected::Every => names.iter().cloned().map(Some).collect(),
            // Case-blind, as DuckDB reads a name whether quoted or not. One
            // that is no column of the table is something else spelled as a
            // name: `rowid`, or the table's whole row.
            Projected::Column(written) => vec![
                names
                    .iter()
                    .find(|name| name.eq_ignore_ascii_case(written))
                    .cloned(),
            ],
            Projected::Computed => vec![None],
        })
        .collect();
    // The statement read one way and the driver's columns another, so neither
    // reading can be trusted to say which column is which.
    if origins.len() != columns.len() {
        return None;
    }
    // One table column twice would be written twice by one edit.
    let mut seen = HashSet::new();
    if !origins.iter().flatten().all(|origin| seen.insert(origin)) {
        return None;
    }
    let keys = key
        .iter()
        .map(|name| {
            origins
                .iter()
                .position(|origin| origin.as_ref() == Some(name))
        })
        .collect::<Option<Vec<usize>>>()?;
    if keys.is_empty() {
        return None;
    }
    Some(EditTarget {
        schema,
        table,
        columns: origins,
        keys,
        ..Default::default()
    })
}

/// The stored schema and name of the base table `written` names in the
/// database the connection is on, where an `UPDATE` dbdelve writes as
/// `"schema"."table"` would land on the same table.
///
/// Asked of the connection that ran the statement, so a `USE` -- the
/// profile's picked database and schema, or the user's own -- is the one in
/// force: a one-part name is looked for in the current schema, the first
/// place DuckDB looks after the temporary catalog.
///
/// Refused wherever that is in doubt: a search path other than the one entry
/// a `USE` writes for the current database and schema (DuckDB tries each entry
/// in turn), a temporary table or view of the same name (the temporary
/// catalog is searched first), a schema whose name is also an attached
/// database's, and a three-part name in another database.
fn resolved_table(connection: &duckdb::Connection, written: &[String]) -> Option<(String, String)> {
    let at = run(
        connection,
        "SELECT current_database(), current_schema(), current_setting('search_path')",
        Fetch::default(),
    )
    .ok()?;
    let [row] = at.rows.as_slice() else {
        return None;
    };
    let (current_database, current_schema, path) = (row.get(0)?, row.get(1)?, row.get(2)?);
    if !path.is_empty() && search_path(path)? != [[current_database, current_schema]] {
        return None;
    }

    let literal = |name: &str| Engine::DuckDb.quote_literal(name);
    let (schema, table, also) = match written {
        [table] => (literal(current_schema), literal(table), String::new()),
        [schema, table] => (literal(schema), literal(table), String::new()),
        [database, schema, table] => (
            literal(schema),
            literal(table),
            format!(
                "AND lower(current_database()) = lower({})",
                literal(database)
            ),
        ),
        _ => return None,
    };
    let result = run(
        connection,
        &format!(
            "SELECT found.schema_name, found.table_name FROM duckdb_tables() AS found
             WHERE found.database_name = current_database()
               AND lower(found.schema_name) = lower({schema})
               AND lower(found.table_name) = lower({table})
               AND NOT EXISTS (SELECT 1 FROM duckdb_databases()
                               WHERE lower(database_name) = lower(found.schema_name))
               AND NOT EXISTS (SELECT 1 FROM duckdb_tables()
                               WHERE database_name = 'temp'
                                 AND lower(table_name) = lower({table}))
               AND NOT EXISTS (SELECT 1 FROM duckdb_views()
                               WHERE database_name = 'temp'
                                 AND lower(view_name) = lower({table}))
               {also}"
        ),
        Fetch::default(),
    )
    .ok()?;
    match result.rows.as_slice() {
        [row] => Some((row.get(0)?.to_string(), row.get(1)?.to_string())),
        _ => None,
    }
}

/// A `search_path` setting's entries, each a name's dotted parts unquoted.
/// None for one DuckDB itself would not read.
fn search_path(text: &str) -> Option<Vec<Vec<String>>> {
    let mut entries = vec![vec![String::new()]];
    let mut chars = text.chars().peekable();
    // Whether the part being read has anything in it yet, quoted or not.
    let mut started = false;
    while let Some(next) = chars.next() {
        let entry = entries.last_mut()?;
        match next {
            '"' => {
                loop {
                    match chars.next()? {
                        '"' if chars.peek() == Some(&'"') => {
                            chars.next();
                            entry.last_mut()?.push('"');
                        }
                        '"' => break,
                        other => entry.last_mut()?.push(other),
                    }
                }
                started = true;
            }
            '.' if started => {
                entry.push(String::new());
                started = false;
            }
            ',' if started => {
                entries.push(vec![String::new()]);
                started = false;
            }
            '.' | ',' => return None,
            other => {
                entry.last_mut()?.push(other);
                started = true;
            }
        }
    }
    started.then_some(entries)
}

/// End the transaction a failed submission opened, and say so in the error,
/// for the reason `sqlite.rs` gives.
///
/// Read off the submission's text, as `mysql.rs` does, because duckdb-rs
/// answers `is_autocommit` with `true` whatever the connection is in. A
/// typo inside a transaction the user began earlier is theirs to finish and
/// is left alone. A `BEGIN` sent inside one fails and aborts it, so it has
/// nothing left in it but a `ROLLBACK` either way. One the batch already
/// ended, with a `COMMIT` before the statement that failed, has nothing to
/// roll back or report.
fn rolled_back(connection: &duckdb::Connection, sql: &str, error: DbError) -> DbError {
    let Some(start) = Engine::DuckDb.transaction_start() else {
        return error;
    };
    if !sql
        .trim_start()
        .get(..start.len())
        .is_some_and(|word| word.eq_ignore_ascii_case(start))
    {
        return error;
    }
    let outcome = match connection.execute_batch("ROLLBACK") {
        Ok(()) => "The transaction the batch opened was rolled back; nothing it wrote remains."
            .to_string(),
        Err(failure) if failure.to_string().contains(NO_TRANSACTION) => return error,
        Err(failure) => format!(
            "The transaction the batch opened is still open: the rollback failed too. {failure}"
        ),
    };
    DbError {
        message: format!("{}\n\n{outcome}", error.message),
        position: error.position,
    }
}

/// `array` in types Arrow's formatter prints the way DuckDB's shell does,
/// inside a list, a struct or a map too.
///
/// A zone a timestamp names is spelled as its offset: DuckDB hands a
/// `TIMESTAMPTZ` over as UTC under the zone's name, and Arrow formats a named
/// zone only with `chrono-tz`, which links the whole zone database in for the
/// one name DuckDB ever gives. A type [`config`] tags, which Arrow would print
/// as bytes, is written out as text, and so is a date or a timestamp column
/// that holds an infinity.
fn printable(
    array: &ArrayRef,
    field: &Field,
    options: &FormatOptions,
) -> Result<ArrayRef, ArrowError> {
    if let Some(name) = extension(field) {
        return match name.as_str() {
            "boolean" => cast(array, &DataType::Boolean),
            "hugeint" => spelled(array, |bytes| {
                Some(i128::from_le_bytes(bytes.try_into().ok()?).to_string())
            }),
            "uhugeint" => spelled(array, |bytes| {
                Some(u128::from_le_bytes(bytes.try_into().ok()?).to_string())
            }),
            "uuid" => spelled(array, |bytes| {
                Some(uuid::Uuid::from_slice(bytes).ok()?.to_string())
            }),
            "bit" => spelled(array, bits),
            "bignum" => spelled(array, bignum),
            "time_tz" => spelled(array, time_tz),
            _ => Ok(array.clone()),
        };
    }
    let item = |values: &ArrayRef, field: &FieldRef| -> FieldRef {
        Arc::new(Field::new(
            field.name(),
            values.data_type().clone(),
            field.is_nullable(),
        ))
    };
    Ok(match field.data_type() {
        DataType::Timestamp(unit, Some(_)) => finite_or_spelled(
            &cast(array, &DataType::Timestamp(*unit, Some("+00:00".into())))?,
            options,
        )?,
        DataType::Date32 | DataType::Timestamp(_, None) => finite_or_spelled(array, options)?,
        DataType::List(field) => {
            let list = array.as_list::<i32>();
            let values = printable(list.values(), field, options)?;
            if Arc::ptr_eq(&values, list.values()) {
                return Ok(array.clone());
            }
            Arc::new(ListArray::try_new(
                item(&values, field),
                list.offsets().clone(),
                values,
                list.nulls().cloned(),
            )?)
        }
        DataType::LargeList(field) => {
            let list = array.as_list::<i64>();
            let values = printable(list.values(), field, options)?;
            if Arc::ptr_eq(&values, list.values()) {
                return Ok(array.clone());
            }
            Arc::new(LargeListArray::try_new(
                item(&values, field),
                list.offsets().clone(),
                values,
                list.nulls().cloned(),
            )?)
        }
        DataType::FixedSizeList(field, size) => {
            let list = array.as_fixed_size_list();
            let values = printable(list.values(), field, options)?;
            if Arc::ptr_eq(&values, list.values()) {
                return Ok(array.clone());
            }
            Arc::new(FixedSizeListArray::try_new(
                item(&values, field),
                *size,
                values,
                list.nulls().cloned(),
            )?)
        }
        DataType::Map(field, sorted) => {
            let map = array.as_map();
            let entries: ArrayRef = Arc::new(map.entries().clone());
            let printed = printable(&entries, field, options)?;
            if Arc::ptr_eq(&printed, &entries) {
                return Ok(array.clone());
            }
            Arc::new(MapArray::try_new(
                item(&printed, field),
                map.offsets().clone(),
                printed.as_struct().clone(),
                map.nulls().cloned(),
                *sorted,
            )?)
        }
        DataType::Struct(fields) => {
            let structure = array.as_struct();
            let columns = structure
                .columns()
                .iter()
                .zip(fields)
                .map(|(column, field)| printable(column, field, options))
                .collect::<Result<Vec<_>, _>>()?;
            if columns
                .iter()
                .zip(structure.columns())
                .all(|(printed, column)| Arc::ptr_eq(printed, column))
            {
                return Ok(array.clone());
            }
            let fields = columns
                .iter()
                .zip(fields)
                .map(|(column, field)| item(column, field))
                .collect::<Vec<_>>();
            Arc::new(StructArray::try_new(
                fields.into(),
                columns,
                structure.nulls().cloned(),
            )?)
        }
        _ => array.clone(),
    })
}

/// `array` as text when it holds DuckDB's `infinity` or `-infinity`: the
/// widest value its integer holds, either way, which Arrow reads as a date
/// past the end of its calendar and fails to print. Left as it is otherwise.
fn finite_or_spelled(array: &ArrayRef, options: &FormatOptions) -> Result<ArrayRef, ArrowError> {
    let infinity = |value: i64, widest: i64| {
        if value == widest {
            Some("infinity")
        } else if value == -widest {
            Some("-infinity")
        } else {
            None
        }
    };
    let data = array.to_data();
    let infinities: Vec<_> = match array.data_type() {
        DataType::Date32 => data
            .buffer::<i32>(0)
            .iter()
            .map(|&day| infinity(day.into(), i32::MAX.into()))
            .collect(),
        _ => data
            .buffer::<i64>(0)
            .iter()
            .map(|&moment| infinity(moment, i64::MAX))
            .collect(),
    };
    if infinities.iter().all(Option::is_none) {
        return Ok(array.clone());
    }
    let formatter = ArrayFormatter::try_new(array.as_ref(), options)?;
    let values: StringArray = infinities
        .iter()
        .enumerate()
        .map(|(row, infinity)| {
            array.is_valid(row).then(|| match infinity {
                Some(infinity) => infinity.to_string(),
                None => formatter.value(row).to_string(),
            })
        })
        .collect();
    Ok(Arc::new(values))
}

/// The DuckDB type `field` carries under an Arrow extension, as [`config`]
/// asks for: Arrow's own boolean as bytes, UUID and JSON, or one of DuckDB's
/// it calls opaque.
fn extension(field: &Field) -> Option<String> {
    let metadata = field.metadata();
    match metadata.get("ARROW:extension:name")?.as_str() {
        "arrow.bool8" => Some("boolean".into()),
        "arrow.uuid" => Some("uuid".into()),
        "arrow.json" => Some("json".into()),
        "arrow.opaque" => {
            let opaque: serde_json::Value =
                serde_json::from_str(metadata.get("ARROW:extension:metadata")?).ok()?;
            Some(opaque.get("type_name")?.as_str()?.to_string())
        }
        _ => None,
    }
}

/// A column of bytes as the text `spell` makes of each value.
fn spelled(
    array: &ArrayRef,
    spell: impl Fn(&[u8]) -> Option<String>,
) -> Result<ArrayRef, ArrowError> {
    let spell = |bytes: Option<&[u8]>| -> Result<Option<String>, ArrowError> {
        bytes
            .map(|bytes| {
                spell(bytes).ok_or_else(|| {
                    ArrowError::InvalidArgumentError(format!(
                        "{} bytes are not a value of this type",
                        bytes.len()
                    ))
                })
            })
            .transpose()
    };
    let values: StringArray = match array.data_type() {
        DataType::FixedSizeBinary(_) => array
            .as_fixed_size_binary()
            .iter()
            .map(spell)
            .collect::<Result<_, _>>()?,
        DataType::Binary => array
            .as_binary::<i32>()
            .iter()
            .map(spell)
            .collect::<Result<_, _>>()?,
        other => {
            return Err(ArrowError::InvalidArgumentError(format!(
                "{other} is not a type of bytes"
            )));
        }
    };
    Ok(Arc::new(values))
}

/// A `BIT` as DuckDB packs it: a byte saying how many of the next byte's
/// leading bits are padding, then the bits.
fn bits(bytes: &[u8]) -> Option<String> {
    let (&padding, bytes) = bytes.split_first()?;
    let bits: String = bytes.iter().map(|byte| format!("{byte:08b}")).collect();
    Some(bits.get(usize::from(padding)..)?.to_string())
}

/// A `BIGNUM` as DuckDB packs it: a three-byte header whose top bit is set
/// for a number that is not negative, then its magnitude in base 256, most
/// significant byte first. A negative number has every byte inverted.
fn bignum(bytes: &[u8]) -> Option<String> {
    let negative = bytes.first()? & 0x80 == 0;
    let digits = bytes.get(3..)?;
    let magnitude = match negative {
        true => BigUint::from_bytes_be(&digits.iter().map(|byte| !byte).collect::<Vec<_>>()),
        false => BigUint::from_bytes_be(digits),
    };
    Some(match negative {
        true => format!("-{magnitude}"),
        false => magnitude.to_string(),
    })
}

/// A `TIMETZ` as DuckDB packs it: microseconds since midnight in the high
/// 40 bits, and in the low 24 its offset in seconds taken from the widest
/// DuckDB allows, so that times sort by the instant they name.
fn time_tz(bytes: &[u8]) -> Option<String> {
    const WIDEST_OFFSET: i64 = 16 * 3600 - 1;
    let bits = u64::from_le_bytes(bytes.try_into().ok()?);
    let offset = WIDEST_OFFSET - (bits & 0xff_ffff) as i64;
    let micros = bits >> 24;
    let (seconds, micros) = (micros / 1_000_000, micros % 1_000_000);
    let mut time = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if micros > 0 {
        let fraction = format!("{micros:06}");
        time.push('.');
        time.push_str(fraction.trim_end_matches('0'));
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.unsigned_abs();
    let (hours, minutes, seconds) = (offset / 3600, offset / 60 % 60, offset % 60);
    write!(time, "{sign}{hours:02}").ok()?;
    if minutes > 0 || seconds > 0 {
        write!(time, ":{minutes:02}").ok()?;
    }
    if seconds > 0 {
        write!(time, ":{seconds:02}").ok()?;
    }
    Some(time)
}

/// An Arrow type under the name DuckDB gives it, lowercased as every engine's
/// type names are in the grid, so `db::is_numeric_type` and the Structure view
/// read the same words DuckDB's own shell prints.
fn type_name(field: &Field) -> String {
    if let Some(name) = extension(field) {
        return match name.as_str() {
            "time_tz" => "time with time zone".into(),
            _ => name,
        };
    }
    match field.data_type() {
        DataType::Boolean => "boolean".into(),
        DataType::Int8 => "tinyint".into(),
        DataType::Int16 => "smallint".into(),
        DataType::Int32 => "integer".into(),
        DataType::Int64 => "bigint".into(),
        DataType::UInt8 => "utinyint".into(),
        DataType::UInt16 => "usmallint".into(),
        DataType::UInt32 => "uinteger".into(),
        DataType::UInt64 => "ubigint".into(),
        DataType::Float16 | DataType::Float32 => "float".into(),
        DataType::Float64 => "double".into(),
        DataType::Decimal128(precision, scale) | DataType::Decimal256(precision, scale) => {
            format!("decimal({precision},{scale})")
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "varchar".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "blob".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Time32(_) | DataType::Time64(_) => "time".into(),
        DataType::Timestamp(_, None) => "timestamp".into(),
        DataType::Timestamp(_, Some(_)) => "timestamp with time zone".into(),
        DataType::Interval(_) | DataType::Duration(_) => "interval".into(),
        DataType::List(item) | DataType::LargeList(item) => format!("{}[]", type_name(item)),
        DataType::FixedSizeList(item, size) => format!("{}[{size}]", type_name(item)),
        DataType::Struct(_) => "struct".into(),
        DataType::Map(..) => "map".into(),
        DataType::Dictionary(_, value) => type_name(&Field::new("", value.as_ref().clone(), true)),
        other => other.to_string().to_lowercase(),
    }
}

/// DuckDB's message as it wrote it. It carries no offset into the statement
/// that `DbError::position` could point the editor at; its `LINE 1:` excerpt
/// already shows where.
fn query_error(error: duckdb::Error) -> DbError {
    DbError {
        message: error.to_string(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::IndexKind;

    /// The fixtures beside the other engines' seeds, by absolute path: a
    /// relative one would resolve against wherever the test runner stands.
    fn fixture(name: &str) -> String {
        format!("{}/dev/duckdb/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn memory(setup: &str) -> Connection {
        let connection = Connection::open(IN_MEMORY, 0).expect("an in-memory database opens");
        if !setup.is_empty() {
            connection.query(setup, Fetch::default()).expect("setup");
        }
        connection
    }

    fn cells(result: &QueryResult) -> Vec<Vec<Option<&str>>> {
        result.rows.iter().map(|row| row.iter().collect()).collect()
    }

    fn count(connection: &Connection) -> String {
        connection
            .query("SELECT count(*) FROM t", Fetch::default())
            .unwrap()
            .rows[0]
            .get(0)
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_csv_file_is_a_table_away() {
        let result = memory("")
            .query(
                &format!(
                    "SELECT nombre FROM read_csv('{}') WHERE ciudad = 'Londres'",
                    fixture("clientes.csv")
                ),
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(result.columns[0].name, "nombre");
        assert_eq!(cells(&result), [[Some("Ada Lovelace")]]);
    }

    #[test]
    fn a_spreadsheet_reads_by_its_header_with_numbers_as_numbers() {
        let connection = memory("");
        let result = connection
            .query(
                &format!("SELECT * FROM read_xlsx('{}')", fixture("facturas.xlsx")),
                Fetch::default(),
            )
            .unwrap();
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["numero", "cliente", "importe", "fecha", "pagada"]);
        assert_eq!(result.columns[2].data_type.as_deref(), Some("double"));
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0].get(2), Some("1250.5"));
        assert_eq!(result.rows[0].get(3), Some("2026-01-03"));
        assert_eq!(result.rows[2].get(2), None);

        let february = connection
            .alongside()
            .unwrap()
            .query(
                &format!(
                    "SELECT numero FROM read_xlsx('{}', sheet := 'Febrero')",
                    fixture("facturas.xlsx")
                ),
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(cells(&february), [[Some("F-010")]]);
    }

    #[test]
    fn a_spreadsheets_header_repeated_in_another_case_reads_as_another_column() {
        let result = memory("")
            .query(
                &format!("SELECT * FROM read_xlsx('{}')", fixture("encabezados.xlsx")),
                Fetch::default(),
            )
            .unwrap();
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Nombre", "nombre_2", "NOMBRE_3", "nombre_2_2"]);
    }

    #[test]
    fn nested_and_exact_types_render_as_duckdb_prints_them() {
        let result = memory("")
            .query(
                "SELECT [1, NULL] AS list, 12.50::DECIMAL(5,2) AS money, NULL AS nothing,
                        TIMESTAMP '2026-01-03 10:04:05' AS moment, DATE '2026-01-03' AS day,
                        {'at': TIMESTAMPTZ '2026-01-03 10:04:05+02'} AS zoned,
                        true AS yes, [false, NULL] AS answers",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(result.columns[1].data_type.as_deref(), Some("decimal(5,2)"));
        assert_eq!(result.columns[6].data_type.as_deref(), Some("boolean"));
        assert_eq!(result.columns[7].data_type.as_deref(), Some("boolean[]"));
        assert_eq!(
            cells(&result),
            [[
                Some("[1, NULL]"),
                Some("12.50"),
                None,
                Some("2026-01-03 10:04:05"),
                Some("2026-01-03"),
                Some("{at: 2026-01-03 08:04:05+00:00}"),
                Some("true"),
                Some("[false, NULL]")
            ]]
        );
    }

    #[test]
    fn every_integer_width_reads_as_a_number() {
        let connection = memory(
            "CREATE TABLE t (a UTINYINT, b USMALLINT, c UINTEGER, d UBIGINT, e HUGEINT,
                             f UHUGEINT);",
        );
        let result = connection
            .query(
                "SELECT 1::UTINYINT, 1::USMALLINT, 1::UINTEGER, 1::UBIGINT, 1::HUGEINT,
                        1::UHUGEINT",
                Fetch::default(),
            )
            .unwrap();
        for column in &result.columns {
            let data_type = column.data_type.as_deref().unwrap();
            assert!(super::super::is_numeric_type(data_type), "{data_type}");
        }
        for column in connection.structure("main", "t").unwrap().columns {
            assert!(
                super::super::is_numeric_type(&column.data_type),
                "{}",
                column.data_type
            );
        }
    }

    #[test]
    fn a_128_bit_integer_reads_exactly_at_either_end() {
        let result = memory("")
            .query(
                "SELECT 340282366920938463463374607431768211455::UHUGEINT AS top,
                        (-170141183460469231731687303715884105728)::HUGEINT AS bottom,
                        [0::UHUGEINT, 340282366920938463463374607431768211455::UHUGEINT] AS listed,
                        '018f1f6e-7c2a-7000-8000-000000000001'::UUID AS id",
                Fetch::default(),
            )
            .unwrap();
        let types: Vec<_> = result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref().unwrap())
            .collect();
        assert_eq!(types, ["uhugeint", "hugeint", "uhugeint[]", "uuid"]);
        assert_eq!(
            cells(&result),
            [[
                Some("340282366920938463463374607431768211455"),
                Some("-170141183460469231731687303715884105728"),
                Some("[0, 340282366920938463463374607431768211455]"),
                Some("018f1f6e-7c2a-7000-8000-000000000001")
            ]]
        );
    }

    #[test]
    fn an_infinite_date_or_timestamp_reads_as_infinity() {
        let result = memory("")
            .query(
                "SELECT DATE 'infinity' AS day, TIMESTAMP '-infinity' AS moment,
                        TIMESTAMPTZ 'infinity' AS zoned, TIMESTAMP_NS '-infinity' AS nanos,
                        TIMESTAMP_MS 'infinity' AS millis, TIMESTAMP_S '-infinity' AS seconds,
                        [DATE '-infinity', DATE '2026-01-03'] AS days,
                        {'at': TIMESTAMPTZ '-infinity'} AS event
                 UNION ALL
                 SELECT DATE '2026-01-03', TIMESTAMP '2026-01-03 10:04:05',
                        TIMESTAMPTZ '2026-01-03 10:04:05+02',
                        TIMESTAMP_NS '2026-01-03 10:04:05.123456789',
                        TIMESTAMP_MS '2026-01-03 10:04:05.123', TIMESTAMP_S '2026-01-03 10:04:05',
                        [], {'at': TIMESTAMPTZ '2026-01-03 10:04:05+02'}",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(
            cells(&result),
            [
                [
                    Some("infinity"),
                    Some("-infinity"),
                    Some("infinity"),
                    Some("-infinity"),
                    Some("infinity"),
                    Some("-infinity"),
                    Some("[-infinity, 2026-01-03]"),
                    Some("{at: -infinity}")
                ],
                [
                    Some("2026-01-03"),
                    Some("2026-01-03 10:04:05"),
                    Some("2026-01-03 08:04:05+00:00"),
                    Some("2026-01-03 10:04:05.123456789"),
                    Some("2026-01-03 10:04:05.123"),
                    Some("2026-01-03 10:04:05"),
                    Some("[]"),
                    Some("{at: 2026-01-03 08:04:05+00:00}")
                ]
            ]
        );
    }

    #[test]
    fn a_bit_string_reads_as_bits_and_a_bignum_as_a_number() {
        let result = memory("")
            .query(
                "SELECT '101010011'::BIT AS flags, '00000000011'::BIT AS padded,
                        ['1'::BIT, '0'::BIT] AS listed_flags,
                        ('-1' || repeat('0', 50))::BIGNUM AS big, 0::BIGNUM AS zero,
                        (-1)::BIGNUM AS minus_one, (-256)::BIGNUM AS minus_byte,
                        [255::BIGNUM] AS listed_big",
                Fetch::default(),
            )
            .unwrap();
        let types: Vec<_> = result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "bit", "bit", "bit[]", "bignum", "bignum", "bignum", "bignum", "bignum[]"
            ]
        );
        assert!(super::super::is_numeric_type("bignum"));
        assert!(!super::super::is_numeric_type("bit"));
        assert_eq!(
            cells(&result),
            [[
                Some("101010011"),
                Some("00000000011"),
                Some("[1, 0]"),
                Some("-100000000000000000000000000000000000000000000000000"),
                Some("0"),
                Some("-1"),
                Some("-256"),
                Some("[255]")
            ]]
        );
    }

    #[test]
    fn a_time_with_a_zone_keeps_its_offset() {
        let result = memory("")
            .query(
                "SELECT '12:34:56+05:30'::TIMETZ AS india, '12:34:56.5-08'::TIMETZ AS pacific,
                        '01:02:03+05:30:15'::TIMETZ AS seconds,
                        '00:00:00.000001-15:59:59'::TIMETZ AS widest,
                        '24:00:00+00'::TIMETZ AS midnight, ['12:00:00+01'::TIMETZ] AS listed",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(
            result.columns[0].data_type.as_deref(),
            Some("time with time zone")
        );
        assert_eq!(
            result.columns[5].data_type.as_deref(),
            Some("time with time zone[]")
        );
        assert_eq!(
            cells(&result),
            [[
                Some("12:34:56+05:30"),
                Some("12:34:56.5-08"),
                Some("01:02:03+05:30:15"),
                Some("00:00:00.000001-15:59:59"),
                Some("24:00:00+00"),
                Some("[12:00:00+01]")
            ]]
        );
    }

    #[test]
    fn a_fixed_size_array_is_named_with_its_size() {
        let connection = memory("CREATE TABLE t (a INTEGER[3], b INTEGER[2][1], c VARCHAR[1][]);");
        let result = connection
            .query(
                "SELECT [1, 2, 3]::INTEGER[3] AS a, [[1, 2]]::INTEGER[2][1] AS b,
                        [['x']]::VARCHAR[1][] AS c",
                Fetch::default(),
            )
            .unwrap();
        let types: Vec<_> = result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref().unwrap())
            .collect();
        assert_eq!(types, ["integer[3]", "integer[2][1]", "varchar[1][]"]);
        let structure: Vec<_> = connection
            .structure("main", "t")
            .unwrap()
            .columns
            .into_iter()
            .map(|column| column.data_type)
            .collect();
        assert_eq!(structure, types);
        assert_eq!(
            cells(&result),
            [[Some("[1, 2, 3]"), Some("[[1, 2]]"), Some("[[x]]")]]
        );
    }

    #[test]
    fn a_query_counts_its_rows_and_a_write_counts_what_it_changed() {
        let connection = memory("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2), (3);");
        let selected = connection
            .query("SELECT a FROM t", Fetch::default())
            .unwrap();
        assert_eq!(selected.rows_affected, Some(3));
        let updated = connection
            .query("UPDATE t SET a = a + 1 WHERE a > 1", Fetch::default())
            .unwrap();
        assert_eq!(updated.rows_affected, Some(2));
        assert!(updated.columns.is_empty());
        let created = connection
            .query("CREATE TABLE u (a INTEGER)", Fetch::default())
            .unwrap();
        assert!(created.columns.is_empty());
        let counted = connection
            .query("SELECT count(*) AS \"Count\" FROM t", Fetch::default())
            .unwrap();
        assert_eq!(cells(&counted), [[Some("3")]]);
    }

    #[test]
    fn a_command_is_told_from_a_query_by_its_statement_not_its_column() {
        let connection = memory("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);");
        let inserted = connection
            .query(
                "WITH s AS (SELECT 2 AS id) INSERT INTO t SELECT * FROM s",
                Fetch::default(),
            )
            .unwrap();
        assert!(inserted.columns.is_empty(), "{inserted:?}");
        assert_eq!(inserted.rows_affected, Some(1));
        let by_name = connection
            .query("INSERT INTO t BY NAME SELECT 3 AS id", Fetch::default())
            .unwrap();
        assert!(by_name.columns.is_empty(), "{by_name:?}");
        assert_eq!(by_name.rows_affected, Some(1));
        let checkpoint = connection.query("CHECKPOINT", Fetch::default()).unwrap();
        assert!(checkpoint.columns.is_empty(), "{checkpoint:?}");

        let tabled = connection
            .query(
                "CREATE TABLE c AS SELECT 42::BIGINT AS \"Count\"; TABLE c",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(cells(&tabled), [[Some("42")]]);
        let counted = connection
            .query(
                "INSERT INTO t BY NAME SELECT 4 AS id; SELECT count(*) AS \"Count\" FROM t",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(cells(&counted), [[Some("4")]]);
    }

    #[test]
    fn a_multi_statement_selection_keeps_the_last_result() {
        let result = memory("")
            .query("SELECT 1; SELECT 2, 3", Fetch::default())
            .unwrap();
        assert_eq!(cells(&result), [[Some("2"), Some("3")]]);
    }

    #[test]
    fn a_row_limit_keeps_the_first_rows_and_counts_the_rest() {
        let connection = memory("");
        let kept = |reads_only| {
            connection
                .query(
                    "SELECT * FROM range(5000) ORDER BY 1",
                    Fetch {
                        limit: Some(2),
                        reads_only,
                        ..Fetch::default()
                    },
                )
                .unwrap()
        };
        let counted = kept(false);
        assert_eq!(cells(&counted), [[Some("0")], [Some("1")]]);
        assert_eq!(counted.capped_from, Some(5000));
        let stopped = kept(true);
        assert_eq!(cells(&stopped), [[Some("0")], [Some("1")]]);
        assert_eq!(stopped.stopped, Some(Stopped::AtLimit));
        assert_eq!(stopped.capped_from, None);
    }

    #[test]
    fn a_bracketed_batch_that_fails_part_way_rolls_back_and_says_so() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        let error = connection
            .query(
                "BEGIN; INSERT INTO t VALUES (1); INSERT INTO nosuch VALUES (1); COMMIT;",
                Fetch::default(),
            )
            .unwrap_err();
        assert!(error.message.contains("nosuch"), "{error:?}");
        assert!(error.message.contains("rolled back"), "{error:?}");
        assert_eq!(count(&connection), "0");
        connection
            .query("BEGIN; INSERT INTO t VALUES (1); COMMIT;", Fetch::default())
            .expect("the next batch opens its own transaction");
    }

    #[test]
    fn a_batch_that_committed_before_it_failed_has_no_transaction_to_report() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        let error = connection
            .query(
                "BEGIN; INSERT INTO t VALUES (1); COMMIT; SELECT * FROM nosuch;",
                Fetch::default(),
            )
            .unwrap_err();
        assert!(error.message.contains("nosuch"), "{error:?}");
        assert!(!error.message.contains("still open"), "{error:?}");
        assert!(!error.message.contains("rolled back"), "{error:?}");
        assert_eq!(count(&connection), "1");
    }

    #[test]
    fn a_cancelled_bracketed_batch_reads_as_a_cancel() {
        let connection = memory("CREATE TABLE t (id BIGINT);");
        let stopper = connection.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = connection
            .query(
                "BEGIN; INSERT INTO t SELECT a.range FROM range(1000000000000) a, range(1000) b; \
                 COMMIT;",
                Fetch::default(),
            )
            .unwrap_err();
        thread.join().unwrap();
        assert!(error.message.contains("rolled back"), "{error:?}");
        assert!(super::super::is_cancel(Engine::DuckDb, &error), "{error:?}");
    }

    #[test]
    fn a_transaction_the_user_opened_survives_their_next_typo() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        connection
            .query("BEGIN; INSERT INTO t VALUES (1)", Fetch::default())
            .unwrap();
        connection
            .query("SELEC 1", Fetch::default())
            .expect_err("a typo");
        connection.query("COMMIT", Fetch::default()).unwrap();
        assert_eq!(count(&connection), "1");
    }

    #[test]
    fn a_cancel_inside_the_users_transaction_aborts_it() {
        let connection = memory("CREATE TABLE t (id INTEGER);");
        connection
            .query("BEGIN; INSERT INTO t VALUES (1)", Fetch::default())
            .unwrap();
        let stopper = connection.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = connection.query(RUNAWAY, Fetch::default()).unwrap_err();
        thread.join().unwrap();
        assert!(super::super::is_cancel(Engine::DuckDb, &error), "{error:?}");
        let refused = connection
            .query("SELECT 1", Fetch::default())
            .expect_err("an aborted transaction refuses everything but a ROLLBACK");
        assert!(refused.message.contains("aborted"), "{refused:?}");
        connection.query("ROLLBACK", Fetch::default()).unwrap();
        assert_eq!(count(&connection), "0");
    }

    const RUNAWAY: &str = "SELECT count(*) FROM range(1000000000000) a, range(1000) b";

    #[test]
    fn a_cancel_from_another_thread_stops_a_statement_and_leaves_the_connection_usable() {
        let connection = memory("");
        let stopper = connection.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = connection.query(RUNAWAY, Fetch::default()).unwrap_err();
        thread.join().unwrap();
        assert_eq!(error.message, INTERRUPTED);
        let after = connection.query("SELECT 7", Fetch::default()).unwrap();
        assert_eq!(cells(&after), [[Some("7")]]);
    }

    #[test]
    fn a_statement_timeout_stops_a_runaway_on_its_own() {
        let connection = Connection::open(IN_MEMORY, 1).unwrap();
        let started = Instant::now();
        let error = connection.query(RUNAWAY, Fetch::default()).unwrap_err();
        assert_eq!(error.message, INTERRUPTED);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_connection_alongside_shares_the_database_and_its_cancel_spares_the_other() {
        let connection = memory("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);");
        let beside = connection.alongside().unwrap();
        assert_eq!(count(&beside), "1");
        let stopper = beside.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = beside.query(RUNAWAY, Fetch::default()).unwrap_err();
        thread.join().unwrap();
        assert_eq!(error.message, INTERRUPTED);
        assert_eq!(count(&connection), "1");
    }

    #[test]
    fn the_catalog_and_a_tables_structure_read_back() {
        let connection = memory(
            "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre VARCHAR NOT NULL DEFAULT 'x');
             CREATE TABLE facturas (id INTEGER PRIMARY KEY,
                                    cliente INTEGER REFERENCES clientes(id));
             CREATE INDEX facturas_cliente ON facturas (cliente);
             CREATE UNIQUE INDEX facturas_par ON facturas (cliente, coalesce(id, 0));
             CREATE VIEW v AS SELECT id FROM clientes;",
        );
        let catalog = connection.catalog().unwrap();
        let main = &catalog.schemas[0];
        assert_eq!(main.name, "main");
        let names: Vec<&str> = main.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["clientes", "facturas", "v"]);

        let clientes = connection.structure("main", "clientes").unwrap();
        let columns: Vec<_> = clientes
            .columns
            .iter()
            .map(|column| {
                (
                    column.name.as_str(),
                    column.nullable,
                    column.default.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            columns,
            [("id", false, None), ("nombre", false, Some("'x'"))]
        );

        let facturas = connection.structure("main", "facturas").unwrap();
        assert_eq!(facturas.primary_key(), ["id"]);
        assert_eq!(facturas.foreign_keys[0].referenced_table, "clientes");
        let kinds: Vec<_> = facturas
            .indexes
            .iter()
            .map(|index| (index.name.as_str(), index.kind, index.columns.clone()))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    "facturas_cliente",
                    IndexKind::Plain,
                    vec!["cliente".to_string()]
                ),
                (
                    "facturas_par",
                    IndexKind::Unique,
                    vec!["cliente".into(), "(COALESCE(id, 0))".into()]
                ),
            ]
        );
        let references = connection.references("main", "clientes").unwrap();
        assert_eq!(references[0].table, "facturas");

        let ddl = connection.ddl("main", "facturas").unwrap();
        assert!(ddl.starts_with("CREATE TABLE facturas"), "{ddl}");
        assert!(ddl.contains("CREATE INDEX facturas_cliente"), "{ddl}");
    }

    #[test]
    fn opening_a_path_that_is_not_there_names_it_and_creates_nothing() {
        let path = std::env::temp_dir().join("dbdelve-duckdb-missing.duckdb");
        let _ = std::fs::remove_file(&path);
        let error = Connection::open(path.to_str().unwrap(), 0)
            .err()
            .expect("a missing file is an error");
        assert!(error.message.contains("No database file at"), "{error:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_url_names_a_file_or_nothing_for_memory() {
        assert_eq!(
            path_from_url("duckdb:///tmp/a%20b.duckdb").unwrap(),
            "/tmp/a b.duckdb"
        );
        assert_eq!(path_from_url("duckdb:").unwrap(), IN_MEMORY);
        assert_eq!(path_from_url("duckdb://").unwrap(), IN_MEMORY);
        assert_eq!(
            path_from_url("duckdb:///C:/data/app.duckdb").unwrap(),
            "C:/data/app.duckdb"
        );
        assert!(path_from_url("duckdb:///tmp/%zz.duckdb").is_err());
    }

    #[test]
    fn an_insert_dbdelve_writes_passes_its_gate_and_runs() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR);");
        let statement = crate::sql::insert_row(
            Engine::DuckDb,
            "main",
            "t",
            &[("id", Some("1")), ("name", Some("o'hara"))],
            &[],
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(
            Engine::DuckDb,
            &statement
        ));
        let inserted = connection.query(&statement, Fetch::default()).unwrap();
        assert_eq!(inserted.rows_affected, Some(1));
        let read = connection
            .query("SELECT name FROM t", Fetch::default())
            .unwrap();
        assert_eq!(cells(&read), [[Some("o'hara")]]);
    }

    #[test]
    fn a_filter_dbdelve_writes_matches_a_wildcard_as_itself() {
        use crate::filter::{Operator, filter_predicate};
        let connection =
            memory("CREATE TABLE t (name VARCHAR); INSERT INTO t VALUES ('50%'), ('500');");
        for (operator, value) in [
            (Operator::Contains, "0%"),
            (Operator::StartsWith, "50%"),
            (Operator::EndsWith, "0%"),
            (Operator::Regex, "%$"),
        ] {
            let predicate =
                filter_predicate(Engine::DuckDb, "name", Some("varchar"), operator, value).unwrap();
            let sql = format!("SELECT name FROM t WHERE {predicate}");
            assert!(
                crate::sql::is_generated_select(Engine::DuckDb, &sql),
                "{sql}"
            );
            let read = connection.query(&sql, Fetch::default()).unwrap();
            assert_eq!(cells(&read), [[Some("50%")]], "{sql}");
        }
    }

    #[test]
    fn a_text_filter_matches_a_column_that_is_not_text() {
        use crate::filter::{Operator, filter_predicate};
        let connection = memory(
            "CREATE TABLE t (day DATE, n INTEGER);
             INSERT INTO t VALUES ('2024-03-01', 2024), ('2025-01-01', 7);",
        );
        for (column, data_type, operator, value) in [
            ("day", "date", Operator::Contains, "2024"),
            ("day", "date", Operator::StartsWith, "2024-"),
            ("day", "date", Operator::EndsWith, "03-01"),
            ("n", "integer", Operator::Contains, "02"),
            ("n", "integer", Operator::NotContains, "7"),
            ("day", "date", Operator::Regex, "^2024"),
        ] {
            let predicate =
                filter_predicate(Engine::DuckDb, column, Some(data_type), operator, value).unwrap();
            let sql = format!("SELECT n FROM t WHERE {predicate}");
            assert!(
                crate::sql::is_generated_select(Engine::DuckDb, &sql),
                "{sql}"
            );
            let read = connection.query(&sql, Fetch::default()).unwrap();
            assert_eq!(cells(&read), [[Some("2024")]], "{sql}");
        }
    }

    #[test]
    fn an_import_asked_to_skip_duplicates_runs_into_a_table_with_no_key() {
        use crate::import_rows::{OnDuplicate, Options, Plan, TargetColumn, prepare};
        let connection = memory("CREATE TABLE t AS SELECT 1 AS id, 'a' AS name;");
        let structure = connection.structure("main", "t").unwrap();
        let targets = TargetColumn::from_structure(&structure, Engine::DuckDb);
        let bytes = b"id,name\n2,b\n";
        let mut options = Options::detected(std::path::Path::new("rows.csv"), bytes);
        options.on_duplicate = OnDuplicate::Skip;
        let checked = prepare(bytes, &options, &targets, None, Engine::DuckDb)
            .unwrap()
            .checked;
        let plan = Plan::new(
            Engine::DuckDb,
            "main".into(),
            "t".into(),
            checked,
            &options,
            Engine::DuckDb.skips_duplicates_into(&structure),
        );
        connection
            .query(&plan.statement(0).unwrap(), Fetch::default())
            .unwrap();
        assert_eq!(count(&connection), "2");

        connection
            .query(
                "CREATE TABLE keyed (id INTEGER); CREATE UNIQUE INDEX keyed_id ON keyed (id);",
                Fetch::default(),
            )
            .unwrap();
        let keyed = connection.structure("main", "keyed").unwrap();
        assert!(Engine::DuckDb.skips_duplicates_into(&keyed));
    }

    fn seeded() -> Connection {
        memory(&std::fs::read_to_string(fixture("001-dbdelve-demo.sql")).expect("the dev seed"))
    }

    fn edit_of(connection: &Connection, sql: &str) -> Option<super::super::EditTarget> {
        connection.query(sql, Fetch::default()).unwrap().edit
    }

    fn named(columns: &[Option<&str>]) -> Vec<Option<String>> {
        columns
            .iter()
            .map(|column| column.map(String::from))
            .collect()
    }

    #[test]
    fn a_tables_preview_and_a_typed_select_of_it_are_editable_by_its_primary_key() {
        let connection = seeded();
        let preview = crate::explorer::preview_sql(
            Engine::DuckDb,
            "main",
            "accounts",
            "\"plan\" = 'team'",
            Some(1000),
            2000,
        );
        let every = named(&[
            Some("id"),
            Some("external_id"),
            Some("name"),
            Some("email"),
            Some("plan"),
            Some("balance"),
            Some("active"),
            Some("tags"),
            Some("metadata"),
            Some("created_at"),
        ]);
        for sql in [
            preview.as_str(),
            "SELECT * FROM accounts",
            "select * from Main.ACCOUNTS order by name desc limit 5",
            "SELECT a.* FROM memory.main.accounts AS a WHERE a.balance > 0",
            "FROM accounts",
        ] {
            let edit = edit_of(&connection, sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(
                (edit.schema.as_str(), edit.table.as_str()),
                ("main", "accounts")
            );
            assert_eq!(edit.columns, every, "{sql}");
            assert_eq!(edit.keys, [0], "{sql}");
        }

        let edit = edit_of(
            &connection,
            "SELECT name, a.ID AS ident, upper(name) AS shout, \"active\" FROM accounts a",
        )
        .unwrap();
        assert_eq!(
            edit.columns,
            named(&[Some("name"), Some("id"), None, Some("active")])
        );
        assert_eq!(edit.keys, [1]);

        let edit = edit_of(&connection, "SELECT * FROM analytics.account_scores").unwrap();
        assert_eq!(
            (edit.schema.as_str(), edit.table.as_str()),
            ("analytics", "account_scores")
        );
        assert_eq!(edit.keys, [0]);
    }

    #[test]
    fn a_result_that_is_not_one_keyed_tables_rows_is_not_editable() {
        let connection = seeded();
        connection
            .query(
                "CREATE TABLE twin (id INTEGER PRIMARY KEY)",
                Fetch::default(),
            )
            .unwrap();
        for sql in [
            "SELECT name FROM accounts",
            "SELECT * FROM tally",
            "SELECT * FROM contacts",
            "SELECT * FROM account_overview",
            "SELECT a.id, b.id FROM accounts a JOIN twin b ON a.id = b.id",
            "SELECT * FROM accounts, twin",
            "SELECT id, count(*) FROM accounts GROUP BY id",
            "SELECT DISTINCT id, name FROM accounts",
            "WITH a AS (SELECT * FROM accounts) SELECT * FROM a",
            "SELECT * FROM (SELECT * FROM accounts)",
            "SELECT * FROM accounts UNION ALL SELECT * FROM accounts",
            "SELECT id, id FROM accounts",
            "SELECT * FROM accounts AS a (ident)",
            "SELECT * EXCLUDE (email) FROM accounts",
            "SELECT * FROM range(3)",
            "SELECT id + 0 AS id FROM accounts",
            "SELECT id FROM accounts QUALIFY row_number() OVER () = 1",
            "INSERT INTO twin VALUES (1) RETURNING *",
        ] {
            assert_eq!(edit_of(&connection, sql), None, "{sql}");
        }
    }

    #[test]
    fn a_table_a_temporary_one_shadows_is_not_editable() {
        let connection = memory(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a VARCHAR);
             CREATE TEMP TABLE t (id INTEGER PRIMARY KEY, a VARCHAR);",
        );
        assert_eq!(edit_of(&connection, "SELECT * FROM t"), None);
        assert_eq!(edit_of(&connection, "SELECT * FROM main.t"), None);
    }

    #[test]
    fn an_edit_dbdelve_writes_on_a_traced_row_passes_its_gates_and_lands() {
        use crate::result_grid::{NewValue, PendingRow};
        let connection = seeded();
        let preview =
            crate::explorer::preview_sql(Engine::DuckDb, "main", "accounts", "", Some(1000), 0);
        let read = connection.query(&preview, Fetch::default()).unwrap();
        let edit = read.edit.clone().expect("accounts has a primary key");
        let types: Vec<(String, String)> = read
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.data_type.clone().unwrap()))
            .collect();
        let key = |row: usize| {
            let name = edit.columns[edit.keys[0]].clone().unwrap();
            let value = read.rows[row].get(edit.keys[0]).unwrap().to_string();
            (name, value)
        };
        let (first, second) = (key(0), key(1));

        let batch = crate::sql::update_batch(
            Engine::DuckDb,
            &[
                PendingRow {
                    schema: edit.schema.clone(),
                    table: edit.table.clone(),
                    sets: vec![
                        ("name".into(), NewValue::Value("o'hara".into())),
                        ("active".into(), NewValue::Value("false".into())),
                        ("balance".into(), NewValue::Value("12.34".into())),
                        ("email".into(), NewValue::Null),
                    ],
                    keys: vec![first.clone()],
                    types: types.clone(),
                    held: Vec::new(),
                    ..Default::default()
                },
                PendingRow {
                    schema: edit.schema.clone(),
                    table: edit.table.clone(),
                    sets: vec![("name".into(), NewValue::Value(String::new().into()))],
                    keys: vec![second.clone()],
                    types: types.clone(),
                    held: Vec::new(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        assert!(
            crate::sql::is_generated_write_on(Engine::DuckDb, &batch),
            "{batch}"
        );
        connection.query(&batch, Fetch::default()).unwrap();
        let after = connection
            .query(
                &format!(
                    "SELECT name, active, balance, email FROM accounts WHERE id IN ({}, {}) ORDER BY id",
                    first.1, second.1
                ),
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(
            cells(&after)[0],
            [Some("o'hara"), Some("false"), Some("12.34"), None]
        );
        assert_eq!(cells(&after)[1][0], Some(""));

        // Nothing references a score, where a referenced account refuses it.
        let scores = connection
            .query("SELECT * FROM analytics.account_scores", Fetch::default())
            .unwrap();
        let edit = scores.edit.expect("account_scores has a primary key");
        let key = scores.rows[0].get(edit.keys[0]).unwrap();
        let delete = crate::sql::delete_row(
            Engine::DuckDb,
            &edit.schema,
            &edit.table,
            &[("account_id", key)],
            &[],
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(Engine::DuckDb, &delete));
        assert!(crate::sql::delete_matches_key_on(
            Engine::DuckDb,
            &delete,
            &["account_id"]
        ));
        let deleted = connection.query(&delete, Fetch::default()).unwrap();
        assert_eq!(deleted.rows_affected, Some(1));
    }

    #[test]
    fn a_default_dbdelve_assigns_passes_its_gate_and_lands() {
        use crate::result_grid::NewValue;
        assert!(Engine::DuckDb.assigns_default());
        let connection = memory(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, note VARCHAR DEFAULT 'unset');
             INSERT INTO t VALUES (1, 'set');",
        );
        let statement = crate::sql::update_row(
            Engine::DuckDb,
            "main",
            "t",
            &[("note", NewValue::Default)],
            &[("id", "1")],
            &[],
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(
            Engine::DuckDb,
            &statement
        ));
        connection.query(&statement, Fetch::default()).unwrap();
        let read = connection
            .query("SELECT note FROM t", Fetch::default())
            .unwrap();
        assert_eq!(cells(&read), [[Some("unset")]]);
    }

    fn at(connection: &Connection) -> Vec<Option<String>> {
        let result = connection
            .query(
                "SELECT current_database(), current_schema()",
                Fetch::default(),
            )
            .unwrap();
        result.rows[0]
            .iter()
            .map(|cell| cell.map(str::to_string))
            .collect()
    }

    fn on(database: &str, schema: &str) -> Vec<Option<String>> {
        vec![Some(database.into()), Some(schema.into())]
    }

    #[test]
    fn the_attached_databases_are_listed_without_duckdbs_own() {
        let connection = memory("ATTACH ':memory:' AS other");
        let databases = connection.databases().unwrap();
        assert_eq!(databases.names, ["memory", "other"]);
        assert_eq!(databases.current.as_deref(), Some("memory"));
        // An attachment belongs to the instance, so a connection cloned from
        // it sees one made on another.
        let beside = connection.alongside().unwrap();
        assert_eq!(beside.databases().unwrap().names, ["memory", "other"]);
    }

    #[test]
    fn a_use_moves_only_the_connection_that_ran_it() {
        let connection = memory("ATTACH ':memory:' AS other; USE other");
        assert_eq!(at(&connection), on("other", "main"));
        assert_eq!(at(&connection.alongside().unwrap()), on("memory", "main"));
        assert_eq!(connection.current_database(), "memory");
        assert_eq!(connection.current_schema(), "main");
    }

    #[test]
    fn a_picked_database_and_schema_reach_every_connection() {
        let connection = memory(
            "ATTACH ':memory:' AS other;
             CREATE SCHEMA other.s;
             CREATE TABLE other.s.t (id INTEGER PRIMARY KEY);
             INSERT INTO other.s.t VALUES (1), (2);",
        );
        let profile = super::super::Connection::DuckDb(connection.clone());
        profile.set_database(Some("other"));
        profile.apply_schema().unwrap();
        assert_eq!(at(&connection), on("other", "main"));
        assert_eq!(at(&connection.alongside().unwrap()), on("other", "main"));

        profile.set_schema(Some("s"));
        profile.apply_schema().unwrap();
        assert_eq!(at(&connection), on("other", "s"));
        assert_eq!(count(&connection), "2");
        let super::super::Connection::DuckDb(apart) = profile.apart().unwrap() else {
            unreachable!()
        };
        assert_eq!(at(&apart), on("other", "s"));
        let beside = profile
            .query_alongside(
                "SELECT count(*) FROM t",
                &super::super::CancelToken::alongside(),
                Fetch::default(),
                false,
            )
            .unwrap();
        assert_eq!(cells(&beside), [[Some("2")]]);

        // A database picked again leaves the schema picked in the other.
        profile.set_database(None);
        profile.apply_schema().unwrap();
        assert_eq!(at(&connection), on("memory", "main"));
        assert_eq!(at(&connection.alongside().unwrap()), on("memory", "main"));
    }

    #[test]
    fn a_pick_is_not_undone_by_a_rollback_but_waits_out_an_aborted_transaction() {
        let connection = memory("ATTACH ':memory:' AS other");
        connection.query("BEGIN", Fetch::default()).unwrap();
        connection.set_database(Some("other"));
        connection.apply_pick().unwrap();
        connection.query("ROLLBACK", Fetch::default()).unwrap();
        assert_eq!(at(&connection), on("other", "main"));

        connection.query("BEGIN", Fetch::default()).unwrap();
        connection
            .query("SELECT * FROM nowhere", Fetch::default())
            .unwrap_err();
        connection.set_database(None);
        connection.apply_pick().unwrap_err();
    }

    #[test]
    fn a_pick_that_is_no_longer_there_opens_where_nothing_was_picked() {
        let open = || Connection::open(IN_MEMORY, 0).unwrap();
        let gone = open().opened_on(Some("gone".into()), Some("s".into()));
        assert!(gone.unpicked().unwrap().database);
        assert!(
            gone.unpicked().unwrap().message.contains("gone"),
            "{:?}",
            gone.unpicked()
        );
        assert_eq!(at(&gone), on("memory", "main"));
        assert_eq!(at(&gone.alongside().unwrap()), on("memory", "main"));

        let schema = open().opened_on(Some("memory".into()), Some("nowhere".into()));
        assert!(!schema.unpicked().unwrap().database);
        assert_eq!(at(&schema), on("memory", "main"));
        let schema = open().opened_on(None, Some("nowhere".into()));
        assert!(!schema.unpicked().unwrap().database);
        assert_eq!(at(&schema.alongside().unwrap()), on("memory", "main"));

        let here = open().opened_on(Some("memory".into()), Some("main".into()));
        assert_eq!(here.unpicked(), None);
        assert_eq!(at(&here), on("memory", "main"));

        let profile = super::super::Connection::open(super::super::ConnectionConfig::DuckDb {
            path: IN_MEMORY.into(),
            statement_timeout: 0,
            database: "gone".into(),
            schema: None,
        })
        .unwrap();
        assert!(profile.unpicked().is_some_and(|unpicked| unpicked.database));
        let nothing = super::super::Connection::open(super::super::ConnectionConfig::duckdb(
            IN_MEMORY.into(),
            0,
        ))
        .unwrap();
        assert_eq!(nothing.unpicked(), None);
        let super::super::Connection::DuckDb(nothing) = nothing else {
            unreachable!()
        };
        // Nothing picked runs nothing: the search path is DuckDB's own.
        let path = nothing
            .query("SELECT current_setting('search_path')", Fetch::default())
            .unwrap();
        assert_eq!(cells(&path), [[Some("")]]);
    }

    #[test]
    fn a_picked_schema_keeps_a_plain_select_of_its_tables_editable() {
        let connection = seeded();
        connection.set_schema(Some("analytics"));
        connection.apply_pick().unwrap();
        for connection in [connection.clone(), connection.alongside().unwrap()] {
            let edit = edit_of(&connection, "SELECT * FROM account_scores").expect("in analytics");
            assert_eq!(
                (edit.schema.as_str(), edit.table.as_str()),
                ("analytics", "account_scores")
            );
            let preview =
                crate::explorer::preview_sql(Engine::DuckDb, "main", "accounts", "", Some(10), 0);
            let edit = edit_of(&connection, &preview).expect("qualified");
            assert_eq!(
                (edit.schema.as_str(), edit.table.as_str()),
                ("main", "accounts")
            );
            // DuckDB finds it in `main` once the picked schema has no such
            // table; the trace looks only where the name resolves first.
            assert_eq!(edit_of(&connection, "SELECT * FROM accounts"), None);
        }
    }

    #[test]
    fn a_picked_database_traces_its_own_tables_and_an_edit_lands_there() {
        use crate::result_grid::NewValue;
        let connection = memory(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a VARCHAR);
             INSERT INTO t VALUES (1, 'home');
             ATTACH ':memory:' AS other;
             CREATE TABLE other.main.t (id INTEGER PRIMARY KEY, a VARCHAR);
             INSERT INTO other.main.t VALUES (1, 'away');",
        );
        connection.set_database(Some("other"));
        connection.apply_pick().unwrap();
        let read = connection
            .query("SELECT * FROM t", Fetch::default())
            .unwrap();
        assert_eq!(cells(&read), [[Some("1"), Some("away")]]);
        let edit = read.edit.expect("other's t has a key");
        assert_eq!((edit.schema.as_str(), edit.table.as_str()), ("main", "t"));
        let update = crate::sql::update_row(
            Engine::DuckDb,
            &edit.schema,
            &edit.table,
            &[("a", NewValue::Value("edited".into()))],
            &[("id", "1")],
            &[],
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(Engine::DuckDb, &update));
        connection.query(&update, Fetch::default()).unwrap();
        let both = connection
            .query(
                "SELECT (SELECT a FROM memory.main.t), (SELECT a FROM other.main.t)",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(cells(&both), [[Some("home"), Some("edited")]]);
        assert_eq!(edit_of(&connection, "SELECT * FROM memory.main.t"), None);
    }

    #[test]
    fn a_name_whose_resolution_is_in_doubt_is_not_editable_after_a_pick() {
        for (setup, sql) in [
            // Set by hand: DuckDB looks in the first that has the name.
            (
                "SET search_path = 'memory.analytics,memory.main'",
                "SELECT * FROM account_scores",
            ),
            // A bare name could be a database's or a schema's.
            (
                "SET search_path = 'analytics'",
                "SELECT * FROM account_scores",
            ),
            (
                "USE memory.analytics; CREATE TEMP TABLE account_scores (account_id INTEGER PRIMARY KEY)",
                "SELECT * FROM account_scores",
            ),
            // `"analytics"."account_scores"` would name this database too.
            (
                "ATTACH ':memory:' AS analytics; USE memory.analytics",
                "SELECT * FROM account_scores",
            ),
        ] {
            let connection = seeded();
            connection.query(setup, Fetch::default()).unwrap();
            assert_eq!(edit_of(&connection, sql), None, "{setup}");
        }
    }

    #[test]
    fn a_database_named_in_capitals_or_quotes_is_traced_after_a_use() {
        let connection = memory(
            r#"ATTACH ':memory:' AS "Other";
               CREATE SCHEMA "Other"."Odd ""S""";
               CREATE TABLE "Other"."Odd ""S""".t (id INTEGER PRIMARY KEY);"#,
        );
        connection.set_database(Some("Other"));
        connection.set_schema(Some(r#"Odd "S""#));
        connection.apply_pick().unwrap();
        let edit = edit_of(&connection, "SELECT * FROM t").expect("in the picked schema");
        assert_eq!(
            (edit.schema.as_str(), edit.table.as_str()),
            (r#"Odd "S""#, "t")
        );
    }

    #[test]
    fn a_search_path_is_read_entry_by_entry_and_part_by_part() {
        assert_eq!(
            search_path(r#"Other."Odd ""S""",memory.main"#),
            Some(vec![
                vec!["Other".to_string(), r#"Odd "S""#.to_string()],
                vec!["memory".to_string(), "main".to_string()],
            ])
        );
        assert_eq!(
            search_path(r#""a,b".c"#),
            Some(vec![vec!["a,b".to_string(), "c".to_string()]])
        );
        assert_eq!(search_path(r#""open"#), None);
        assert_eq!(search_path("a..b"), None);
    }

    #[test]
    fn an_import_round_trips_on_duckdb() {
        crate::import_rows::live_round_trip(
            super::super::Connection::DuckDb(memory("")),
            Engine::DuckDb,
            "main",
        );
    }

    struct ScratchDir(std::path::PathBuf);

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn conformance_subject() -> crate::db::conformance::Subject {
        use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "dbdelve-conformance-duckdb-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("a scratch directory nobody else made");
        let scratch = ScratchDir(directory);
        let path = scratch.0.join("dbdelve.duckdb");
        drop(::duckdb::Connection::open(&path).expect("the scratch database is created"));
        let connection = crate::db::Connection::open(crate::db::ConnectionConfig::duckdb(
            path.to_string_lossy().into_owned(),
            0,
        ))
        .expect("connection should open");
        let seed = std::fs::read_to_string(fixture("001-dbdelve-demo.sql")).expect("the dev seed");
        connection
            .query(&seed, &crate::db::CancelToken::default(), Fetch::default())
            .expect("the dev seed loads");
        Subject {
            connection,
            engine: Engine::DuckDb,
            schema: "main",
            relation: "conformance",
            key: "id",
            columns: &COLUMNS,
            read: sql_read(Engine::DuckDb, "main"),
            endless: "SELECT range FROM range(1000000000)",
            runaway: "SELECT count(*) FROM range(100000000000)",
            read_only_refusal: "",
            fixture: Some(Fixture {
                types: &[
                    "integer",
                    "varchar",
                    "bigint",
                    "decimal(10,2)",
                    "double",
                    "boolean",
                    "date",
                    "timestamp",
                    "blob",
                ],
                renders: &[
                    ("flag", 0, "true"),
                    ("flag", 1, "false"),
                    ("bytes", 0, "00ff"),
                    ("bytes", 1, "deadbeef"),
                ],
                find: Box::new(|key| sql_find(Engine::DuckDb, "main", key)),
            }),
            _guard: Some(Box::new(scratch)),
        }
    }

    crate::db::conformance::suite!(
        live_conformance,
        "builds the DuckDB development seed into a temp file",
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
    );
}

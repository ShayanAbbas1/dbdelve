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
//! file would have to keep up with.
//!
//! Nothing here is editable in the grid yet: DuckDB does not say which table a
//! result column was read from, and a predicate dbdelve cannot read off the
//! grid is one it must not write. Inserting from an object tab works.

use std::fmt::Write as _;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duckdb::arrow::array::{Array, ArrayRef, Int64Array};
use duckdb::arrow::compute::cast;
use duckdb::arrow::datatypes::{DataType, FieldRef};
use duckdb::arrow::error::ArrowError;
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use duckdb::{Config, InterruptHandle};

use super::{
    Catalog, Column, DbError, Engine, Fetch, QueryResult, Reference, RowBuilder, Stopped,
    Structure, assemble_catalog, assemble_foreign_keys, assemble_references, assemble_structure,
    plain_error, required_cell, terminated,
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
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let connection = match path.trim() {
            "" | IN_MEMORY => duckdb::Connection::open_in_memory()
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
                duckdb::Connection::open_with_flags(&path, Config::default()).map_err(|error| {
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
        Ok(Self {
            interrupt: connection.interrupt_handle(),
            connection: Arc::new(Mutex::new(connection)),
            origin: Arc::new(Mutex::new(origin)),
            statement_timeout: (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout))),
        })
    }

    /// A second connection to the same database, for a statement to run on
    /// beside this one's; see [`super::Connection::query_alongside`].
    pub fn alongside(&self) -> Result<Self, DbError> {
        let connection = lock(&self.origin)?
            .try_clone()
            .map_err(|error| plain_error(error.to_string()))?;
        Ok(Self {
            interrupt: connection.interrupt_handle(),
            connection: Arc::new(Mutex::new(connection)),
            ..self.clone()
        })
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
        run(&connection, sql, fetch).map_err(|error| rolled_back(&connection, sql, error))
    }

    /// The tables and views of the database the connection opened, less the
    /// catalogs DuckDB keeps for itself.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let relations = self.query(
            "SELECT schema_name, table_name AS relation_name, 'table' AS relation_kind,
                    estimated_size AS row_estimate
             FROM duckdb_tables()
             WHERE NOT internal AND database_name = current_database()
             UNION ALL
             SELECT schema_name, view_name, 'view', NULL
             FROM duckdb_views()
             WHERE NOT internal AND database_name = current_database()
             ORDER BY schema_name, relation_name",
            Fetch::default(),
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
        let columns = self.query(
            &format!(
                "SELECT column_name, lower(data_type) AS data_type,
                        CASE WHEN is_nullable THEN 'yes' ELSE 'no' END AS nullable,
                        COALESCE(column_default, '') AS column_default
                 FROM duckdb_columns()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                 ORDER BY column_index"
            ),
            Fetch::default(),
        )?;
        let indexes = self.query(
            &format!(
                "SELECT index_name AS object_name, COALESCE(sql, '') AS definition
                 FROM duckdb_indexes()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                 ORDER BY index_name"
            ),
            Fetch::default(),
        )?;
        // Written out of the column lists rather than taken from
        // `constraint_text`, which spells a key `PRIMARY KEY(id)`: the
        // Structure view and `Structure::primary_key` read the shape every
        // other engine gives them, `PRIMARY KEY (id)`.
        let constraints = self.query(
            &format!(
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
            ),
            Fetch::default(),
        )?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&self.query(
            &format!(
                "SELECT unnest(constraint_column_names) AS column_name,
                        schema_name AS referenced_schema,
                        referenced_table,
                        unnest(referenced_column_names) AS referenced_column
                 FROM duckdb_constraints()
                 WHERE database_name = current_database()
                   AND schema_name = {schema_literal} AND table_name = {relation_literal}
                   AND constraint_type = 'FOREIGN KEY'"
            ),
            Fetch::default(),
        )?)?;
        Ok(structure)
    }

    /// Who points at this relation. A DuckDB foreign key names its parent by
    /// table alone, in its own schema.
    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        let result = self.query(
            &format!(
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
            ),
            Fetch::default(),
        )?;
        assemble_references(&result)
    }

    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        let (schema_literal, relation_literal) = (
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        );
        let listed = self.query(
            &format!(
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
            ),
            Fetch::default(),
        )?;
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
            data_type: Some(type_name(field.data_type())),
        })
        .collect();

    // A command answers with a column of its own: `Count`, the rows a write
    // changed, or `Success`, empty. Which statements those are is the
    // parser's call, since a `SELECT count(*) AS "Count"` has the same shape.
    let command = match schema.fields().as_ref() {
        [field] => matches!(
            (field.name().as_str(), field.data_type()),
            ("Count", DataType::Int64) | ("Success", DataType::Boolean)
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
            .map(zones_as_offsets)
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

/// `array` with every zone a timestamp names spelled as its offset. DuckDB
/// hands a `TIMESTAMPTZ` over as UTC under the zone's name, and Arrow formats
/// a named zone only with `chrono-tz`, which links the whole zone database in
/// for the one name DuckDB ever gives. Inside a list, a struct or a map too.
fn zones_as_offsets(array: &ArrayRef) -> Result<ArrayRef, ArrowError> {
    match offset_type(array.data_type()) {
        Some(data_type) => cast(array, &data_type),
        None => Ok(array.clone()),
    }
}

fn offset_type(data_type: &DataType) -> Option<DataType> {
    let item = |field: &FieldRef| -> Option<FieldRef> {
        Some(Arc::new(
            field
                .as_ref()
                .clone()
                .with_data_type(offset_type(field.data_type())?),
        ))
    };
    Some(match data_type {
        DataType::Timestamp(unit, Some(_)) => DataType::Timestamp(*unit, Some("+00:00".into())),
        DataType::List(field) => DataType::List(item(field)?),
        DataType::LargeList(field) => DataType::LargeList(item(field)?),
        DataType::FixedSizeList(field, size) => DataType::FixedSizeList(item(field)?, *size),
        DataType::Map(entries, sorted) => DataType::Map(item(entries)?, *sorted),
        DataType::Struct(fields) => {
            let offset: Vec<Option<FieldRef>> = fields.iter().map(item).collect();
            if offset.iter().all(Option::is_none) {
                return None;
            }
            DataType::Struct(
                fields
                    .iter()
                    .zip(offset)
                    .map(|(field, offset)| offset.unwrap_or_else(|| field.clone()))
                    .collect(),
            )
        }
        _ => return None,
    })
}

/// An Arrow type under the name DuckDB gives it, lowercased as every engine's
/// type names are in the grid, so `db::is_numeric_type` and the Structure view
/// read the same words DuckDB's own shell prints.
fn type_name(data_type: &DataType) -> String {
    match data_type {
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
        DataType::List(item) | DataType::LargeList(item) => {
            format!("{}[]", type_name(item.data_type()))
        }
        DataType::Struct(_) => "struct".into(),
        DataType::Map(..) => "map".into(),
        DataType::Dictionary(_, value) => type_name(value),
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
                        {'at': TIMESTAMPTZ '2026-01-03 10:04:05+02'} AS zoned",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(result.columns[1].data_type.as_deref(), Some("decimal(5,2)"));
        assert_eq!(
            cells(&result),
            [[
                Some("[1, NULL]"),
                Some("12.50"),
                None,
                Some("2026-01-03 10:04:05"),
                Some("2026-01-03"),
                Some("{at: 2026-01-03 08:04:05+00:00}")
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
        assert!(
            facturas
                .indexes
                .iter()
                .any(|index| index.name == "facturas_cliente")
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

    #[test]
    fn an_import_round_trips_on_duckdb() {
        crate::import_rows::live_round_trip(
            super::super::Connection::DuckDb(memory("")),
            Engine::DuckDb,
            "main",
        );
    }
}

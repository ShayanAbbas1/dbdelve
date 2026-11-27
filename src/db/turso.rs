use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use turso_core::{Numeric, Value};
use turso_sdk_kit::rsapi::{
    TursoConnection, TursoDatabase, TursoDatabaseConfig, TursoError, TursoStatusCode,
};

use super::sqlite::{Schema, render_real};
use super::{
    Catalog, Column, DbError, Engine, Fetch, QueryResult, Reference, RowBuilder, SchemaGraph,
    Stopped, Structure, plain_error,
};

pub const INTERRUPTED: &str = "interrupted";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Connection {
    connect: Arc<dyn Fn() -> Result<Arc<TursoConnection>, TursoError> + Send + Sync>,
    connection: Arc<TursoConnection>,
    running: Arc<Mutex<()>>,
    /// The statements the profile's gated startup script came to, replayed on
    /// every session opened beside this one. The `connect` closure above is
    /// built before any script exists and holds none.
    startup: Arc<[String]>,
    /// Opened by [`Self::open_query_only`]: every handle [`Self::alongside`]
    /// derives sets the pragma again, since it is per connection.
    query_only: bool,
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        Self::open_inner(path, statement_timeout)
    }

    /// Opened for an agent: `PRAGMA query_only` on this connection makes Turso
    /// refuse a write on it.
    ///
    /// Not `OpenFlags::ReadOnly`: `turso_core` keeps one `Database` per file for
    /// the whole process, so the flag is ignored when the user already has the
    /// file open here (the write goes through), and when the agent opens first
    /// it makes the user's own connection read-only. The pragma is per
    /// connection and touches neither. The agent could undo it by statement, so
    /// `sql::agent_gate` stays the boundary -- this is as strong as the server
    /// engines' session hold, no stronger. It is set here and not through
    /// `read_only_statement`, so the user's own Read-only mode is unchanged.
    pub fn open_query_only(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let connection = Self {
            query_only: true,
            ..Self::open_inner(path, statement_timeout)?
        };
        connection.query("PRAGMA query_only = 1", Fetch::default())?;
        Ok(connection)
    }

    fn open_inner(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let path = crate::store::home_expanded(path);
        if !path.exists() {
            return Err(plain_error(format!(
                "No database file at {}",
                path.display()
            )));
        }
        let text = path
            .to_str()
            .ok_or_else(|| plain_error(format!("{} is not valid UTF-8.", path.display())))?;
        let shared = std::path::Path::new(&format!("{text}-tshm")).exists();
        let failed =
            |reason: String| plain_error(format!("Cannot open {}: {reason}", path.display()));
        if !shared {
            return Self::open_at(text, None, statement_timeout)
                .map_err(|error| failed(error.to_string()));
        }
        let features = Some("multiprocess_wal");
        Self::open_at(text, features, statement_timeout).or_else(|first| {
            Self::open_at(text, None, statement_timeout).map_err(|second| {
                failed(format!(
                    "{first}; opening without the shared WAL failed too: {second}"
                ))
            })
        })
    }

    fn open_at(
        path: &str,
        features: Option<&str>,
        statement_timeout: u32,
    ) -> Result<Self, TursoError> {
        let database = TursoDatabase::new(TursoDatabaseConfig {
            path: path.to_string(),
            experimental_features: features.map(str::to_string),
            async_io: false,
            encryption: None,
            vfs: Default::default(),
            io: None,
            db_file: None,
            page_codec: None,
            open_flags: Default::default(),
        });
        let _ = database.open()?;
        let statement_timeout = Duration::from_secs(u64::from(statement_timeout));
        let connect: Arc<dyn Fn() -> Result<Arc<TursoConnection>, TursoError> + Send + Sync> =
            Arc::new(move || {
                let connection = database.connect()?;
                connection.set_busy_timeout(BUSY_TIMEOUT);
                connection.set_query_timeout(statement_timeout);
                Ok(connection)
            });
        Ok(Self {
            connection: connect()?,
            connect,
            running: Arc::new(Mutex::new(())),
            startup: Arc::default(),
            query_only: false,
        })
    }

    pub fn alongside(&self) -> Result<Self, DbError> {
        let connection = (self.connect)().map_err(|error| plain_error(error.to_string()))?;
        let beside = Self {
            connection,
            running: Arc::new(Mutex::new(())),
            ..self.clone()
        };
        if beside.query_only {
            beside.query("PRAGMA query_only = 1", Fetch::default())?;
        }
        beside.replay()?;
        Ok(beside)
    }

    /// Runs `statements` now, in the order written, and keeps them for every
    /// session [`Self::alongside`] opens. The first error stops the rest.
    pub fn with_startup(mut self, statements: Vec<String>) -> Result<Self, DbError> {
        self.startup = statements.into();
        self.replay()?;
        Ok(self)
    }

    /// The startup statements on this session, rows discarded.
    fn replay(&self) -> Result<(), DbError> {
        self.startup
            .iter()
            .try_for_each(|statement| self.internal_query(statement).map(|_| ()))
    }

    pub fn cancel(&self) -> Result<(), DbError> {
        self.connection.interrupt();
        Ok(())
    }

    pub fn query(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        self.run(sql, fetch)
    }

    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.run(sql, Fetch::default())
    }

    fn run(&self, sql: &str, fetch: Fetch) -> Result<QueryResult, DbError> {
        let _running = self.running.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;
        let _held = fetch.hold()?;

        let mut result = QueryResult::default();
        let mut filled = None;
        let mut past_limit = false;
        let outside_a_transaction = self.connection.get_auto_commit();

        let outcome = (|| -> Result<(), DbError> {
            let mut rest = sql;
            let started = Instant::now();
            let mut builder = RowBuilder::default();
            while let Some((mut statement, consumed)) =
                self.connection.prepare_first(rest).map_err(query_error)?
            {
                rest = &rest[consumed..];

                let columns: Vec<Column> = (0..statement.column_count())
                    .map(|index| {
                        Ok(Column {
                            name: statement.column_name(index)?,
                            data_type: statement
                                .column_decltype(index)
                                .map(|declared| declared.to_lowercase()),
                        })
                    })
                    .collect::<Result<_, TursoError>>()
                    .map_err(query_error)?;

                if columns.is_empty() {
                    let executed = statement.execute(None).map_err(query_error)?;
                    result.rows_affected = Some(executed.rows_changed);
                    continue;
                }

                if let Some(feed) = fetch.feed {
                    feed.begin(columns.clone());
                }
                let mut rows = Vec::new();
                let mut bytes = 0;
                let (mut returned, mut kept) = (0, 0);
                filled = None;
                loop {
                    match statement.step(None).map_err(query_error)? {
                        TursoStatusCode::Done => break,
                        TursoStatusCode::Io => {
                            return Err(plain_error(
                                "The driver asked for asynchronous IO on a blocking connection."
                                    .into(),
                            ));
                        }
                        TursoStatusCode::Row => {}
                    }
                    returned += 1;
                    if fetch.limit.is_some_and(|limit| kept >= limit) {
                        if fetch.reads_only {
                            result.stopped = Some(Stopped::AtLimit);
                            statement.reset().map_err(query_error)?;
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
                        let value = statement.row_value(index).map_err(query_error)?;
                        builder.push(render(&value).as_deref());
                    }
                    let cells = builder.finish()?;
                    bytes += cells.text_len();
                    match fetch.feed {
                        Some(feed) => feed.push(cells),
                        None => rows.push(cells),
                    }
                }

                result.rows_affected = Some(returned as u64);
                result.capped_from =
                    (result.stopped.is_none() && returned > kept).then_some(returned);
                past_limit = returned > kept;
                result.columns = columns;
                result.rows = rows;
                result.bytes = bytes;
                if result.stopped.is_some() {
                    break;
                }
            }
            result.elapsed = match filled {
                Some(filled) if past_limit => filled,
                _ => started.elapsed(),
            };
            Ok(())
        })();

        if let Err(error) = outcome {
            return Err(self.rolled_back(outside_a_transaction, error));
        }
        result.transaction_open = Some(!self.connection.get_auto_commit());
        Ok(result)
    }

    fn rolled_back(&self, outside_a_transaction: bool, error: DbError) -> DbError {
        if !outside_a_transaction || self.connection.get_auto_commit() {
            return error;
        }
        let rollback = self
            .connection
            .prepare_single("ROLLBACK")
            .and_then(|mut statement| statement.execute(None));
        let outcome = match rollback {
            Ok(_) => "The transaction the batch opened was rolled back; nothing it wrote remains."
                .to_string(),
            Err(failure) => format!(
                "The transaction the batch opened is still open: the rollback failed too. {}",
                failure
            ),
        };
        DbError {
            message: format!("{}\n\n{outcome}", error.message),
            position: error.position,
        }
    }

    fn schema(&self) -> Schema<impl Fn(&str) -> Result<QueryResult, DbError> + '_> {
        Schema {
            run: |sql: &str| self.internal_query(sql),
            engine: Engine::Turso,
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

fn render(value: &Value) -> Option<Cow<'_, str>> {
    Some(match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(integer)) => integer.to_string().into(),
        Value::Numeric(Numeric::Float(real)) => render_real(f64::from(*real)).into(),
        Value::Text(text) => text.as_str().into(),
        Value::Blob(bytes) => format!("x'{}'", hex::encode_upper(bytes)).into(),
    })
}

fn query_error(error: TursoError) -> DbError {
    DbError {
        message: error.to_string(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::IndexKind;

    fn memory(setup: &str) -> Connection {
        let connection = Connection::open_at(":memory:", None, 0).expect("in-memory database");
        if !setup.is_empty() {
            connection.query(setup, Fetch::default()).expect("setup");
        }
        connection
    }

    fn cells(result: &QueryResult) -> Vec<Vec<Option<&str>>> {
        result
            .rows
            .iter()
            .map(|row| {
                (0..result.columns.len())
                    .map(|index| row.get(index))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_schema_graph_reads_keys_through_the_pragma_functions() {
        let graph = memory(crate::db::sqlite::graph_fixture::GRAPH_FIXTURE)
            .schema_graph("main")
            .expect("the graph should read");
        crate::db::sqlite::graph_fixture::assert_graph_fixture(&graph);
    }

    #[test]
    fn a_graph_reads_a_descending_integer_key_as_nullable_and_a_key_beside_an_index_named_alike() {
        let graph = memory(crate::db::sqlite::graph_fixture::EDGE_FIXTURE)
            .schema_graph("main")
            .expect("the graph should read");
        crate::db::sqlite::graph_fixture::assert_edge_fixture(&graph);
    }

    #[test]
    fn every_value_renders_as_something_a_statement_can_contain() {
        let connection = memory("");
        let result = connection
            .query(
                "SELECT 1, 1.0, 2.5, 'a''b', x'ab01', NULL",
                Fetch::default(),
            )
            .unwrap();
        assert_eq!(
            cells(&result),
            [[
                Some("1"),
                Some("1.0"),
                Some("2.5"),
                Some("a'b"),
                Some("x'AB01'"),
                None
            ]]
        );
    }

    #[test]
    fn a_declared_type_is_a_tag_and_an_expression_has_none() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);");
        let result = connection
            .query("SELECT id, name, id + 1 FROM t", Fetch::default())
            .unwrap();
        let types: Vec<_> = result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref())
            .collect();
        assert_eq!(types, [Some("integer"), Some("text"), None]);
    }

    #[test]
    fn a_query_counts_its_rows_and_a_command_counts_what_it_changed() {
        let connection = memory("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2), (3);");
        let selected = connection
            .query("SELECT a FROM t", Fetch::default())
            .unwrap();
        assert_eq!(selected.rows_affected, Some(3));
        let updated = connection
            .query("UPDATE t SET a = a + 1 WHERE a > 1", Fetch::default())
            .unwrap();
        assert_eq!(updated.rows_affected, Some(2));
    }

    #[test]
    fn a_multi_statement_selection_keeps_the_last_result() {
        let connection = memory("");
        let result = connection
            .query("SELECT 1; SELECT 2, 3", Fetch::default())
            .unwrap();
        assert_eq!(cells(&result), [[Some("2"), Some("3")]]);
    }

    #[test]
    fn a_statement_the_engine_refuses_reports_the_engines_own_words() {
        let connection = memory("");
        let error = connection
            .query("SELECT * FROM nosuch", Fetch::default())
            .unwrap_err();
        assert_eq!(error.message, "Parse error: no such table: nosuch");
        assert_eq!(error.position, None);
    }

    #[test]
    fn a_row_limit_keeps_the_first_rows_and_counts_the_rest() {
        let connection =
            memory("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2), (3), (4);");
        let kept = |reads_only| {
            connection
                .query(
                    "SELECT a FROM t ORDER BY a",
                    Fetch {
                        limit: Some(2),
                        reads_only,
                        ..Fetch::default()
                    },
                )
                .unwrap()
        };
        let counted = kept(false);
        assert_eq!(cells(&counted), [[Some("1")], [Some("2")]]);
        assert_eq!(counted.capped_from, Some(4));
        let stopped = kept(true);
        assert_eq!(cells(&stopped), [[Some("1")], [Some("2")]]);
        assert_eq!(stopped.stopped, Some(Stopped::AtLimit));
        assert_eq!(
            connection
                .query("SELECT count(*) FROM t", Fetch::default())
                .unwrap()
                .rows[0]
                .get(0),
            Some("4")
        );
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
        assert!(error.message.contains("no such table: nosuch"), "{error:?}");
        assert!(error.message.contains("rolled back"), "{error:?}");
        let left = connection
            .query("SELECT count(*) FROM t", Fetch::default())
            .unwrap();
        assert_eq!(left.rows[0].get(0), Some("0"));
        connection
            .query("BEGIN; INSERT INTO t VALUES (1); COMMIT;", Fetch::default())
            .expect("the next batch opens its own transaction");
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
        assert!(!connection.connection.get_auto_commit());
        connection.query("ROLLBACK", Fetch::default()).unwrap();
    }

    const RUNAWAY: &str = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) \
                           SELECT count(*) FROM r";

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
        assert_eq!(after.rows[0].get(0), Some("7"));
    }

    #[test]
    fn a_statement_timeout_stops_a_runaway_on_its_own() {
        let connection = Connection::open_at(":memory:", None, 1).unwrap();
        let started = Instant::now();
        let error = connection.query(RUNAWAY, Fetch::default()).unwrap_err();
        assert_eq!(error.message, INTERRUPTED);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_cancel_reaches_a_statement_run_alongside_and_spares_the_one_beside_it() {
        let connection = memory("");
        let beside = connection.alongside().unwrap();
        let stopper = beside.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = beside.query(RUNAWAY, Fetch::default()).unwrap_err();
        thread.join().unwrap();
        assert_eq!(error.message, INTERRUPTED);
        assert_eq!(
            connection.query("SELECT 1", Fetch::default()).unwrap().rows[0].get(0),
            Some("1")
        );
    }

    #[test]
    fn the_catalog_lists_tables_and_views() {
        let connection = memory(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE VIEW v AS SELECT id FROM a;",
        );
        let catalog = connection.catalog().unwrap();
        let names: Vec<_> = catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema
                    .relations
                    .iter()
                    .map(|relation| relation.name.as_str())
            })
            .collect();
        assert_eq!(names, ["a", "v"]);
    }

    #[test]
    fn structure_reads_columns_nullability_defaults_and_keys() {
        let connection = memory(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x');
             CREATE TABLE c (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES p (id));
             CREATE INDEX c_p ON c (p_id);
             CREATE UNIQUE INDEX c_pair ON c (p_id, id);",
        );
        let parent = connection.structure("main", "p").unwrap();
        let columns: Vec<_> = parent
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
        assert_eq!(columns, [("id", false, None), ("name", false, Some("'x'"))]);

        let child = connection.structure("main", "c").unwrap();
        assert_eq!(child.foreign_keys.len(), 1);
        assert_eq!(child.foreign_keys[0].referenced_table, "p");
        let kinds: Vec<_> = child
            .indexes
            .iter()
            .map(|index| (index.name.as_str(), index.kind, index.columns.clone()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("c_p", IndexKind::Plain, vec!["p_id".to_string()]),
                (
                    "c_pair",
                    IndexKind::Unique,
                    vec!["p_id".into(), "id".into()]
                ),
            ]
        );

        let pointing = connection.references("main", "p").unwrap();
        assert_eq!(pointing.len(), 1);
        assert_eq!(pointing[0].table, "c");
    }

    #[test]
    fn the_ddl_is_the_table_then_what_was_made_on_it() {
        let connection = memory(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT);
             CREATE INDEX t_a ON t (a);
             CREATE TABLE other (id INTEGER);",
        );
        let ddl = connection.ddl("main", "t").unwrap();
        assert!(ddl.starts_with("CREATE TABLE t"), "{ddl}");
        assert!(ddl.contains("CREATE INDEX t_a"), "{ddl}");
        assert!(!ddl.contains("other"), "{ddl}");
    }

    #[test]
    fn opening_a_path_that_is_not_there_names_it_and_creates_nothing() {
        let path = std::env::temp_dir().join("dbdelve-turso-missing.db");
        let _ = std::fs::remove_file(&path);
        let error = Connection::open(path.to_str().unwrap(), 0)
            .err()
            .expect("a missing file is an error");
        assert!(error.message.contains("No database file at"), "{error:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_result_is_never_editable() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT);");
        let result = connection
            .query("SELECT id, a FROM t", Fetch::default())
            .unwrap();
        assert!(result.edit.is_none());
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
    fn a_failed_write_inside_the_users_transaction_leaves_it_to_commit() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        connection
            .query("BEGIN; INSERT INTO t VALUES (1)", Fetch::default())
            .unwrap();
        connection
            .query("INSERT INTO t VALUES (1)", Fetch::default())
            .expect_err("a duplicate key");
        let committed = connection.query("COMMIT", Fetch::default());
        assert!(committed.is_ok(), "{committed:?}");
        assert_eq!(count(&connection), "1");
    }

    #[test]
    fn a_cancelled_write_inside_the_users_transaction_makes_it_uncommittable() {
        let connection = memory("CREATE TABLE t (id INTEGER);");
        connection
            .query("BEGIN; INSERT INTO t VALUES (1)", Fetch::default())
            .unwrap();
        let stopper = connection.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel().unwrap();
        });
        let error = connection
            .query(
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) \
                 INSERT INTO t SELECT n FROM r",
                Fetch::default(),
            )
            .unwrap_err();
        thread.join().unwrap();
        assert_eq!(error.message, INTERRUPTED);
        let committed = connection
            .query("COMMIT", Fetch::default())
            .expect_err("the abandoned write poisons the transaction");
        assert!(committed.message.contains("cannot commit"), "{committed:?}");
        assert_eq!(count(&connection), "0");
    }

    #[test]
    fn an_insert_dbdelve_writes_passes_its_gate_and_runs() {
        let connection = memory("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);");
        let statement = crate::sql::insert_row(
            Engine::Turso,
            "main",
            "t",
            &[("id", Some("1")), ("name", Some("o'hara"))],
            &[],
        )
        .unwrap();
        assert!(crate::sql::is_generated_write_on(Engine::Turso, &statement));
        connection.query(&statement, Fetch::default()).unwrap();
        let read = connection
            .query("SELECT name FROM t", Fetch::default())
            .unwrap();
        assert_eq!(read.rows[0].get(0), Some("o'hara"));
    }

    #[test]
    fn a_file_opens_plain_and_opens_again_with_a_shared_wal_sidecar_beside_it() {
        let dir = std::env::temp_dir().join(format!("dbdelve-turso-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.db");
        let beside = dir.join("beside.db");
        let features = cfg!(not(windows)).then_some("multiprocess_wal");
        for (path, features) in [(&plain, None), (&beside, features)] {
            let created = Connection::open_at(path.to_str().unwrap(), features, 0).unwrap();
            created
                .query(
                    "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)",
                    Fetch::default(),
                )
                .unwrap();
        }
        let sidecar = format!("{}-tshm", beside.display());
        if features.is_none() {
            std::fs::write(&sidecar, b"").unwrap();
        }
        assert!(std::path::Path::new(&sidecar).exists());
        for path in [&plain, &beside] {
            let opened = Connection::open(path.to_str().unwrap(), 0).unwrap();
            assert_eq!(count(&opened), "1");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_waits_for_a_lock_another_connection_releases() {
        let dir = std::env::temp_dir().join(format!("dbdelve-turso-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busy.db");
        let holder = Connection::open_at(path.to_str().unwrap(), None, 0).unwrap();
        holder
            .query("CREATE TABLE t (id INTEGER)", Fetch::default())
            .unwrap();
        let waiter = holder.alongside().unwrap();
        holder
            .query("BEGIN; INSERT INTO t VALUES (1)", Fetch::default())
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            holder.query("COMMIT", Fetch::default()).unwrap();
        });
        waiter
            .query("INSERT INTO t VALUES (2)", Fetch::default())
            .unwrap();
        release.join().unwrap();
        assert_eq!(count(&waiter), "2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `cache_size` is per connection, so a session that never ran the script
    /// reads its default and one that did reads the script's value.
    #[test]
    fn a_startup_script_replays_beside_the_connection() {
        let dir =
            std::env::temp_dir().join(format!("dbdelve-turso-startup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("startup.db").to_string_lossy().into_owned();
        let cache_size = |connection: &Connection| {
            let result = connection
                .query("PRAGMA cache_size", Fetch::default())
                .expect("the pragma reads");
            result.rows[0].get(0).map(str::to_string)
        };
        let plain = Connection::open_at(&path, None, 0).unwrap();
        let default = cache_size(&plain);
        assert_ne!(default.as_deref(), Some("4000"));

        let scripted = Connection::open(&path, 0)
            .unwrap()
            .with_startup(vec!["PRAGMA cache_size = 4000".into()])
            .expect("the script runs");
        assert_eq!(cache_size(&scripted).as_deref(), Some("4000"));
        let beside = scripted.alongside().unwrap();
        assert_eq!(cache_size(&beside).as_deref(), Some("4000"));
        assert_eq!(
            cache_size(&plain),
            default,
            "the script is the session's own"
        );
        drop((scripted, beside, plain));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_agent_open_reads_committed_rows_and_refuses_a_write() {
        let dir = std::env::temp_dir().join(format!("dbdelve-turso-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = |path: &str| crate::db::ConnectionConfig::Turso {
            path: path.to_string(),
            statement_timeout: 0,
        };

        // The user's connection is open first.
        let path = dir.join("user-first.db").to_string_lossy().into_owned();
        let user = Connection::open_at(&path, None, 0).unwrap();
        user.query(
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)",
            Fetch::default(),
        )
        .unwrap();
        let user = crate::db::Connection::Turso(user);
        let refusal = crate::db::assert_agent_open_reads_and_refuses_a_write(config(&path), &user);
        assert!(refusal.contains("query_only"), "{refusal}");
        // The user's connection is still writable after the agent's refusal.
        user.query(
            "INSERT INTO t VALUES (3)",
            &crate::db::CancelToken::default(),
            Fetch::default(),
        )
        .expect("the user's connection stays writable");
        drop(user);

        // The agent's connection is open first.
        let path = dir.join("agent-first.db").to_string_lossy().into_owned();
        let setup = Connection::open_at(&path, None, 0).unwrap();
        setup
            .query(
                "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)",
                Fetch::default(),
            )
            .unwrap();
        drop(setup);
        let agent = crate::db::Connection::open_for_agent(config(&path)).unwrap();
        let user = Connection::open(&path, 0).unwrap();
        user.query("INSERT INTO t VALUES (2)", Fetch::default())
            .expect("the user's connection stays writable when the agent opened first");
        let cancel = crate::db::CancelToken::default();
        let rows = agent
            .query("SELECT count(*) FROM t", &cancel, Fetch::default())
            .unwrap();
        assert_eq!(rows.rows[0].get(0).unwrap().to_string(), "2");
        let refusal = agent
            .query("INSERT INTO t VALUES (4)", &cancel, Fetch::default())
            .expect_err("the agent's connection refuses the write");
        assert!(refusal.message.contains("query_only"), "{refusal}");
        drop((agent, user));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_handle_derived_from_an_agent_open_refuses_a_write_too() {
        let dir =
            std::env::temp_dir().join(format!("dbdelve-turso-alongside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db").to_string_lossy().into_owned();
        let user = Connection::open_at(&path, None, 0).unwrap();
        user.query("CREATE TABLE t (id INTEGER)", Fetch::default())
            .unwrap();

        let derived = Connection::open_query_only(&path, 0)
            .unwrap()
            .alongside()
            .unwrap();
        let refusal = derived
            .query("INSERT INTO t VALUES (1)", Fetch::default())
            .expect_err("a handle derived from the agent's refuses the write");
        assert!(refusal.message.contains("query_only"), "{refusal}");
        // One derived from an ordinary open stays writable.
        user.alongside()
            .unwrap()
            .query("INSERT INTO t VALUES (2)", Fetch::default())
            .expect("a handle derived from the user's stays writable");
        drop((derived, user));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn live() -> Option<Connection> {
        let path = std::env::var("dbdelve_TURSO_PATH").ok()?;
        Some(Connection::open(&path, 0).expect("the Turso database named by dbdelve_TURSO_PATH"))
    }

    #[test]
    #[ignore = "needs dbdelve_TURSO_PATH"]
    fn live_an_import_round_trips_on_turso() {
        let connection = live().expect("dbdelve_TURSO_PATH is set");
        crate::import_rows::live_round_trip(
            super::super::Connection::Turso(connection),
            Engine::Turso,
            "main",
        );
    }

    #[test]
    #[ignore = "needs dbdelve_TURSO_PATH"]
    fn live_catalog_structure_and_a_read_round_trip() {
        let connection = live().expect("dbdelve_TURSO_PATH is set");
        let catalog = connection.catalog().expect("the catalog loads");
        let relations: Vec<_> = catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema
                    .relations
                    .iter()
                    .map(|relation| (schema.name.as_str(), relation.name.as_str()))
            })
            .collect();
        assert!(!relations.is_empty(), "the database has no relations");
        assert!(
            relations
                .iter()
                .all(|(_, name)| !name.starts_with("__turso_internal_")),
            "{relations:?}"
        );
        for (schema, relation) in &relations {
            let structure = connection
                .structure(schema, relation)
                .unwrap_or_else(|error| panic!("{relation}: {}", error.message));
            assert!(!structure.columns.is_empty(), "{relation}");
            connection
                .ddl(schema, relation)
                .unwrap_or_else(|error| panic!("{relation}: {}", error.message));
            let sql = format!(
                "SELECT * FROM {} LIMIT 5",
                Engine::Turso.qualified(schema, relation)
            );
            let rows = connection
                .query(&sql, Fetch::default())
                .unwrap_or_else(|error| panic!("{relation}: {}", error.message));
            assert_eq!(rows.columns.len(), structure.columns.len(), "{relation}");
        }
    }

    fn conformance_subject() -> crate::db::conformance::Subject {
        use crate::db::conformance::{COLUMNS, Fixture, Subject, sql_find, sql_read};
        let path = std::env::var("dbdelve_TURSO_PATH").expect("dbdelve_TURSO_PATH is required");
        Subject {
            connection: crate::db::Connection::open(crate::db::ConnectionConfig::Turso {
                path,
                statement_timeout: 0,
            })
            .expect("connection should open"),
            target: crate::db::Target::built_in(Engine::Turso),
            schema: "main",
            relation: "conformance",
            key: "id",
            columns: &COLUMNS,
            read: sql_read(Engine::Turso, "main"),
            endless_rows: None,
            endless: "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
                      SELECT i FROM n",
            runaway: RUNAWAY,
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
                find: Box::new(|key| sql_find(Engine::Turso, "main", key)),
            }),
            _guard: None,
        }
    }

    crate::db::conformance::suite!(
        live_conformance,
        "requires a Turso database file configured through dbdelve_TURSO_PATH, a copy of the SQLite development database",
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

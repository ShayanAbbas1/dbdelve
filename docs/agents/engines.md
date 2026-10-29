# Engine divergences

Decided, and not to be re-litigated:

- **MariaDB is an `Engine` of its own on the MySQL driver.** `mysql.rs` serves
  both and is told which at `open`; what this file says of MySQL holds for
  MariaDB unless a bullet names it. A profile stored before it existed says
  `mysql` and stays MySQL. The `live_` tests in `mysql.rs` run once per server
  (`on_mysql::`, `on_mariadb::`).
- **Ten functions generate the SQL DBDelve runs, and every one quotes through the engine's
  `EngineSpec` (`Engine::quote_*` forwards to it on SQL engines):**
  `explorer::preview_sql`, `explorer::probe_sql` (the reference arrow's
  `SELECT 1 … LIMIT 1` per referencing relation), `explorer::count_sql` (the status bar's Count, a
  `COUNT(*)` -- `COUNT_BIG(*)` on SQL Server, whose `COUNT` is an `int` --
  under the tab's filter, run only when the user asks and through
  `sql::is_generated_select`, `sql::classify` and `sql::gate`, refused rather
  than prompted for when the gate would stop it), `sql::with_order_by`, `sql::update_batch`,
  `sql::insert_row`, `sql::insert_rows` (Import Rows' chunks), `sql::delete_row`, `filter::sort_expression` and
  `filter::filter_predicate`. `filter::sort_expression` is the one that gets
  forgotten, and forgetting it is silent: a double-quoted name is a _string
  literal_ in MySQL, so `ORDER BY "name"` sorts every row by the same constant
  with no error. `filter::filter_predicate` quotes a _value the user supplied_
  (a filter bar's value, typed or put there by following a foreign key), which
  is the other half of the same hazard. Since SQL Server spells a literal by
  its column's type, `filter_predicate` and `substring` take the column's
  `data_type` and quote through `Engine::quote_value`, the same call an edit
  makes, fed from the tab's loaded structure; a followed foreign key's bar
  borrows the referencing column's type for the referenced one, whose
  structure is not loaded, and a bar applied before the structure loads quotes
  as an unknown type (`N'…'`) until it is applied again. A saved tab records
  the engine its filter was written for (`StoredObject::filter_engine`) and
  reopens on that engine with the filter exactly as it ran
  (`filter::restored_filter`), since re-deriving it without the structure
  would spell it differently and miss the grid snapshot and tab identity keyed
  by it. Every filter operator lives inside it,
  `filter::substring` and `filter::like_pattern` included;
  `filter::foreign_key_filter` yields a bar rather than a `WHERE`, and
  `filter::derived_filter` folds the bars through `filter_predicate` rather than
  quoting anything itself. `sql::paged` writes too, but only two integers, into
  a preview's limit (see the SQL Server paging bullet). The explorer
  menu's clipboard text (`explorer::select_top_sql`, `explorer::drop_sql`,
  `explorer::truncate_sql`, and `Connection::ddl` where an engine assembles
  its DDL rather than asking the server for it) quotes through `Engine` too,
  though none of it is ever run.
- **An engine is one `EngineSpec` in `src/db/spec.rs`, and `Engine::spec()` is
  the only per-variant match in `impl Engine`.** A SQL engine's `SqlSpec` is `SqlSpec::ANSI`
  plus overrides, so a field it leaves alone is the shared behaviour and the
  writers in `sql.rs`, `filter.rs` and `explorer.rs` read the spec, not the
  engine. Its `Abilities` start from `Abilities::NONE`, the read-only tier, and
  switch on only what the engine does. The one hook, `Hooks::quote_value`,
  covers a literal that depends on the column's type (SQL Server) and returns
  `None` to fall back to `quote_literal`. A spec field is a bool, a strategy
  enum or a list of names DBDelve's own code reads, never SQL text. Per-engine
  snapshots in `src/sql_snapshots` hold the generated text, and
  `fake_spec.rs` shows a spec nobody wrote an arm for generating SQL.
- **A relation tab's row number is the catalog's, never a count run unasked.**
  An unfiltered tab shows the row estimate that rides along with the explorer's
  sizes (Postgres `reltuples`, MySQL `TABLE_ROWS`, SQL Server
  `sys.partitions.rows`), marked `≈`; Snowflake's `ROW_COUNT` is exact
  (`Engine::exact_row_estimates`) and SQLite has none. A never-analyzed
  table's mark, and any approximate zero, show nothing rather than a wrong
  number, and so does a filter, which no estimate covers; the bar then falls
  back to the rows on screen. An exact number is the Count button's, a full
  scan the user asked for.
- **Three filter operators are written differently per engine, and two of those
  are decided by the grammar rather than by any server.**
  `sql::is_generated_select` refuses whatever `tree_sitter_sequel` cannot parse
  whole, and that pin does not move, so a predicate the grammar does not know
  is one DBDelve cannot run, however valid the server would find it. It has **no
  `ESCAPE` clause and no infix `REGEXP`**. So the pattern operators lean on the
  engine's default `LIKE` escape, which is the backslash on Postgres and MySQL,
  and `like_pattern` escapes `%`, `_` and the backslash itself with it.
  **SQLite, which has no default escape at all**, gets `instr`/`substr`
  substring arithmetic instead, and so does DuckDB, which has none either: case-sensitive where its own `LIKE` is not,
  which is the price of not silently widening a match on a value containing
  `%`. **SQL Server has no default escape either**, but a bracket makes any
  character literal there, so its pattern brackets `%`, `_` and `[` (`[%]`)
  and keeps `LIKE`. The regex match is Postgres `~`, MySQL `REGEXP_LIKE(col, pattern)`
  (8.0.4 and later), DuckDB `regexp_matches(col, pattern)`, MariaDB
  `REGEXP_INSTR(col, pattern) > 0` (it has no
  `REGEXP_LIKE`, and its infix `REGEXP` is not in the grammar the gate
  parses), and **omitted from the dropdown on SQLite and SQL Server**, which
  ship no regex (SQL Server not before 2025).
- **MySQL, SQLite and SQL Server bracket a generated multi-row batch** in
  `BEGIN`/`COMMIT` (`sql::update_batch`), because each commits every statement
  on its own where a Postgres `simple_query` submission is one implicit
  transaction. The brackets go in the statement text, never around it
  invisibly, and `sql::is_generated_write` refuses a transaction it cannot see
  closed. `Engine::transaction_start` answers which engine needs one, with an
  explicit arm per engine: never a `_ =>` catch-all, which is how MySQL once
  went unbracketed while `AGENTS.md` claimed it was atomic. `BEGIN` rather than
  MySQL's own `START TRANSACTION` because the gate has to read the brackets back
  and the grammar knows only the first; MySQL takes it as an alias outside a
  stored program. SQL Server's is `BEGIN TRANSACTION`, because a bare `BEGIN`
  opens a statement block in T-SQL; the grammar reads that spelling and
  `generated_statements` sees through its second word.
- **A batch that fails part way is rolled back, and the error says which state
  the data is in.** Without that the brackets produce a third state, neither
  applied nor discarded, and rendered as applied, because the refresh `SELECT`
  runs on the same long-lived connection and reads the uncommitted rows back.
  Only a transaction _this_ submission opened is rolled back; one the user began
  in an earlier run is theirs to finish. SQLite asks `is_autocommit` before and
  after; MySQL cannot, because the driver keeps the server's
  `SERVER_STATUS_IN_TRANS` flag private, so it reads the submitted text instead.
  **SQL Server's session runs with `SET XACT_ABORT ON`**, set at login like a
  timeout, because without it a constraint violation ends only its own
  statement and the batch carries on to the `COMMIT` dbdelve wrote. dbdelve's
  own SQL is reached through `Connection::generated` (`Origin::Generated`: a
  relation tab's preview, a grid edit) or the private `internal_query`
  (`Origin::Internal`: catalog and structure queries), never `Connection::query`
  (`Origin::User`). `execute_unchecked` is the one place `connection.query` is
  called, and calls `connection.generated` for an object tab's query and a
  query tab's grid-edit apply run; the only other callers of
  `connection.generated` are the status bar's Count (`Workspace::count_rows`),
  the reference arrow's one-row checks (`Workspace::show_references`), and
  `import_rows::run::send`, driven by `Workspace::import_step`, on the
  import's own connection from `Connection::apart`. A
  statement run under `Origin::Generated` or `Origin::Internal` is wrapped in
  `EXEC sp_executesql` (`scoped`) behind `SESSION_OPTIONS` (`XACT_ABORT ON`,
  `QUOTED_IDENTIFIER ON`, `ANSI_NULLS ON`, `ANSI_WARNINGS ON`,
  `IMPLICIT_TRANSACTIONS OFF`, `ROWCOUNT 0`, `DATEFORMAT ymd`), scoped to that
  one `sp_executesql` call so a `SET` the user made themselves is exactly as
  they left it once dbdelve's statement returns. An import's lone `BEGIN
  TRANSACTION`, `COMMIT` and `ROLLBACK` (`sql::is_transaction_bracket`) go out
  bare instead: one opened or closed inside `sp_executesql` changes
  `@@TRANCOUNT` across a module boundary, which the server reports as error
  266. `Origin::User` runs as typed, under nothing but the login default. A
  preflight before the statement (`PREFLIGHT_SQL`, run under `SESSION_OPTIONS`
  for dbdelve's own statement and under the user's options for theirs, bar
  `USER_DESCRIBE_OPTIONS`' `IMPLICIT_TRANSACTIONS OFF` and `ROWCOUNT 0`, so it
  compiles the statement the way it will run) reads `@@TRANCOUNT`,
  and `transaction_outcome` reads it again after a failure: T-SQL has no
  nested transactions, so dbdelve issues its own `ROLLBACK` only when its own
  `BEGIN TRANSACTION` batch opened the transaction from a count of zero, never
  one open before it arrived; when `XACT_ABORT` ends a transaction that
  predates the statement regardless, the error says that transaction -- not
  dbdelve's -- was rolled back.
- **`sql_variant` and every CLR type (`geography`, `geometry`, `hierarchyid`,
  system type id 240) panic tiberius part way through a result it cannot
  decode**, which would otherwise close the session and the transaction in it.
  The same preflight describes the first result set's columns
  (`unreadable_columns`) and refuses before running when one of them is
  unreadable: `readable_preview` rewrites the one shape `explorer::preview_sql`
  writes to read such a column as text (`CONVERT(nvarchar(max), …)` for a CLR
  type, `CAST(… AS nvarchar(4000))` for `sql_variant`, the spellings the
  gate's grammar reads) under its own name, which is why the column has no
  source and stays uneditable, and the rewrite passes `sql::is_generated_select`
  itself before it is sent (`AGENTS.md` hard rule 2); a user's own statement is refused
  instead of rewritten, naming the column and how to cast it, per `AGENTS.md` hard rule 1.
  A later result set the preflight cannot see still panics the driver;
  `Session::trip` catches it (`Lost::Panicked`) and the run reconnects, same as
  a lost socket, its error carrying the panic message plus that the session
  reset took any open transaction and temp tables with it.
- **A statement timeout is one number per profile, applied at connect**, and
  each engine buys something different with it. Postgres's `statement_timeout`
  bounds any statement; MySQL's `max_execution_time` bounds read-only `SELECT`s
  only, so a runaway `UPDATE` or `ALTER` there is Cancel's problem alone, and a
  server older than 5.7.8 fails the connect rather than the statement;
  MariaDB's `max_statement_time` bounds any statement, as Postgres's does (an
  interrupted `DO` there is only a warning, so its live test bounds a `SET`),
  and a server older than 10.1 lacks it and fails the connect the same way;
  SQLite has no such setting and gets a wall-clock timer firing
  `sqlite3_interrupt`, which counts waiting on a lock the same as scanning; SQL
  Server has none either and gets a timer around the run, which stops the
  statement the way Cancel does; ClickHouse's is `max_execution_time` on each
  request, with a client bound behind it (see its entry below). It goes in at connect and never into the
  user's submission (`AGENTS.md` hard rule 1, and on Postgres a `SET` inside their submission
  would be scoped to the implicit transaction around it). It therefore bounds
  DBDelve's own catalog and structure queries too, which is intended. On SQL
  Server the timer wraps the whole of `Session::trip`, so it bounds every round
  trip a run needs and not just the statement: the preflight ahead of it
  (`@@TRANCOUNT` and the result-set describe) and the follow-ups behind it
  (`@@ROWCOUNT`, the edit-target describe) run inside the same window and are
  stopped the same way. The timer stops applying once a result set's kept rows
  reach the limit: what follows is rows read only to be counted, and closing the
  connection for them would take the user's transaction with it. Cancel still
  reaches it.
- **Cancel reaches the running statement and nothing queued behind it.** The
  handle it needs (Postgres's `CancelToken`, MySQL's connection id, SQLite's
  `InterruptHandle`, a second handle on SQL Server's socket) is captured in
  each engine's `open`, before the client goes behind the connection mutex,
  because the statement being cancelled is holding that mutex.
  `Connection::cancel` takes `&self` and locks nothing but its own flags. A
  catalog load is never what Cancel is for (`Origin::Internal` is never
  cancellable on SQL Server, and the other engines' `cancel` stops only the
  handle their one connection is running, which a catalog load does not
  register): `live_a_cancel_while_queued_behind_a_catalog_load_stops_the_statement_not_the_load`
  is the proof for SQL Server. A statement still waiting for the connection
  mutex behind one that is running, or for SQL Server's reconnect, is cancelled
  before it is ever sent (`InFlight::cancel_queued`, checked in
  `Connection::run` once the mutex is taken and the session reconnected), and a Cancel that lands after the statement already finished is
  answered "Cancel arrived after that." rather than left to look like it did
  nothing. On SQL Server that Cancel still closes the connection, so the
  complete result lands with a notice about the reset instead of an error.
  Rows that streamed in are kept as a cancelled result only for a stoppable
  read where `db::is_cancel` matched the error; otherwise the error is shown.
  Either way `db::cancel_cost` says what the cancel cost: SQL Server's session
  reset, Postgres's aborted transaction, a SQLite write's rolled-back
  transaction.
- **Postgres's side session** (describe, and the check before a stop at the
  limit) opens with a 2-second `statement_timeout`, and neither ever blocks
  the run on it: a describe waits its turn there on a thread of its own, and a
  stop that cannot get it quickly drains instead. A describe waits on the same
  locks as the user's statement, which their own open transaction can hold for
  good. The run's first rows are held for the describe for up to a second
  (`Feed::await_types`), so they are first shown by type rather than restyled
  once it answers.
- **SQL Server's Cancel closes the connection** (a read stopped at the row
  limit does not: that is `SET ROWCOUNT`). tiberius cannot send TDS's
  attention signal, and `KILL` needs `ALTER ANY CONNECTION`, which an ordinary
  login lacks, so Cancel shuts the socket down and the server abandons the
  batch, rolling back what it had open (`live_a_cancel_stops_the_statement_on_the_server_and_reconnects`
  proves the statement behind a `WAITFOR` never runs). The session does not
  survive, so the run reconnects and its error says what was lost.
- **Postgres's notices are read off the connection with `poll_message`**, not
  by awaiting it as a future: the future impl logs each notice and drops it.
  The driver buffers them as psql prints them (severity, then `DETAIL` and
  `HINT` lines). The buffer is cleared before the user's statement and taken
  right after it, before the describe or edit-target queries, so those cannot
  leak theirs in; they land in `QueryResult::messages` for typed runs only.
- **MySQL and MariaDB warnings are `SHOW WARNINGS`, run only when the
  connection's warning count is non-zero**, so a clean statement costs no
  extra round trip. It runs after the row counts and edit target are read, and
  it lists only the last statement's. A multi-statement submission therefore
  gets one lead message counting the earlier statements' warnings, which the
  server no longer lists. A non-final `SELECT`'s count is not exposed by the
  driver, so its warnings are not counted. `SHOW WARNINGS` leaves
  `ROW_COUNT()` at -1 for whatever the user runs next; that is accepted, as
  the mysql client's `\W` accepts it, because the warnings' text is the point.
- **Explain** is `EXPLAIN` / `EXPLAIN ANALYZE` on Postgres and MySQL,
  `EXPLAIN` / `ANALYZE` on MariaDB, and `EXPLAIN QUERY PLAN` on SQLite, which
  has no analyze form, so that mode is not offered there
  (`Engine::explain_prefix` returns `None`). MariaDB's `EXPLAIN` and `ANALYZE`
  are tabular, so they render as the flat list MySQL's plain `EXPLAIN` already
  does. SQL Server offers neither: its plans come from `SET SHOWPLAN_XML`, a
  session switch that must be a batch of its own, which is not a prefix on a
  copy of the statement. Server versions are not detected: an older server
  refuses the statement and says so. On ClickHouse DBDelve offers plain
  `EXPLAIN` only, not the server's `EXPLAIN ANALYZE` (26.7+): its `EXPLAIN`
  plans the statement and never runs it, and its rows stay in the grid.
  `sqlparser`
  cannot read MariaDB's `ANALYZE <statement>`, so on MariaDB `sql::classify`
  reads it as `EXPLAIN ANALYZE <statement>`, classified by the statement it
  runs as MySQL's is. Only when a `SELECT`, `WITH`, `INSERT`, `REPLACE`,
  `UPDATE`, `DELETE` or `(` follows (after an optional `FORMAT=JSON`):
  `ANALYZE TABLE t` would otherwise read as an `EXPLAIN` of the query `TABLE t`.
  A lone statement the user typed as an explain lands in the plan pane as the
  command's does, but only where its answer is a shape `explain.rs` reads
  (`sql::typed_explain`): Postgres text, MySQL's tree, SQLite's `QUERY PLAN`,
  MongoDB's `.explain()`. Tables, JSON and bytecode stay in the grid. Its
  rows still land in the grid, for copying and export, with the plan drawn
  over them and no Data toggle. It never becomes `last_query`, so no sort or
  refresh runs an `EXPLAIN ANALYZE` again.
- **`SET column = DEFAULT` is withheld on SQLite** (`Engine::assigns_default`),
  where `DEFAULT` is not an expression.
- **Read-only has no server-side backstop on SQLite, DuckDB, Snowflake or SQL
  Server.** Postgres gets `default_transaction_read_only`, MySQL `SET SESSION
  TRANSACTION READ ONLY`, ClickHouse `readonly=2` on each request. So on the
  other four, Read-only refuses a statement `sql::classify` cannot
  read rather than offering to run it once (`Engine::holds_read_only`).
- **Snowflake has no session, because it is spoken to over its SQL REST API.**
  It publishes no Rust driver. Each submission is one HTTPS request, so nothing
  set in one run reaches the next, an open transaction included. The API does
  not even pretend: a `USE` comes back as "Command not supported by SQL API:
  USE", which `live_a_use_is_refused_rather_than_quietly_forgotten` pins. The database, warehouse, role, timeout and `MULTI_STATEMENT_COUNT`
  are fields of the request and never SQL (`AGENTS.md` hard rule 1).
  So Import Rows offers no All or nothing there (`Engine::all_or_nothing`):
  no transaction can span its chunks.
- **Snowflake has no connection mutex**, alone among the six: there is no
  socket to serialise, so a catalog load does not queue behind a slow query,
  and `snowflake::Connection::at_once` runs a structure load's four statements
  on four threads rather than one after another (1.3s against 3.6s, measured).
  The consequence is that more than one statement can be in flight -- every
  tab's, and the catalog's -- so a run goes out under a `db::CancelToken` the
  tab keeps in `QueryState::Running`, and `cancel` stops only the handles
  registered under that token. A cancel that lands before the submit has
  returned a handle is kept on the token and carried out when the handle
  arrives. The other engines take the token and ignore it:
  they stop whatever their one connection is running, except under a
  token made by `CancelToken::alongside`, which stops the connections a queue
  run at once opened instead (see `docs/agents/session.md`). Between partitions the
  fetch checks whether Cancel was asked and stops fetching the rest.
  Statements are always submitted `async=true`, because a synchronous submit
  withholds its handle for up to 45 seconds and the handle is what Cancel needs.
- **Snowflake signs in with a key pair and nothing else.** An RS256 token per
  request, signed with `ring` from the key file the profile points at by
  absolute path; nothing goes to the Keychain, so `ConnectionConfig::server`
  answers `None` for it and nothing prompts on its behalf. An encrypted key is refused by name (`ring` does not
  decrypt PKCS#8). Password, OAuth, browser SSO and access tokens are not
  implemented. There is no `sslmode` to honour or weaken: the API is HTTPS and
  always verified against `webpki-roots`.
- **An engine's own statements are submitted synchronously; the user's are
  not.** The asynchronous submit exists so Cancel has a handle from the first
  moment, and it costs a round trip -- 885ms against 315ms for three
  `SELECT 1`s. Nothing offers to cancel a catalog load, so
  `snowflake::Connection::internal_query` pays for neither.
- **The relations and the routines are two loads, everywhere.**
  `Connection::catalog` answers with relations alone and
  `Connection::routines` follows behind it, merged in by `Catalog::merge` when
  it lands. It is engine-agnostic by design, and Snowflake is why it exists:
  `INFORMATION_SCHEMA.FUNCTIONS` and `PROCEDURES` took four and eight seconds
  to report that a database had neither, with the tables in hand after two. A
  routines load that fails leaves the relations on screen and says so once.
  `Catalog::merge` appends a routines-only schema rather than re-sorting, so a
  `schema_index` taken before the merge still names the same schema;
  `Catalog::by_name` is the display order. Until `Routines::Loaded`, a stored
  routine tab stays pending rather than being resolved against a catalog that
  cannot have it yet.
- **Snowflake's catalog needs a running warehouse.** It is read through
  `INFORMATION_SCHEMA`, so connecting resumes a suspended warehouse and so does
  opening a Structure tab. That view has nothing naming the columns of a key,
  so keys come from `SHOW PRIMARY KEYS`, `SHOW UNIQUE KEYS` and `SHOW IMPORTED
  KEYS`, asked `IN SCHEMA` and narrowed to the relation because `IN TABLE` is
  an error for a view. The schema there is written from the database down: a
  `SHOW` does not resolve against the request's `database` field the way a
  query does, and refuses a bare schema with "Must specify the full search
  path". `SnowflakeConfig::stored_database` folds a bare name to upper case
  for it, as the server does. A profile is bound to one database, as on Postgres, and
  a foreign key into another database is listed and not followable.
- **A Snowflake result is never editable.** Its primary keys are declared and
  not enforced, so a `WHERE` over one may name several rows, and the API says
  nothing about which table a result column came from. `QueryResult::edit` is
  always `None`. Inserting a row from an object tab needs no key and works as
  it does elsewhere.
- **Snowflake's temporal values arrive as counts from the epoch** whatever
  output format is asked for, and `snowflake::render` turns them into text
  before they leave `src/db/`. `TIMESTAMP_LTZ` is shown in UTC with a `Z`,
  because the API carries no session time zone to show it in.
- **Snowflake filters use `CONTAINS`, `STARTSWITH` and `ENDSWITH`**, for
  SQLite's reason: no default `LIKE` escape. Its regex is `REGEXP_COUNT(col,
  pattern) > 0` and not `REGEXP_LIKE`, which there anchors the pattern to the
  whole value where Postgres `~` and MySQL's `REGEXP_LIKE` match anywhere.
  Explain is not offered: its plan is a fourth shape `explain.rs` does not read.
- **`CHECK` constraints are absent** from the Structure tab on MySQL and SQLite.
  SQLite keeps them only in the `CREATE TABLE` text; MySQL's
  `information_schema.CHECK_CONSTRAINTS` only exists from 8.0.16.
- **An index's kind, key and partial condition come from the catalog, never
  from parsing its definition** (`db::Index`). A key crosses `assemble_structure`
  joined by `chr(31)`, since `, ` splits `coalesce(a, b)`. Where a catalog falls
  short: SQLite names no expression key and keeps a partial condition only in
  the `CREATE INDEX` text, so both are read from it; MySQL's `EXPRESSION`
  (8.0.13+) is read inside a `/*!80013 */` comment, which MariaDB, lacking the
  column, skips; DuckDB keeps no key order; ClickHouse's keys are split at
  top-level commas and are all plain; MongoDB's key reads `email`, `created -1`,
  `title text`, and its `partialFilterExpression` is the condition.
- **MySQL verifies certificates against `webpki-roots`**, not the platform
  trust store the Postgres path reads. It fails loudly, which `AGENTS.md` hard rule 6 permits.
- **SQL Server identifiers are double-quoted, not bracketed.** The grammar
  every gate parses with has no `[name]` and its pin does not move; the login
  tiberius sends turns `QUOTED_IDENTIFIER` on, so `"name"` is an identifier.
  A value's literal is spelled by its column's type (`Engine::quote_value`,
  fed the types the grid, insert form or filter bar knows): `N'…'` for a Unicode or
  unknown type, or any non-ASCII value, because a bare one is converted to the
  database's code page and a character it lacks is stored as `?`; plain `'…'`
  for an ASCII value bound for a `varchar`, numeric or date column, because an
  `N` literal there converts the column instead and the edit scans the index,
  with a `datetime` or `smalldatetime` value in the ISO `T` form
  (`'2024-01-02T03:04:05.000'`) that no `DATEFORMAT` or `SET LANGUAGE`
  reorders, since the literal also runs from a query tab's buffer under the
  user's options; and a bare `0x…` (a `0X` prefix normalised) for a binary
  column, only when the value is exactly a hex literal, since a quoted one is
  compared as text and matches no row. `sql::equality_columns` accepts `N'…'`
  as a single-quoted literal and `0x…` beside it. `image`, `rowversion` and
  `timestamp` are binary on SQL Server alone (`Engine::is_binary_type`). A
  binary alias type is known only by its alias name, so it is quoted as an
  unknown type (known limitation).
- **SQL Server previews page with `OFFSET … FETCH`.** T-SQL has no `LIMIT` and
  the grammar has no `FETCH`, so a preview is generated, sorted and gated as
  `LIMIT n OFFSET m` and re-spelled by `sql::paged` afterwards. `OFFSET`
  requires an `ORDER BY`, so an unsorted page gets `ORDER BY (SELECT NULL),
  <key>`: the key (`Structure::row_key`, primary else unique) is what keeps a
  parallel plan from repeating or skipping rows between pages, which is why
  the first page waits for the structure (`Engine::pages_by_key`), and the
  `(SELECT NULL)` is how `sql::unpaged` tells it from a user's sort. It
  replaces only the `limit` node the parse tree locates, with the two integers
  read out of it; what runs, and what the tab shows, is the T-SQL. A user's own
  `TOP` or `[bracketed]` statement cannot be sorted from a header, for the
  grammar's reason.
- **A SQL Server buffer is split a batch at a time** (`Buffer::for_engine`):
  `GO` lines are separators never sent, `[names]` are quoted, a routine's body
  runs to the end of its batch and a `BEGIN … END` block is never cut. A
  selection covering one batch is sent as that batch; one covering several runs
  them in turn, a submission apiece (`sql::queued_batches`), because the batch
  is the scope boundary a `DECLARE` or a temp table ends with, and splitting
  below it would send a declaration and its reader as two batches. The
  exception is a selection whose batches are nothing but plain reads naming no
  `@variable` or `#temp` table (`sql::split_reads`): when it would run at once,
  `run_query` cuts it into its statements and each goes out on a lane of its
  own, every chip up front, as on the other engines. `GO n` is
  still refused (`sql::batch_counts`): the count repeats its batch, and running
  it once is not what the buffer says.
  Format Query reflows each batch alone with sqlformat's SQL Server dialect,
  which reads `[names]`, and leaves every `GO` line as written.
- **A SQL Server batch keeps every result set it returns, not the last one.**
  Because the unit sent there is the batch, `SELECT 1; SELECT 2;` is one
  submission where on every other engine it is two, and a submission that kept
  only its last set would show one result where the others show two.
  `mssql::collect` opens a set per description (`open_set`) rather than
  starting the kept one over, and `collected_sets` hands them back as the first
  carrying the others in `QueryResult::rest` — one level deep, a submission's
  sets and never a tree, and empty on the other five engines, which have no
  second set to carry. `columns`, `rows`, `bytes`, `rows_affected` and `edit`
  are each set's own; `elapsed` is the submission's, and every set carries it.
  `Collected::sets` is still the count, and still decides both the
  `SELECT @@ROWCOUNT` follow-up and the editability describe, so **a multi-set
  batch stays uneditable**. `Workspace::advance_queue` lands each extra set in
  the tab's queue as a `Finished` of its own, labelled `Result 2`, `Result 3`
  and so on, since a set after the first has no statement of its own to be
  named after. The grids those sets need are the ones the run reserved in
  `Queue::spare`, because the completion handler has no `Window` to build one
  with: `sql::expected_sets` reserves one per statement in the batch, which is
  an upper bound for an ordinary batch and not for a procedure, a loop or a
  trigger, and a set past the last spare is dropped with a notice saying so
  rather than silently. A batch that returns one set after all leaves a queue
  of one result, which `advance_queue` drops, so an ordinary run is exactly
  what it always was.
- **SQL Server's messages (`PRINT`, `RAISERROR` at severity 10 or less, a
  `USE`'s "Changed database context") are heard, not read.** tiberius 0.12.3
  drops INFO tokens from its stream and only logs a `tracing` event, so
  `Session::fetching` runs the submission's own drain under a scoped
  subscriber (`Heard`) and puts what it heard in `QueryResult::messages`. The
  callsite is picked by `INFO_TOKEN_LINE`, because the environment-change
  events share its target and shape; the exact `=0.12.3` pin is what keeps
  that line right, so a tiberius bump must re-check it (the live message tests
  catch a wrong one). Internal trips never listen, so dbdelve's own `USE` and
  describes say nothing. Messages from a submission that fails are lost with
  the error.
- **SQL Server edit targets come from a describe**, as Postgres's do:
  `sys.dm_exec_describe_first_result_set` in mode 2 (a view is its own source,
  and has no key), after the statement ran, only when it returned exactly one
  result set, and only when the described names match the result's. A column
  from another database is not local, since an `EditTarget` names no
  database. `is_updateable` is false for a computed, identity or rowversion
  column, so it shows in a result but is never a `SET` target; it still names
  its row's key when it is one, since the describe's key columns and its
  `SET`-target columns are read separately. Writes report the last statement's
  row count: tiberius keeps the done tokens to itself, so it is asked for with
  a follow-up `SELECT @@ROWCOUNT`, the way Postgres's own last-command count
  works, whenever the write's own result carried none. That follow-up, and the
  preflight before the next statement, leave `@@ROWCOUNT` and `@@ERROR` at
  their own values, not the user's last statement's (known limitation: an
  `sp_executesql` wrapper does not shield them).
- **The Structure tab reads more than one privilege level can see.** An alias
  type's `TYPE_NAME(user_type_id)` is null when the login cannot see it, and
  falls back to `TYPE_NAME(system_type_id)`, its base type. A computed column's
  definition is null the same way without `VIEW DEFINITION`, and reads as `AS
  <hidden>` rather than a bare `AS `. An index's text carries its `INCLUDE`
  columns and its filter predicate when it has either, and a columnstore index
  (clustered or not, no key of its own) is read through an `OUTER APPLY`
  rather than dropped for having none. A foreign key's text carries its `ON
  DELETE`/`ON UPDATE` actions when they are not `NO ACTION`, and ` DISABLED`
  or ` NOT TRUSTED` when either is set.
- **A SQL Server profile is held to its database.** Every name dbdelve writes
  stops at the schema, so after a `USE` a generated `DELETE` would find the same
  name elsewhere. A run whose text says `USE` is followed by `DB_NAME()`; if it
  moved, the session is moved back and the run fails saying so, whether or not
  the statement that moved it also failed on its own account -- a batch that
  moved the session and then failed leaves it moved all the same.
- **SQL Server's TLS is tiberius's.** `disable` is `EncryptionLevel::Off`,
  which still encrypts the login packet; the promising rungs are `Required`
  (never `On`, which panics the driver when the server offers less); `prefer`
  and `disable` alone retry with `NotSupported` for a server that cannot
  encrypt, and only after a failed handshake (`negotiation_failed`): a refused
  login or a connect that timed out fails the same way again, so neither
  retries. `verify-ca` checks the hostname too, since tiberius cannot switch
  that alone off: stricter, which `AGENTS.md` hard rule 6 permits. A named root certificate must
  be one certificate.
- **An error from inside a procedure, trigger or function carries no position.**
  `token.procedure()` names it when one raised the error; the line the server
  gives is the routine's own, not the batch's, so `query_error` maps a line
  back into the submitted text only when that name is empty.
- **SQL Server values are rendered in `mssql::render`.** TDS is binary, so the
  server's own formats are rebuilt there: dates from day counts, `datetime`'s
  1/300 s ticks, `datetimeoffset` from its UTC instant plus offset, decimals from
  the unscaled integer. `money` arrives from tiberius as an `f64`, exact below
  about 9 × 10¹¹. A `float` or `real` outside roughly `1e-4` to `1e15` prints in
  the exponent form the server itself would, `1E+308`, rather than `Display`'s
  hundreds of zeros. `Money` and `Datetimen` are what tiberius names a nullable
  `money`/`smallmoney` or `datetime`/`smalldatetime` column alike, without the
  length that would tell them apart: a nullable `smallmoney` always renders as
  `money`, and a nullable `smalldatetime` only corrects itself to
  `smalldatetime` once a row hands back an actual `SmallDateTime` value, so an
  empty or all-`NULL` one still reads `datetime` (known limitation). Driver
  panics (unimplemented tokens, an empty trust store) are caught and reported
  rather than poisoning the mutex.
- **Turso is a local file only, through `turso_sdk_kit::rsapi`, not the `turso`
  crate.** `turso` forces async IO and keeps `interrupt` and the statement
  timeout private, and its default `mimalloc` installs a global allocator.
  With `async_io: false` the rsapi blocks like rusqlite, so no runtime is
  involved; Cancel is `interrupt`, the timeout is `set_query_timeout`, both
  reporting `interrupted`. `fts` is required to open any database holding a
  `USING fts` index. Turso Cloud and remote libSQL are not supported. The
  catalog, structure, DDL and references run through `sqlite::Schema`, the same
  pragmas as SQLite minus the schema argument, which Turso's pragma functions
  refuse; its `__turso_internal_` tables are hidden. `multiprocess_wal` is
  switched on only when a `-tshm` sidecar already exists, since the library
  refuses a mixed open against a live process. Where the IO backend has no
  shared WAL (Windows), the open falls back to a plain one, which the library
  still refuses while another process holds the file. **A Turso result is never
  editable**: the driver names no column's table, and reading it off the
  statement would be a guessed predicate (`AGENTS.md` hard rule 2); inserting from an
  object tab works. No Explain, and no Read-only backstop
  (`Engine::holds_read_only`). `Connection::connect` is a closure because
  holding `Arc<TursoDatabase>` directly overflows the compiler's `Send`
  evaluation of `db::Connection`. Its live test reads `dbdelve_TURSO_PATH`.
- **DuckDB is embedded through duckdb-rs, `bundled`: a file, or a database in
  memory.** A blank path is a database in memory, gone when the profile
  disconnects; a path that does not exist is refused rather than created, as
  on SQLite. `parquet` and `json` are linked in, so reading those files
  downloads nothing. The catalog, structure, keys and DDL come from
  `duckdb_tables()`, `duckdb_views()`, `duckdb_columns()`,
  `duckdb_constraints()` and `duckdb_indexes()`. Results arrive as Arrow
  batches and every cell is rendered by Arrow's formatter; a `TIMESTAMPTZ`,
  which DuckDB hands over under the zone's name, is relabelled `+00:00` first,
  since Arrow formats a named zone only with `chrono-tz`. Every statement of a
  submission but the last runs as it is prepared, and the last one streams,
  so a read past the row limit is stopped by fetching no further. DuckDB
  answers a command with a one-column result of its own, `Count` or
  `Success`, and `sql::answers_with_rows` is what tells that apart from a
  query's column of the same name. duckdb-rs answers `is_autocommit` with
  `true` whatever the state, so a failed submission's transaction is rolled
  back by reading its text, as on MySQL. A cancel, even a read's, or a failed
  write aborts the transaction it lands in: it refuses everything until a
  `ROLLBACK`, and a `COMMIT` rolls it back. A second connection (`alongside`,
  and an import's `apart`) is cloned from an idle one the profile holds,
  since opening an in-memory database again makes a new, empty one. A
  connection cloned that way starts on the database the file opened as, not
  where a `USE` moved the one it was cloned from: `USE` belongs to the
  connection and `ATTACH` to the instance. So the profile's picked database
  and schema are kept on the driver's connection and shared by its clones,
  and every connection it hands out (`alongside`, so `query_alongside` and
  `apart` too) runs the `USE` before anything else. Nothing picked runs
  nothing, and the session's `search_path` stays DuckDB's own. **A
  DuckDB result is editable only as a plain select of one keyed table**: the
  driver names no column's table, so `sql::table_read` reads the statement
  for one base table (no join, subquery, table function, CTE, set operation,
  `DISTINCT`, grouping, window clause or renaming alias) whose columns are
  `*` or bare names, and `duckdb::edit_target` takes the table and its
  primary key from the catalog. The connection that ran the statement says
  where its names resolve: `current_database()`, `current_schema()` and the
  `search_path`, which is either DuckDB's own (blank) or exactly the one
  entry a `USE` writes for those two, so a picked schema keeps a bare
  `SELECT * FROM account_scores` editable. A bare name is looked for only in
  the current schema, though DuckDB would go on to `main` when it is not
  there. Anything in doubt refuses: a column count the parse and the driver
  disagree on, a column read twice, any other search path (DuckDB tries each
  entry in turn), a temporary table of the same name, a schema whose name is
  also an attached database's (DuckDB calls `"analytics"."t"` ambiguous
  then), a table in another database. DuckDB enforces its keys, unlike
  Snowflake. No Explain, and no Read-only backstop.
  Its tests need nothing external and run under plain `cargo test`.
  `read_xlsx` is a table function of DBDelve's own over calamine (`db::xlsx`),
  registered at open, since DuckDB's `excel` extension cannot be linked in: a
  column whose filled cells are all numbers is a `DOUBLE`, anything else text
  as the sheet shows it. A data file dropped on the window or picked from the
  palette (`workspace::files`) opens in a DuckDB profile held in memory, made
  if there is none, as a query tab whose `SELECT * FROM read_csv(…)` DBDelve
  wrote on screen and runs once the profile connects.
- **Geometry is Postgres-only.** MySQL has a `GEOMETRY` type; rendering it is a
  separate decision nobody has asked for.
- **MariaDB's `JSON` is a `longtext` with a `json_valid` check**, and is shown
  as the server reports it: `longtext` on the Structure tab, `text` in the
  grid, never special-cased into a JSON type. Its catalog also
  reports what MySQL 8 no longer does, `bigint(20)`'s display width and a
  nullable column's default as the text `NULL`, and both are shown as they come.
- **MongoDB is a statement language of its own, and the classifier is the only
  Read-only boundary.** The server has no read-only hold to ask for
  (`holds_read_only` is false), so `mql::classify` is all that stands between a
  Read-only profile and a write. It fails closed: anything it cannot parse whole
  is `Unreadable`, a `$out` or `$merge` at any depth of any argument counts, and
  a `runCommand` is a read only when its name is on a whitelist.
- **`$out` is a `Drop`.** It replaces the target collection whole, as
  `CREATE OR REPLACE TABLE … AS SELECT` does, which `sql::classify` also calls a
  `Drop`, so it asks for confirmation the same way. `$merge` is a write. An
  unfiltered `deleteMany` is an `UnfilteredDelete`; `updateMany({})` is a plain
  write, as an unqualified `UPDATE` is.
- **There are no transactions to bracket a batch with** (`transaction_start` is
  `None`). A multi-row edit applies in order, and when one fails the error says
  which row did and that the rows before it applied. Nothing is rolled back.
  Import Rows offers no All or nothing either (`Engine::all_or_nothing`), so
  each chunk's `insertMany` is kept as it lands.
- **A missing field is not a null.** A document may lack a field another has, and
  the grid says so: a missing cell renders empty with no `NULL` label, a null
  renders `NULL`, and a snapshot keeps the difference.
- **Types are per cell, not per column.** A field's type varies from document to
  document, so `QueryResult` carries a type tag for each cell (`$type` aliases,
  plus `missing`) beside the SQL engines' empty one. `Column::data_type` is the
  shared type when the present, non-null cells agree and `mixed` otherwise. An
  edit is coerced back to its cell's tag; text that does not coerce is an error
  before the review, never a quiet string.
- **The row limit caps a `find` or `aggregate` cursor, and a read is stopped
  there by dropping it**, which the driver kills on the server and which
  undoes nothing; an aggregate with `$out` or `$merge` runs to its end. The
  first batch is sized to limit+1 (capped at 10,000) unless the statement set
  its own `batchSize`. A
  command's reply is one document of at most 16MB and is not capped. Rows
  are not streamed into the grid: a later document can bring a field no
  earlier one had, so the columns are not known until the last one is in.
- **Cancel goes by session, not by comment.** Every run is sent in a driver
  session of its own with the statement untouched (a user's `comment` is theirs
  and is never rewritten to carry a tag). `cancel` takes `&self`, finds the run's
  operations through `$currentOp` on that session's `lsid` and `killOp`s them.
  The connection survives.
- **`login_database` is the database a profile logs in to, kept apart from the
  one it is on.** Select Database moves the profile to another database, and a
  user is defined in one database: the driver authenticates against the
  connection string's database unless `options` names an `authSource`. So the
  first switch records the original in `login_database` and the string keeps
  naming it, rather than rewriting the options. The form does not show it and
  saves it as it was.

- **Redis is a command language of its own, classified by table.** As with
  MongoDB, `holds_read_only` is false and `redis_cmd::classify` is the only
  Read-only boundary; it fails closed on any name it does not list. There is
  no `EXPLAIN`, no transaction to bracket a batch with (`transaction_start` is
  `None`: a batch applies in order and a failure says which command failed and
  how many ran), no Import Rows (`Engine::takes_imports` is false: a new key
  names its own value, not a row under a file's columns) and no server-side
  sort or count under a filter (`SCAN` has neither).
- **The keys grid is one `keys` relation and a key opens as a query tab.**
  Its contents have no relation to open, so a new row and a deleted row on
  that tab are offered through `Engine::edits_query_rows`, shown in the review
  dialog and never written into the buffer. Each cell carries a tag for how
  its text stands for the bytes behind it, and only whole text is written
  back: a value cut for display, a non-UTF-8 key or value, JSON documents
  and streams are read-only. A collection's first 100 entries are read with a
  bounded `HSCAN`/`LRANGE`/`SSCAN`/`ZRANGE` beside its size; it shows as JSON
  only when that is all of it and at most 200 characters (`GLANCE`), tagged
  `hash json` and so on so an edit can diff it, else as its size. JSON with a
  byte that is not UTF-8 or an infinite score shows tagged `summary`, and is
  read-only. `Engine::new_row_omits` leaves the keys grid's `type` out of New
  row, since a new key is a string.
- **Edits never clobber what they did not read.** A new key is `SET … NX`, a
  field `HSETNX`, a rename `RENAMENX`, a string's new value `SET … XX … GET`
  (nil on a key gone since), and the connection turns the reply that
  says nothing happened into an error. A list item, named by its index,
  is checked first: `WATCH`, an `LPOS … COUNT 0 MAXLEN n` that must end at
  the index, then the write in a `MULTI`, where a nil `EXEC` is an error. A
  collection's JSON edit is diffed against the JSON shown (`PendingRow::held`)
  into the commands an opened key's grid writes, after a `WATCH`, an `ECHO`
  of that JSON and a bounded read (`HSCAN`/`SSCAN`, `LRANGE 0 100`, `ZSCAN`), which the connection compares
  with the `ECHO`: a different value, a cursor not `0`, or more entries is an
  error before the `MULTI`. A generated run that fails `UNWATCH`es, or closes
  the session.
- **Cancel kills the session.** The connection's `CLIENT ID` is killed from a
  side connection and the next command redials, so an open `MULTI` or `WATCH`
  goes with it.
- **The session remembers an open `MULTI` across runs** (`Session::multi`),
  and a generated run, read or write, is refused while it is set: inside it a
  write's reply is `QUEUED`, which would pass for applied. Nor does listing
  the databases or the catalog send `CONFIG GET` or `DBSIZE` then, which
  would be queued into the user's transaction. A Cancel that skips a command
  closes the session, since the one skipped may have been the `EXEC`.
- **It remembers a query tab's `WATCH` the same way** (`Session::watching`:
  set by a typed `WATCH`, cleared by `UNWATCH`, by an `EXEC` or `DISCARD`
  that closes a `MULTI`, and by a redial), and a generated run holding a
  `WATCH` of its own, a list item's write, is refused while it is set: its
  `EXEC` or `UNWATCH` would clear the user's along with its own.

- **ClickHouse is spoken to over its HTTP interface with `ureq`, and every
  result is asked for as `TabSeparatedWithNamesAndTypes`.** One `POST` per
  statement, the statement as the body byte for byte, `default_format` in the
  URL (`clickhouse.rs`, `FORMAT`). Native TCP is out of scope, and the
  `clickhouse` crate is HTTP too but hands back raw bytes for a result whose
  columns are not known at compile time, with TLS that would bypass `tls.rs`.
  Auth is the `X-ClickHouse-User` and `-Key` headers, never the URL, so an `@`
  in a user name needs no escaping. The port is blank to 8123, or 8443 under
  `require` and `verify-*`; `verify-ca` is verified as `verify-full`, which
  `ureq` forces and hard rule 6 permits.
- **Read-only is `readonly=2` on every request, and the classifier fills what
  that does not hold.** `ReadOnlySession::PerRequest`: `set_read_only` flips a
  flag the driver reads per request, and `readonly=2` cannot be lifted by a
  `SET` or a statement's own `SETTINGS`. It is not a backstop for everything:
  `BACKUP` is unchecked and `RESTORE` is refused only at `readonly=1`. So an
  unparseable statement is read by its first keyword (`UnreadableFloor::
  FirstKeyword`, `sql::by_statement`, `first_keyword_verdict`): `SELECT`, `WITH`,
  `SHOW`, `DESCRIBE`, `EXISTS`, `EXPLAIN`, `CHECK`, `WATCH`, `USE` and `SET`
  are Read-only and run once on confirmation; `INSERT`, `UPDATE` and `DELETE`
  are Read-write; DDL, access control, `SYSTEM`, `KILL`, `BACKUP` and
  `RESTORE` are Full; anything else fails closed to Read-write. A
  submission where the tokenizer and the server's lexer may part ways
  (`sql::misread`) needs Full, and an `OUTFILE` is Full. A user already
  `readonly` on the server is never sent the setting.
- **A table function in a `SELECT` is a plain read; `INSERT INTO FUNCTION`
  needs Full.** `url()`, `s3()`, `remote()` and `executable()` change nothing
  in the database, and `executable()` runs only what an admin placed on the
  server (owner decision). An `INSERT INTO FUNCTION` writes outside the
  database, and only ClickHouse's dialect parses it
  (`TableObject::TableFunction`, in `sql::spec_verdict`), as does a `CREATE
  TABLE` with an external or file `ENGINE`.
- **The user's statements share one session; DBDelve's own use none.** A
  session runs one query at a time, so `Connection::query` runs on the
  profile's session behind a mutex and `generated`, `internal_query` and every
  `KILL` go without one, never queued behind a long run. `query_alongside` is
  sessionless under the run's hold. A new session sends `USE <db>` as its own
  request under the run's Cancel (`Kind::Enter`), because the `database`
  parameter does not persist into a session. A session the server expired is
  `SESSION_NOT_FOUND`: the statement did not run, and the error says to run
  it again, since re-running silently could land in the wrong database.
- **Cancel is `KILL QUERY … SYNC` by a `query_id` DBDelve chose, retried until
  it lands.** The id is on the token before the `POST` reaches the server, so
  a `KILL` can arrive first and match nothing: `Connection::cancel` sends it
  again every 100 ms until a row answers or the request returns, and after 10 s
  aborts the local read and says the statement may still be running
  (`stop`, `settle`, `kill`). Dropping the connection stops nothing, so every
  stop and every abandonment is a `KILL`. A read stopped at the row limit is
  closed and killed the same way; one that cannot be stopped is drained
  through the exception-aware reader and counted. The error is normalised to
  `CANCELLED` so `is_cancel` reads it, only when the token asked: another
  user's `KILL` keeps the server's words.
- **A timeout is `max_execution_time` per request, and the guarantee is the
  client's.** The server's setting is best effort and does not bound a
  mutation's background work. So a request also gets a client bound of the
  timeout plus slack, as the whole wait for a response (`Bounds::user`) and
  as the longest silence between bytes (`Watched`, in `http.rs`), and hitting
  it `KILL`s the query. With no timeout the user's statements have no bound and
  Cancel is the remedy; DBDelve's own catalog requests get 120 s. What is sent
  is decided once at connect from `system.settings` and checked by a
  validation request, so a `readonly=1` user whose profile pins another
  `max_execution_time` fails the connect in the server's words (`facts`).
- **Results are read by type, and a late error is told from data.** TSV's
  escapes are undone for scalars; `Array`, `Tuple`, `Map` and the geo types
  arrive as ClickHouse literal text already and are kept raw; `Variant`,
  `Dynamic` and `JSON` are shown as the server sent them, so a string variant
  holding a tab shows `\t` (`tsv::Decoder`, `Unescape`). Bytes that are not
  UTF-8 and every `AggregateFunction` are `0x` and hex. Totals and extremes
  are further result sets. A body can end in an exception after a 200: the
  server tags it, and `Framed` finds the tag across reads; a server before 25.11
  has no tag, and a line that looks like `Code: N. DB::Exception:` and cannot
  be a row is held until it is shown to be one (`Untagged`). A cut body is an
  error, never a short result.
- **An unsorted preview pages in key order.** `LIMIT/OFFSET` without an
  `ORDER BY` is not stable in ClickHouse, so `Paging::LimitOffsetByKey` has
  `sql::paged` splice the key (`PRIMARY KEY`, from `system.tables`) in before
  the `LIMIT`; a header sort is read back from the statement before paging.
  A key holding an expression gives none, and that preview stays unordered.
- **Filters go through `toString`.** `LIKE` takes only a string, so a substring
  is `toString(col) LIKE …` and a regex `match(toString(col), …)`
  (`SubstringMatch::LikeToString`, `RegexMatch::Match`), which keeps a NULL
  NULL and answers on every type.
- **A dictionary is its own `RelationKind`, and a materialized view drops with
  `DROP VIEW`.** Copy DROP writes `DROP DICTIONARY` and Copy TRUNCATE is not
  offered; `DROP MATERIALIZED VIEW` is a syntax error. A foreign table drops as
  a table.
- **ClickHouse has no unique key, so a row is named by every value it was read
  with, and each write is guarded** (`AGENTS.md` hard rules 1 and 2). `PRIMARY
  KEY` is a sparse index and feeds paging only. A term is `col = '…'` where the
  type converts a literal exactly, `toString(col) = '…'` for floats,
  decimals, `DateTime`, `DateTime64`, `Array`, `Tuple`, `Map` and geo, and `col
  IS NULL` (`clickhouse::match_value`, `edit::spelling`). A `DateTime` is
  matched as text because the hour a clock goes back is shown twice and a
  literal names only one instant. A row with an `AggregateFunction`, `Variant`,
  `Dynamic` or `JSON` value, bytes shown as hex or a NUL cannot be matched and
  is read-only, saying why (`QueryResult::read_only_reason`). Before each
  `UPDATE`, `DELETE` or `ALTER … UPDATE`, `Connection::guarded_write` runs
  `sql::row_guard`, a `COUNT(*)` with the write's own predicate cut from the
  tree the gate reads, and refuses a count other than one: a duplicate not yet
  merged is refused. Another client can still insert an identical row between
  the count and the write; nothing in ClickHouse closes that window.
- **A query tab's result has an edit target when its statement provably reads
  one table's own rows** (owner decision): `edit::query_shape` over
  `sql::table_read_or_why` admits one plain `SELECT` of columns or `*` with
  `WHERE`, `PREWHERE`, `ORDER BY`, `LIMIT` (`BY` too) or `SAMPLE`, and refuses
  every other shape in words (`Unread`, `edit::unread`); `FINAL` is found among
  the bare words, since the grammar takes it for a table alias. `edit::origins`
  maps each result column back: a `*` by the header's names, a name as written,
  and any alias or qualifier that is also a column's name refuses, because the
  server reads that name as the alias everywhere (`SELECT a AS b, b` is `a`
  twice) and `t.x` can be a field of a column `t`. Names compare
  case-sensitively there, each result column must carry its column's type (a
  bare `inf` is the literal), and an `ALIAS` column, computed as it is read,
  refuses. A
  subset of columns names a row by those alone, which the guard keeps to one
  row. An unqualified name is resolved on the request's own session
  (`Connection::resolve`: `currentDatabase()`, and refused as a temporary
  table of the session's), never guessed from `USE`s seen.
- **A query tab's rows read after a `SET` are not matched.** The guard and the
  write stay sessionless, so the rendered text they compare must be the
  sessionless rendering: `Session::set` is raised by any user statement that
  opens with `SET` or `EXECUTE` (`sets_session`, past comments) and cleared
  only with the session. Running the guard on the session instead was
  rejected: `SET final = 1` or `apply_deleted_mask = 0` would make its count
  disagree with the rows a write touches. A run at once and a preview are
  sessionless and unaffected.
- **A query tab's edits are reviewed, never appended**
  (`Engine::reviews_query_edits`, true where `row_terms` is): a statement left
  in the buffer, or in the history, would run without its guard when run
  again. Delete row is offered on a query tab for the same engines
  (`edits_query_rows`).
- **Edit targets exist for MergeTree-family tables only, and the spelling is
  decided at describe time.** Once per table per connection
  (`Connection::describe`, forgotten when a statement that changes a table
  starts and again when it ends) from `system.tables`, `system.columns` and
  `system.merge_tree_settings`. Lightweight `UPDATE` when the server's
  `enable_lightweight_update` is on, the engine can and both
  `enable_block_number_column` and `enable_block_offset_column` are set;
  otherwise `ALTER TABLE … UPDATE … SETTINGS mutations_sync = 2`, which
  rewrites parts and returns once every replica has applied it. Sorting-key,
  primary-key and partition-key columns, `MATERIALIZED`, `ALIAS` and
  `AggregateFunction` columns cannot be set, and a `Map` cannot be edited, only
  inserted: the server casts a string to one nowhere an assignment reaches.
  Delete is the lightweight `DELETE`, withheld on a table with projections when
  `lightweight_mutation_projection_mode` is `throw`. Values go as quoted text
  and the server converts them (`input_format_values_interpret_expressions`).
- **There is no transaction.** `SqlSpec::transactions` is false, so a batch is
  not bracketed; it stops at its first refusal or error and the error says how
  many edits were applied and stay. Import Rows chunks stay as they land
  (`chunk_refusal`), and a cancelled mutation is said to keep running
  (`db::cancel_cost`). The review notes say how rows are matched, what a
  mutation costs and what stays (`review_notes`).
- **A refused lightweight `UPDATE` gets one line added, never a rewrite.** When
  the server refuses a typed `UPDATE` for want of the block columns
  (`NOT_IMPLEMENTED`, "Lightweight updates are not supported"), the error keeps
  the server's words and adds the `ALTER TABLE … UPDATE … SETTINGS
  mutations_sync = 2` line (`edit::hinted`). The statement is the user's and
  is not changed.
- **The importers read two names.** DBeaver's provider `clickhouse` and
  TablePlus's driver `ClickHouse`; a DBeaver connection given by a JDBC URL
  is skipped, since DBDelve cannot read one for this engine. DBeaver's
  provider id is from its source and unverified against a real file, and only
  the first entry of TablePlus's TLS dropdown is known; any other comes in as
  verify-full rather than something it may be stronger than.

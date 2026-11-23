//! The database boundary.
//!
//! Everything crossing out of this module is a rendered `String`. No
//! `postgres::Row`, no `rusqlite::ValueRef`, no OIDs — the UI layer never
//! learns which engine it is talking to. Dispatch is the [`Connection`] enum
//! below, and it stops here (AGENTS.md, hard rule 4).
//!
//! Each engine module owns everything about its own driver: how a value becomes
//! text, what a type is called, which catalog answers a question. What they
//! share is the vocabulary in this file — and the assemblers that turn a
//! [`QueryResult`] into a [`Catalog`], a [`Structure`] or its
//! [`ForeignKey`]s, which is why an
//! engine's catalog SQL aliases its columns to names chosen here rather than to
//! its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::tls::SslMode;

mod builtins;
mod clickhouse;
#[cfg(test)]
mod conformance;
mod duckdb;
mod external;
mod graph;
use graph::assemble_schema_graph;
mod include;
mod mongo;
mod mssql;
mod mysql;
pub(crate) mod odbc_string;
mod password_command;
pub(crate) mod plugin;
mod postgres;
mod properties;
mod redis;
mod rewrites;
pub mod roles;
mod snowflake;
mod spec;
mod sqlite;
mod ssh;
pub(crate) mod ssm;
mod tunnel;
pub use ssm::helper as ssm_helper;
pub use ssm::{ConnectAttempt, ConnectGuard, SsmTunnel};
mod target;
mod turso;
mod xlsx;

pub use builtins::{Builtin, BuiltinKind};
#[cfg(test)]
pub(crate) use clickhouse::mock as clickhouse_mock;
pub use duckdb::{IN_MEMORY as DUCKDB_IN_MEMORY, is_in_memory as duckdb_in_memory};
pub use external::ExternalConfig;
#[cfg(test)]
pub(crate) use external::echo;
pub use graph::{GraphColumn, KeyTarget, SchemaGraph};
#[cfg(test)]
pub use graph::{GraphKey, GraphTable};
pub(crate) use include::same_name;
pub use include::{Include, Shown};
pub use password_command::{PasswordCommand, loggable_error};
pub use plugin::Plugin;
pub use properties::{Properties, Property};
#[cfg(test)]
pub(crate) use snowflake::mock as snowflake_mock;
#[cfg(test)]
pub(crate) use sqlite::tests::COMMIT_ANSWER_LOST as SQLITE_COMMIT_ANSWER_LOST;
pub use ssh::askpass;

pub use mongo::MongoConfig;
pub use postgres::PostgresConfig;
pub use snowflake::{SnowflakeConfig, account_identifier, normalize_host};
#[cfg(test)]
pub use spec::DdlNames;
#[cfg(test)]
pub use spec::{Abilities, Hints, Hooks};
pub use spec::{
    BinaryLiteral, EngineSpec, ExplainPrefix, ForeignTable, FormatDialect, IdentifierQuote,
    MaterializedViewDrop, Paging, Params, ParserDialect, RegexMatch, SelectTop, SkipDuplicates,
    SqlSpec, StatementSplit, SubstringMatch, Truncate, TypedExplain,
};
pub use spec::{
    ForeignKeyReplay, IdentityInsert, ReadOnlySession, SchemaExport, SessionTimeZone,
    UnreadableFloor,
};
pub use target::{Driver, Levels, Target, cancel_cost, is_cancel};

/// One run's claim on Cancel: [`Connection::query`] runs under it, and
/// [`Connection::cancel`] stops only what ran under it.
///
/// Snowflake is why it exists. Every tab and every catalog load there is a
/// statement of its own in flight on the one connection at once, so a cancel
/// has to know whose to stop. Postgres, MySQL and SQLite run one statement at
/// a time behind their mutex and stop that one, whoever's it is. A plugin
/// runs one at a time too, but every lane of a queue run at once waits on its
/// one process, so it stops only the request its run sent.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Mutex<Cancelling>>);

/// Whether a cancel has been asked for, which has to outlast a statement whose
/// handle is still on its way, and the handles there are to stop.
#[derive(Default)]
struct Cancelling {
    asked: bool,
    handles: Vec<String>,
    /// `Some` for a run made by [`CancelToken::alongside`]: the connections
    /// its statements are running on, each emptied once its statement ends.
    alongside: Option<Vec<Option<Connection>>>,
    /// Held by a Cancel for as long as it is sending to `alongside`'s
    /// connections, and taken by a statement before it empties its place
    /// there: a Cancel that found the statement is sent before the statement
    /// lets go of its connection, and never reaches what runs there next.
    /// Taken before the lock on this, never under it.
    sending: Arc<Mutex<()>>,
}

impl CancelToken {
    /// For a run whose statements each go out on a connection of their own
    /// ([`Connection::query_alongside`]). A cancel under it stops those, and
    /// never what the connection it is asked on is running, which is some
    /// other tab's statement or nothing, unless one of its statements fell
    /// back to that connection and is running there.
    pub fn alongside() -> Self {
        Self(Arc::new(Mutex::new(Cancelling {
            alongside: Some(Vec::new()),
            ..Cancelling::default()
        })))
    }

    /// Whether the two are one run's.
    pub fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Mark the run cancelled and `send` to the connections it is running on,
    /// each still held by its statement until `send` returns; `None` for a run
    /// on the profile's own.
    fn stop_alongside<T>(&self, send: impl FnOnce(&[Connection]) -> T) -> Option<T> {
        let sending = self.0.lock().ok()?.sending.clone();
        let _sending = sending.lock().unwrap_or_else(PoisonError::into_inner);
        let connections: Vec<Connection> = {
            let mut running = self.0.lock().ok()?;
            let connections = running
                .alongside
                .as_ref()?
                .iter()
                .flatten()
                .cloned()
                .collect();
            running.asked = true;
            connections
        };
        Some(send(&connections))
    }
}

/// A Cancel kept for a schema graph read on Postgres, MySQL and SQL Server,
/// whose several statements leave the session idle before and between them:
/// a Cancel that lands there stops nothing, so the read's next statement
/// takes it from here and is dropped unsent ([`Fetch::held`]). One with no
/// read under way is kept for the next, as a plugin keeps one.
#[derive(Clone, Default)]
struct GraphCancel(Arc<AtomicBool>);

impl GraphCancel {
    fn ask(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Runs a graph read whose statements are each held by `held`. A Cancel
    /// that came while it ran was for it, so none outlives it.
    fn read<T>(&self, read: impl FnOnce(&dyn Fn() -> bool) -> T) -> T {
        let read = read(&|| !self.0.load(Ordering::SeqCst));
        self.0.store(false, Ordering::SeqCst);
        read
    }
}

/// What a statement a Cancel reached before it held its connection fails
/// with: it was dropped unsent, so the server never saw it.
pub(crate) const CANCELLED_UNSENT: &str = "Cancelled before it was sent.";

/// What a SQL Server statement a Cancel reached while it waited for its
/// session fails with: dropped unsent, as [`CANCELLED_UNSENT`] is.
pub(crate) const CANCELLED_QUEUED: &str =
    "Cancelled before it started: nothing was sent to the server.";

/// Whether `error` says the statement never reached the server: a Cancel
/// dropped it before its turn ([`CANCELLED_UNSENT`], [`CANCELLED_QUEUED`]), or
/// the binding put back ahead of it failed ([`unplaced`]). What it would have
/// changed on the session, a transaction's end among it, did not happen.
pub(crate) fn sent_nothing(error: &DbError) -> bool {
    error.message == CANCELLED_UNSENT
        || error.message == CANCELLED_QUEUED
        || error.message.starts_with(UNPLACED)
}

/// How long a keep-alive ping waits for its answer before the session it
/// went out on is taken for lost: a peer that stops answering without
/// closing the socket would otherwise hold the session from every later
/// statement, waiting behind the ping.
pub(crate) const PING_WITHIN: Duration = match cfg!(test) {
    true => Duration::from_secs(1),
    false => Duration::from_secs(30),
};

/// What a driver adds to an error when the server rolled back the
/// transaction open before the statement on its own (SQL Server under
/// `XACT_ABORT`): the session is autocommitting again.
pub(crate) const TRANSACTION_ROLLED_BACK: &str = "The transaction that was open before this \
     statement was rolled back, and everything written in it is gone.";

/// What a driver adds to an error when the transaction open before the
/// statement is gone and it cannot tell how: the statement committed it
/// before failing, or the server rolled it back.
pub(crate) const TRANSACTION_ENDED: &str = "The transaction that was open before this \
     statement has ended: committed by the statement before it failed, or rolled back by \
     the server. Check what it wrote before running it again.";

/// What a driver adds to an error when a statement that would have ended the
/// open transaction failed short of it (SQL Server's compile error, which
/// stops the batch before its `COMMIT`): the transaction is still open.
pub(crate) const TRANSACTION_KEPT_OPEN: &str =
    "The transaction that was open before this statement is still open.";

/// Whether the driver says `error` ended the open transaction
/// ([`TRANSACTION_ROLLED_BACK`]).
pub(crate) fn transaction_rolled_back(error: &DbError) -> bool {
    error.message.ends_with(TRANSACTION_ROLLED_BACK)
}

/// Whether the driver says `error` ended the open transaction, rolled back
/// ([`TRANSACTION_ROLLED_BACK`]) or one way or the other
/// ([`TRANSACTION_ENDED`]).
pub(crate) fn transaction_ended(error: &DbError) -> bool {
    transaction_rolled_back(error) || error.message.ends_with(TRANSACTION_ENDED)
}

/// What a driver adds to a statement's notice when it ended a transaction and
/// one is still open: SQL Server's `COMMIT` inside a nested `BEGIN
/// TRANSACTION`, which takes one level off and commits nothing.
pub(crate) const TRANSACTION_STILL_OPEN: &str = "A transaction is still open: this ended \
     only the innermost level of the transactions nested in it, and nothing is committed \
     until the outermost one ends.";

/// Whether the driver says the transaction outlived a statement that ended
/// one ([`TRANSACTION_STILL_OPEN`]).
pub(crate) fn transaction_still_open(result: &QueryResult) -> bool {
    result
        .notice
        .as_deref()
        .is_some_and(|notice| notice.ends_with(TRANSACTION_STILL_OPEN))
}

/// Whether the driver says the open transaction outlived `error`
/// ([`TRANSACTION_KEPT_OPEN`]).
pub(crate) fn transaction_kept_open(error: &DbError) -> bool {
    error.message.ends_with(TRANSACTION_KEPT_OPEN)
}

const UNPLACED: &str = "The statement was not sent:";

/// An error from putting a bound tab's database and schema back, which is
/// dbdelve's own call and not the user's statement: no byte of their text is
/// where it points, and it says nothing was sent.
fn unplaced(error: DbError) -> DbError {
    DbError {
        message: format!(
            "{UNPLACED} its tab's database and schema could not be set again. {}",
            error.message
        ),
        position: None,
    }
}

/// Runs `run`, handed a `held` for the statement to ask once it holds
/// `connection`: `true` makes `connection` the one a Cancel on `cancel` (made
/// by [`CancelToken::alongside`]) stops, `false` says the Cancel came first
/// and the statement is dropped unsent. And a `let_go` for it to call before
/// it lets go of `connection` ([`Fetch::let_go`]), after which no Cancel on
/// `cancel` reaches it; called again when `run` ends, for a statement that
/// asked `held` itself.
///
/// A connection that is shared becomes the run's to stop only once this
/// statement holds it; until then a Cancel just marks the run, and the
/// statement is dropped unsent when its turn comes. It stops being the run's
/// before the next statement can take it.
fn running_on<T>(
    cancel: &CancelToken,
    connection: &Connection,
    run: impl FnOnce(&dyn Fn() -> bool, &dyn Fn()) -> T,
) -> T {
    let slot = std::cell::Cell::new(None);
    let hold = || {
        let mut running = cancel.0.lock().unwrap_or_else(PoisonError::into_inner);
        if running.asked {
            return false;
        }
        let alongside = running.alongside.get_or_insert_default();
        alongside.push(Some(connection.clone()));
        slot.set(Some(alongside.len() - 1));
        true
    };
    let let_go = || {
        let Some(slot) = slot.take() else {
            return;
        };
        // A Cancel sending to this connection finishes first.
        let sending = cancel
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sending
            .clone();
        let _sending = sending.lock().unwrap_or_else(PoisonError::into_inner);
        let mut running = cancel.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(alongside) = &mut running.alongside {
            alongside[slot] = None;
        }
    };
    let result = run(&hold, &let_go);
    let_go();
    result
}

/// Which engine a profile talks to.
///
/// Also the answer to the only three questions dbdelve's own generated SQL asks
/// about dialect. There being three is why there is no `Dialect` type: an
/// engine quotes an identifier, quotes a literal, and qualifies a name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Engine {
    #[default]
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    Turso,
    DuckDb,
    Snowflake,
    ClickHouse,
    SqlServer,
    MongoDb,
    Redis,
    /// SQL with no dialect of its own, reached through a plugin. No built-in
    /// driver: nothing in DBDelve opens a connection to it.
    Generic,
}

/// The shape of a connection's details. The form draws one of these and never
/// asks which engine it is drawing for (hard rule 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fields {
    /// Host, port, database, user, password and an `sslmode`.
    Server,
    /// A path and nothing else.
    File,
    /// An account reached over HTTPS with a private key.
    Account,
    /// A plugin driver's own connection string, a user and a password: the
    /// address is in the string, in keys only the driver knows.
    ConnectionString,
}

/// What a query buffer is written in: what the editor highlights it as, what
/// completion reads it as, and whether a row can be copied as a statement that
/// recreates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Sql,
    /// mongosh statements, which are JavaScript expressions.
    Mongo,
    /// redis-cli commands, one a line.
    Redis,
}

impl Syntax {
    /// gpui-component's name for the grammar it highlights with.
    pub fn highlighter(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::Mongo => "javascript",
            // A shell grammar: it colours the command word and quoted strings,
            // which is what a redis-cli line has.
            Self::Redis => "bash",
        }
    }

    pub fn line_comment(self) -> &'static str {
        match self {
            Self::Sql => "--",
            Self::Mongo => "//",
            Self::Redis => "#",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Sql => "Write SQL…",
            Self::Mongo => "Write a query, like db.collection.find({})…",
            Self::Redis => "Write a command, like GET key…",
        }
    }
}

/// How much the server should be asked to do to answer "how would you run
/// this?".
///
/// The distinction is not a detail of presentation: `Plan` only plans, while
/// `Analyze` *runs the statement* to report what it really cost. Explaining a
/// `DELETE` under `Analyze` deletes. That is why the two are a choice the user
/// makes in front of the button rather than a mode dbdelve picks for them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExplainMode {
    /// Plan only. Never executes the statement.
    #[default]
    Plan,
    /// Executes the statement and reports the timings and row counts it
    /// actually saw.
    Analyze,
}

impl ExplainMode {
    pub const ALL: [Self; 2] = [Self::Plan, Self::Analyze];

    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "Explain",
            Self::Analyze => "Explain Analyze",
        }
    }

    /// Said in front of the choice, because the cost of picking wrong is a
    /// write the user did not mean to make. What each one *does*, in the
    /// server's own vocabulary -- not a sentence about it.
    pub fn caption(self) -> &'static str {
        match self {
            Self::Plan => "print query plan",
            Self::Analyze => "run query and print query plan",
        }
    }
}

/// Why a profile cannot be made for `Engine::Generic` from anything but a plugin.
pub(crate) const GENERIC_REFUSAL: &str =
    "Generic SQL has no driver of its own; connect through ODBC.";

impl Engine {
    /// Everything that sets this engine apart, as data.
    pub fn spec(self) -> &'static EngineSpec {
        match self {
            Self::Postgres => &spec::POSTGRES,
            Self::MySql => &spec::MYSQL,
            Self::MariaDb => &spec::MARIADB,
            Self::Sqlite => &spec::SQLITE,
            Self::Turso => &spec::TURSO,
            Self::DuckDb => &spec::DUCKDB,
            Self::Snowflake => &spec::SNOWFLAKE,
            Self::ClickHouse => &spec::CLICKHOUSE,
            Self::SqlServer => &spec::SQL_SERVER,
            Self::MongoDb => &spec::MONGODB,
            Self::Redis => &spec::REDIS,
            Self::Generic => &spec::GENERIC,
        }
    }

    /// Whether DBDelve opens connections to this engine itself, which the
    /// pickers that make a profile list it by.
    pub fn has_built_in_driver(self) -> bool {
        self.spec().built_in_driver
    }

    /// Whether a plugin driver may carry this dialect: SQL whose Read-only
    /// hold, where it has one, is a statement the plugin can run on its one
    /// session. ClickHouse's hold rides on each request, which nothing a
    /// plugin is sent can carry, so offering it would hold nothing.
    pub fn speaks_through_plugins(self) -> bool {
        self.syntax() == Syntax::Sql
            && self.spec().sql.read_only_session != ReadOnlySession::PerRequest
    }

    /// The engine's brand logo.
    pub fn logo(self) -> &'static str {
        self.spec().logo
    }

    /// The engine's brand colour as 0xRRGGBB. The theme reads only its hue.
    pub fn brand(self) -> u32 {
        self.spec().brand
    }

    /// Presentation order, which is the order the form's chips appear in.
    pub const ALL: [Self; 12] = [
        Self::Postgres,
        Self::MySql,
        Self::MariaDb,
        Self::Sqlite,
        Self::Turso,
        Self::DuckDb,
        Self::Snowflake,
        Self::ClickHouse,
        Self::SqlServer,
        Self::MongoDb,
        Self::Redis,
        Self::Generic,
    ];

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    /// The spelling stored in `profiles.toml`. Changing one of these strings
    /// orphans every profile already written with it.
    pub fn as_str(self) -> &'static str {
        self.spec().id
    }

    /// Every spelling [`Engine::parse`] reads: the URL schemes as well as the
    /// stored spellings. Listed so the welcome surface can tell a scheme still
    /// being typed from one that is no scheme at all.
    pub fn schemes() -> impl Iterator<Item = (&'static str, Self)> {
        Self::ALL.into_iter().flat_map(|engine| {
            engine
                .spec()
                .schemes
                .iter()
                .map(move |scheme| (*scheme, engine))
        })
    }

    /// Accepts the URL schemes as well as the stored spellings, so one function
    /// serves both the profile reader and the URL box.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim().to_ascii_lowercase();
        Self::schemes()
            .find(|(spelling, _)| *spelling == value)
            .map(|(_, engine)| engine)
            .ok_or_else(|| format!("{value} is not a database engine dbdelve speaks."))
    }

    /// The engine a driver's reported DBMS name (`SQL_DBMS_NAME`) belongs to,
    /// by prefix and whatever its case. Read only by the connection form's
    /// dialect suggestion.
    pub fn from_dbms_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|engine| {
            engine
                .spec()
                .dbms_names
                .iter()
                .any(|known| name.starts_with(&known.to_ascii_lowercase()))
        })
    }

    /// The engine whose files end in `extension`, whatever its case.
    pub fn from_extension(extension: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|engine| {
            engine
                .spec()
                .file_extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
    }

    /// The port a blank one means, where the engine has one.
    pub fn default_port(self) -> Option<u16> {
        self.spec().default_port
    }

    /// Which set of fields makes a connection to this engine, which is what
    /// the form asks before drawing any. A server's host, credentials and TLS
    /// are absent for a file, and an account has a key where a server has a
    /// password and no transport to choose.
    pub fn fields(self) -> Fields {
        self.spec().layout
    }

    /// Whether the form offers a field of driver options passed through as
    /// typed: MongoDB's connection-string options (`authSource`, `replicaSet`,
    /// `readPreference`, …), which outnumber any set of fields worth drawing.
    pub fn takes_options(self) -> bool {
        self.spec().takes_options
    }

    /// Whether a profile can name a section of a connection service file
    /// rather than spell out where to connect.
    pub fn takes_service(self) -> bool {
        self.spec().takes_service
    }

    /// Whether the host may instead be a DNS name whose SRV records list the
    /// servers (`mongodb+srv`), which the form offers as a toggle.
    pub fn resolves_srv(self) -> bool {
        self.spec().resolves_srv
    }

    /// What `mode` is spelled as here, or `None` where the engine has no such
    /// mode. Returned as a prefix because that is the whole of the difference:
    /// the statement it is put in front of is the user's, unchanged.
    pub fn explain_prefix(self, mode: ExplainMode) -> Option<&'static str> {
        match self.syntax() {
            Syntax::Sql => {
                let sql = &self.spec().sql;
                let prefix = match mode {
                    ExplainMode::Plan => sql.explain_plan,
                    ExplainMode::Analyze => sql.explain_analyze,
                };
                prefix.map(|prefix| match prefix {
                    ExplainPrefix::Explain => "EXPLAIN ",
                    ExplainPrefix::ExplainAnalyze => "EXPLAIN ANALYZE ",
                    ExplainPrefix::Analyze => "ANALYZE ",
                    ExplainPrefix::ExplainQueryPlan => "EXPLAIN QUERY PLAN ",
                })
            }
            // A plan there is `.explain()` on the end of the statement, which
            // is `explain_suffix`; nothing goes in front.
            Syntax::Mongo => Some(""),
            // No command plans another.
            Syntax::Redis => None,
        }
    }

    /// `sql` as the statement that explains it in `mode`, or `None` where
    /// `explain_prefix` is.
    pub fn explain(self, mode: ExplainMode, sql: &str) -> Option<String> {
        self.explain_prefix(mode)
            .map(|prefix| format!("{prefix}{sql}{}", self.explain_suffix(mode)))
    }

    /// What goes after the statement to put it in `mode`: nothing on the SQL
    /// engines, whose whole difference is the prefix, and the cursor's own
    /// `.explain(verbosity)` on MongoDB.
    pub fn explain_suffix(self, mode: ExplainMode) -> &'static str {
        match (self.syntax(), mode) {
            (Syntax::Mongo, ExplainMode::Plan) => r#".explain("queryPlanner")"#,
            (Syntax::Mongo, ExplainMode::Analyze) => r#".explain("executionStats")"#,
            (Syntax::Sql | Syntax::Redis, ExplainMode::Plan | ExplainMode::Analyze) => "",
        }
    }

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` for the engine that already makes one submission
    /// atomic. `COMMIT` and `ROLLBACK` are spelled the same everywhere, so the
    /// opening word is the whole of the difference.
    pub fn transaction_start(self) -> Option<&'static str> {
        match self.syntax() {
            Syntax::Sql => self.spec().sql.transaction_opening(),
            // No statement opens one: a batch applies in order, and a failure
            // says which rows already did. Redis's `MULTI` queues commands
            // without rolling back the ones that fail inside it.
            Syntax::Mongo | Syntax::Redis => None,
        }
    }

    /// Whether a connection to this engine verifies an apply: has an arm of
    /// [`Connection::apply_verified`]. An explicit arm per engine for the reason
    /// [`Engine::transaction_start`] has one: the engines without one keep
    /// [`Connection::generated`], and one added later must be decided here.
    pub fn verifies_applies(self) -> bool {
        match self {
            Self::Sqlite
            | Self::DuckDb
            | Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::SqlServer
            | Self::MongoDb => true,
            Self::Turso | Self::Snowflake | Self::ClickHouse | Self::Redis | Self::Generic => false,
        }
    }

    /// How a transaction the user drives is opened on this engine, for the
    /// `BEGIN` Begin Transaction writes into their buffer. `None` where there
    /// is none to drive.
    ///
    /// Not [`Engine::transaction_start`], which answers a different question:
    /// whether a *generated* batch needs brackets of its own. Postgres needs
    /// none there, because one submission is already one implicit
    /// transaction, and still opens a transaction here, because an explicit
    /// `BEGIN` is held across submissions by the session. Keeping them apart
    /// is the point; collapsing them would silently stop opening transactions
    /// on Postgres. An explicit arm per engine for the reason
    /// `transaction_start` has one.
    pub fn begins_transaction(self) -> Option<&'static str> {
        match self {
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Turso
            | Self::DuckDb => Some("BEGIN"),
            // A bare `BEGIN` opens a statement block in T-SQL, not a
            // transaction, and this spelling is the one the gate reads.
            Self::SqlServer => Some("BEGIN TRANSACTION"),
            // Each request is a session of its own (`stateless_requests`), so
            // a `BEGIN` ends with its request and every later write commits
            // on its own under a switch that would read Manual.
            Self::Snowflake => None,
            // `MULTI` queues commands and rolls none of them back, and
            // MongoDB's transactions need a client session dbdelve does not
            // hold open across statements.
            Self::MongoDb | Self::Redis => None,
            // Its spec says `transactions: false`: no statement opens one that
            // a later submission could close, so there is nothing to drive.
            Self::ClickHouse => None,
            // A plugin's server is not known to read `BEGIN` as opening a
            // transaction (Db2 has no such statement), so none is driven.
            Self::Generic => None,
        }
    }

    /// Whether a `BEGIN` sent inside an open transaction commits it first.
    ///
    /// MySQL and MariaDB do: `START TRANSACTION` implicitly commits the
    /// current transaction. Postgres warns and keeps the open one, Snowflake
    /// ignores the second, SQL Server only raises its nesting count, and
    /// SQLite, Turso and DuckDB refuse the `BEGIN`. Answering `false` where
    /// unsure keeps statements listed rather than dropping them, which is the
    /// safe side for the list of what the next `COMMIT` ends.
    pub fn begin_ends_open_transaction(self) -> bool {
        matches!(self, Self::MySql | Self::MariaDb)
    }

    /// Whether a statement that fails inside a transaction leaves it able
    /// only to roll back: Postgres refuses every later statement but its end,
    /// and answers a `COMMIT` by rolling it back, without an error.
    pub fn failure_aborts_transaction(self) -> bool {
        self == Self::Postgres
    }

    /// Whether a `COMMIT` or `ROLLBACK` that failed with `message` left the
    /// transaction open on the session, so a profile that read it as ended
    /// should read it as open again.
    ///
    /// Only SQLite is known to: a `COMMIT` that meets `SQLITE_BUSY` or a
    /// deferred foreign key keeps the transaction for a retry, unless the
    /// failure is that none was active. Postgres, MySQL and DuckDB roll back
    /// a transaction whose `COMMIT` fails, and for the others it is not
    /// known. `false` is the safe answer: a profile reading "open" over a
    /// session that is not offers a Roll Back for work already committed, and
    /// cannot be cleared by a `COMMIT` that has nothing to end.
    pub fn failed_end_keeps_transaction(self, message: &str) -> bool {
        self == Self::Sqlite
            && !message
                .to_ascii_lowercase()
                .contains("no transaction is active")
    }

    /// Whether `SET column = DEFAULT` is an assignment this engine accepts.
    ///
    /// Where it is not, the gesture has to be withheld rather than attempted.
    /// It answers here for the reason [`Engine::transaction_start`] does: the
    /// question is about which engine is connected, and rule 4 keeps every one
    /// of those inside `src/db/` — the caller asks, and never matches.
    pub fn assigns_default(self) -> bool {
        self.spec().abilities.assigns_default
    }

    /// Whether the server fills `column` itself and an insert leaves it out:
    /// one it refuses a value for, or `AUTO_INCREMENT`, which takes one but is
    /// practically never given it. Read off the markers the structure queries
    /// write into `default`, so a plain default -- `now()`, a serial's
    /// `nextval`, an identity `BY DEFAULT` -- is not one.
    pub fn generates(self, column: &ColumnDefinition) -> bool {
        // MongoDB's and Redis's spec carry no markers, which is their answer
        // too.
        self.spec().generates(column)
    }

    /// Whether New row's form leaves `column` out because no value of it is
    /// one a new row can take: a Redis key's type, which is whatever command
    /// made the key, and a new key is a `SET`.
    pub fn new_row_omits(self, column: &str) -> bool {
        match self.syntax() {
            Syntax::Redis => column == "type",
            Syntax::Sql | Syntax::Mongo => false,
        }
    }

    /// Whether New row sends a field typed into and emptied again as an empty
    /// string. MongoDB leaves an empty field out of the document, and Redis
    /// leaves out an empty one that is not text.
    pub fn new_row_sends_empty(self, column: &str) -> bool {
        match self.syntax() {
            Syntax::Sql => true,
            Syntax::Mongo => false,
            Syntax::Redis => !crate::redis_cmd::edit::empty_is_none(column),
        }
    }

    /// What the review of a generated batch says beyond its statements: how
    /// the engine matches rows, what it cannot undo, what it cannot write.
    /// Empty for every engine that has nothing to add.
    pub fn review_notes(self, sql: &str) -> Vec<String> {
        match self {
            Self::ClickHouse => clickhouse::review_notes(sql),
            _ => Vec::new(),
        }
    }

    /// How a boolean is spelled as an Import Rows value. Import Rows reads
    /// `spec().sql.boolean_literal` itself (`sql::sql_insert_rows`), so this is
    /// the per-engine readout the tests pin.
    #[cfg(test)]
    pub fn boolean_literal(self, value: bool) -> &'static str {
        let literal = match self.syntax() {
            Syntax::Sql => self.spec().sql.boolean_literal,
            Syntax::Mongo => spec::BooleanLiteral::TrueFalse,
            Syntax::Redis => spec::BooleanLiteral::OneZero,
        };
        literal.spelled(value)
    }

    /// Whether an insert can be told to leave a row that collides with an
    /// existing key alone.
    pub fn skips_duplicates(self) -> bool {
        // MongoDB's and Redis's spec say `None`, which is their answer too.
        self.spec().sql.skip_duplicates != SkipDuplicates::None
    }

    /// [`Engine::skips_duplicates`] for an insert into a table of `structure`.
    pub fn skips_duplicates_into(self, structure: &Structure) -> bool {
        match self.spec().sql.skip_duplicates {
            SkipDuplicates::None => false,
            SkipDuplicates::OnConflictDoNothing {
                needs_unique_key: true,
            } => {
                structure.constraints.iter().any(|constraint| {
                    constraint.definition.starts_with("PRIMARY KEY (")
                        || constraint.definition.starts_with("UNIQUE (")
                }) || structure
                    .indexes
                    .iter()
                    .any(|index| index.kind != IndexKind::Plain)
            }
            SkipDuplicates::OnConflictDoNothing {
                needs_unique_key: false,
            }
            | SkipDuplicates::OnDuplicateKey => true,
        }
    }

    /// The statement that opens a transaction spanning an import's chunks, or
    /// why there cannot be one.
    pub fn all_or_nothing(self) -> Result<&'static str, &'static str> {
        let spec = self.spec();
        match spec.abilities.chunk_refusal {
            Some(reason) => Err(reason),
            // Not `transaction_opening`: one submission being atomic says
            // nothing of a transaction across several.
            None => Ok(spec.sql.transaction.opening()),
        }
    }

    /// Whether an import may map a column to a field the sample has not seen.
    /// A collection has no fixed columns, and an unmapped field is left out
    /// rather than written as null.
    pub fn takes_new_fields(self) -> bool {
        match self.syntax() {
            Syntax::Mongo => true,
            Syntax::Sql | Syntax::Redis => false,
        }
    }

    /// The column of a relation's preview naming what each of its rows opens
    /// as a result of its own ([`crate::explorer::open_row_sql`]): a Redis key,
    /// whose contents are a read of their own. `None` where a row is all there
    /// is.
    pub fn opened_column(self) -> Option<&'static str> {
        match self.syntax() {
            Syntax::Redis => Some("key"),
            Syntax::Sql | Syntax::Mongo => None,
        }
    }

    /// Whether a header click can ask the server for an order. A Redis reply's
    /// order is the command's own, so a view of one sorts the rows it holds.
    pub fn sorts_on_server(self) -> bool {
        match self.syntax() {
            Syntax::Sql | Syntax::Mongo => true,
            Syntax::Redis => false,
        }
    }

    /// Whether a preview's statement carries its row limit and its page's
    /// offset. A Redis `SCAN` has neither: the fetch keeps the limit, and a
    /// cursor walk has no page after the first that it can be asked for.
    pub fn pages(self) -> bool {
        match self.syntax() {
            Syntax::Sql | Syntax::Mongo => true,
            Syntax::Redis => false,
        }
    }

    /// Whether a preview page is ordered by the relation's key when nothing
    /// else sorts it, which `OFFSET … FETCH` paging needs and an unsorted
    /// ClickHouse page needs to be stable (`sql::paged` writes it).
    pub fn pages_by_key(self) -> bool {
        self.spec().sql.paging != Paging::LimitOffset
    }

    /// The aggregate a relation's row count is written with.
    #[cfg(test)]
    pub fn count_all(self) -> &'static str {
        // MongoDB and Redis write no SQL, so their spec's answer is never
        // what their count runs.
        self.spec().sql.count_all.aggregate()
    }

    /// Whether the server holds a Read-only session to reads -- the backstop
    /// `read_only_statement` sets. Without one, a statement `sql::classify`
    /// cannot read has nothing behind it that would stop a write, so the gate
    /// refuses it in Read-only instead of offering to run it once.
    pub fn holds_read_only(self) -> bool {
        self.spec().sql.read_only_session != ReadOnlySession::None
    }

    /// Why a statement `sql::classify` cannot read is refused in Read-only,
    /// finishing "dbdelve can't parse this, and …".
    pub fn unreadable_reason(self) -> &'static str {
        match self.spec().sql.unreadable_floor {
            UnreadableFloor::None => {
                "this database has no server-side read-only setting to stop it if it writes"
            }
            UnreadableFloor::FirstKeyword => {
                "the server's read-only setting does not stop every statement of this kind"
            }
        }
    }

    /// Whether switching the schema has to wait for an open transaction to
    /// end, because the switch would be sent inside it and undone by the
    /// user's `ROLLBACK`.
    pub fn schema_switch_waits_for_transaction(self) -> bool {
        self.spec().abilities.schema_switch_waits_for_transaction
    }

    /// What the filter bar's raw input asks for.
    pub fn raw_filter_placeholder(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "SQL…",
            Syntax::Mongo => "{ status: \"active\" }",
            Syntax::Redis => "user:*",
        }
    }

    /// The column dropdown's entry that swaps the bar for the user's own filter.
    pub fn raw_filter_label(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "Raw SQL",
            Syntax::Mongo => "Raw filter",
            Syntax::Redis => "MATCH pattern",
        }
    }

    /// What a generated statement is called where one is shown for review.
    pub fn review_label(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "Review SQL",
            Syntax::Mongo => "Review statement",
            Syntax::Redis => "Review commands",
        }
    }

    pub fn syntax(self) -> Syntax {
        self.spec().syntax
    }

    /// A database the server keeps for itself, listed after the user's own.
    pub fn is_system_database(self, name: &str) -> bool {
        self.spec().system_databases.contains(&name)
    }

    pub fn quote_identifier(self, identifier: &str) -> String {
        match self.syntax() {
            Syntax::Sql => self.spec().quote_identifier(identifier),
            // A field name is a JavaScript string, escaped with a backslash
            // rather than doubled.
            Syntax::Mongo => json_string(identifier),
            Syntax::Redis => crate::redis_cmd::quoted(identifier),
        }
    }

    /// [`Self::quote_identifier`], for a name that is not one a bare spelling
    /// reads back as: Postgres folds `Audit` to `audit`, Snowflake `events` to
    /// `EVENTS`, and `Audit Log` is two words. For a name dbdelve writes where
    /// the user reads it, where quoting every `users` would be noise.
    pub fn quote_identifier_if_needed(self, identifier: &str) -> String {
        let folded = |byte: u8| match self.spec().sql.identifier_folds_upper {
            true => byte.is_ascii_uppercase(),
            false => byte.is_ascii_lowercase(),
        };
        let mut bytes = identifier.bytes();
        let plain = bytes
            .next()
            .is_some_and(|first| folded(first) || first == b'_')
            && bytes.all(|byte| folded(byte) || byte.is_ascii_digit() || byte == b'_');
        match plain {
            true => identifier.to_string(),
            false => self.quote_identifier(identifier),
        }
    }

    /// The inverse, for reading back a name dbdelve wrote — matching a sort key in
    /// a statement to the column header it belongs to, say.
    ///
    /// Anything that is not a quoted identifier comes back unchanged: a bare
    /// position or a function call names no column, and pretending otherwise
    /// would light up the wrong header.
    pub fn unquote_identifier(self, expression: &str) -> String {
        match self.syntax() {
            Syntax::Sql => self.spec().unquote_identifier(expression),
            Syntax::Mongo => serde_json::from_str::<String>(expression)
                .unwrap_or_else(|_| expression.to_string()),
            Syntax::Redis => match crate::redis_cmd::split(expression).as_deref() {
                Ok([word]) => String::from_utf8_lossy(word).into_owned(),
                _ => expression.to_string(),
            },
        }
    }

    /// Whether a date literal may carry a UTC offset.
    pub fn reads_literal_offsets(self) -> bool {
        self.spec().sql.literal_offsets
    }

    /// The functions and value expressions this dialect has built in, as it
    /// spells them.
    pub fn builtins(self) -> &'static [Builtin] {
        self.spec().sql.builtins
    }

    /// Whether a backslash in a single-quoted literal escapes the character
    /// after it.
    #[cfg(test)]
    pub fn backslash_escapes(self) -> bool {
        self.spec().sql.backslash_escapes
    }

    pub fn quote_literal(self, value: &str) -> String {
        match self.syntax() {
            Syntax::Sql => self.spec().quote_literal(value),
            Syntax::Mongo => json_string(value),
            Syntax::Redis => crate::redis_cmd::quoted(value),
        }
    }

    /// [`Self::quote_literal`] for a value bound for a column of `data_type`,
    /// on an engine where the type changes how its literal is spelled.
    pub fn quote_value(self, value: &str, data_type: Option<&str>) -> String {
        match self.syntax() {
            Syntax::Sql => self.spec().quote_value(value, data_type),
            Syntax::Mongo | Syntax::Redis => self.quote_literal(value),
        }
    }

    /// [`Hooks::fetched_literal`].
    pub fn fetched_literal(self, value: &str, data_type: &str) -> Option<String> {
        (self.spec().hooks.fetched_literal?)(value, data_type)
    }

    /// Whether a [`Column::data_type`] names a type whose values are bytes.
    ///
    /// What it is for: a blob is rendered as the engine's own literal — `x'AB'`,
    /// `0xAB` — and [`Engine::quote_literal`] would quote that back as the six
    /// characters it looks like, so an edited blob column becomes text. The grid
    /// writes such a cell as bytes ([`Engine::binary_literal`]) or refuses the
    /// edit instead. Absent here means unknown, not text: a driver
    /// that could not name the type says nothing, and treating silence as binary
    /// would make ordinary columns read-only.
    pub fn is_binary_type(self, data_type: &str) -> bool {
        self.spec().is_binary_type(data_type)
    }

    /// The first `most` bytes of a value of a binary type
    /// ([`Engine::is_binary_type`]) as this engine's driver rendered it, or
    /// `None` where `value` is not its spelling of bytes. What the grid shows
    /// a column parsed as text or base64 from, and what an edit typed in the
    /// driver's own spelling is read back to bytes by.
    pub fn bytes_of(self, value: &str, most: usize) -> Option<Vec<u8>> {
        self.spec().bytes_of(value, most)
    }

    /// `bytes` spelled as this engine's driver renders them, which
    /// [`Engine::bytes_of`] reads back: how a staged edit of bytes is shown
    /// beside the values the server sent. `None` where no column is binary.
    pub fn bytes_text(self, bytes: &[u8]) -> Option<String> {
        self.spec().bytes_text(bytes)
    }

    /// `bytes` as a literal a grid edit sets a binary column to, or `None`
    /// where the engine writes none, or none of these bytes: SQL Server's
    /// empty `0x` is no literal the write gate's grammar reads. Whether a
    /// target writes bytes at all is [`Target::edits_bytes`]'s to say.
    pub fn binary_literal(self, bytes: &[u8]) -> Option<String> {
        self.spec().binary_literal(bytes)
    }

    /// Whether bytes already arrive as hex digits, so that showing them as
    /// hex is showing them as they came.
    pub fn shows_bytes_as_hex(self) -> bool {
        self.spec().binary_text != Some(spec::BinaryText::BinData)
    }

    /// Whether a column of `data_type` holds numbers on this engine: the names
    /// [`is_numeric_type`] knows, plus the ones its spec adds, read through
    /// the wrappers its spec peels.
    pub fn is_numeric_type(self, data_type: &str) -> bool {
        self.spec().is_numeric_type(data_type)
    }

    /// Whether a cell whose type is known as each of `types` -- none, where
    /// no driver named one -- holds what an edit of it would write back as
    /// text: not bytes ([`Engine::is_binary_type`]), and on Redis only text that is the
    /// whole of its value, never the start of a long one, a collection's size
    /// or the spelling of bytes that are not text. Whether rows are written
    /// back at all is the target's to say ([`Target::edits_rows`]): Generic
    /// SQL edits nothing built in and does through a plugin.
    pub fn writes_back<'a>(self, types: impl IntoIterator<Item = &'a str>) -> bool {
        self.spec().writes_back(types)
    }

    /// Whether a query tab's edits are shown in the review dialog an object
    /// tab's are, rather than appended to its buffer. A row named by every
    /// value is written only behind the exactly-one-row count DBDelve runs
    /// before each write; a copy left in the buffer would run without it when
    /// the user ran it again.
    pub fn reviews_query_edits(self) -> bool {
        self.syntax() == Syntax::Sql && self.spec().sql.row_terms
    }

    /// Whether `column`, part of what names a grid's row, can be edited all
    /// the same. Elsewhere a key column says which row an edit lands on and is
    /// never what it changes. On Redis a key's name, a set's member, a sorted
    /// set's member and score and a string's value are the row, and an edit
    /// of one is written as a `RENAMENX`, or as a removal and an addition.
    pub fn edits_key_column(self, column: &str) -> bool {
        match self.syntax() {
            Syntax::Redis => crate::redis_cmd::edit::edits_key_column(column),
            Syntax::Sql | Syntax::Mongo => false,
        }
    }

    /// What a header click adds to a statement, as a notice names it.
    pub fn sort_clause(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "an ORDER BY",
            Syntax::Mongo | Syntax::Redis => "a sort",
        }
    }

    pub fn qualified(self, schema: &str, name: &str) -> String {
        match self.syntax() {
            Syntax::Sql => self.spec().qualified(schema, name),
            Syntax::Mongo | Syntax::Redis => format!(
                "{}.{}",
                self.quote_identifier(schema),
                self.quote_identifier(name)
            ),
        }
    }
}

/// A JavaScript string literal, which a JSON string always is.
fn json_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// A URL is percent-encoded by definition, and a path with a space in it is
/// ordinary on macOS.
pub(super) fn percent_decoded(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = value
                .get(index + 1..index + 3)
                .ok_or_else(|| "Connection URL ends in an incomplete escape.".to_string())?;
            decoded
                .push(u8::from_str_radix(digits, 16).map_err(|_| {
                    format!("Connection URL contains an invalid escape %{digits}.")
                })?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).map_err(|_| "Connection URL path is not valid UTF-8.".to_string())
}

/// A server engine's URL, read by dbdelve rather than by any driver: the
/// userinfo, host, port and database, plus the two TLS keys dbdelve owns.
pub(super) fn server_from_url(url: &str, engine: &str) -> Result<ServerConfig, String> {
    let parsed =
        url::Url::parse(url).map_err(|error| format!("Connection URL is invalid: {error}"))?;

    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "Connection URL does not contain a host.".to_string())?
        .to_string();
    let database = parsed.path().trim_start_matches('/').to_string();
    let user = percent_decoded(parsed.username())?;
    if user.is_empty() {
        return Err("Connection URL does not contain a username.".into());
    }

    let mut sslmode = SslMode::default();
    let mut root_certificate = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "sslmode" => sslmode = SslMode::parse(value.as_ref())?,
            "sslrootcert" => {
                root_certificate = Some(value.trim().to_string()).filter(|path| !path.is_empty());
            }
            // Refused rather than dropped. The driver's options are built from
            // fields here, so a parameter dbdelve does not carry has nowhere to
            // go, and silently ignoring one is how a connection ends up not
            // being the connection that was asked for.
            other => {
                return Err(format!(
                    "Connection URL parameter {other} is not one dbdelve can pass to {engine}."
                ));
            }
        }
    }

    Ok(ServerConfig {
        host,
        port: parsed.port(),
        database: percent_decoded(&database)?,
        user,
        password: parsed
            .password()
            .map(percent_decoded)
            .transpose()?
            .unwrap_or_default(),
        password_command: None,
        sslmode,
        root_certificate,
        // A URL has nowhere to say it; the form is where it is set.
        statement_timeout: 0,
        // Same reason: a URL has nowhere to say it either.
        ssm: None,
        ssh: None,
        schema: None,
        time_zone: None,
        properties: Properties::default(),
    })
}

/// A local port forward opened over the system `ssh` binary before the engine
/// dials in. The one secret is the key's passphrase, which the Keychain holds,
/// never `profiles.toml`; the private key stays on disk or with ssh-agent.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnel {
    /// A hostname, or an alias `~/.ssh/config` resolves.
    pub host: String,
    /// None defers to `~/.ssh/config`, or 22.
    #[serde(default)]
    pub port: Option<u16>,
    /// Blank defers to `~/.ssh/config`, or the local user.
    #[serde(default)]
    pub user: String,
    /// None defers to `~/.ssh/config`, or ssh-agent. No password field: a
    /// host is reached by key, never by password.
    #[serde(default)]
    pub identity_file: Option<String>,
    /// The OS user `sudo` opens a Unix socket as on the SSH host, for peer
    /// authentication. None, or blank, opens it as the SSH user.
    #[serde(default)]
    pub run_as: Option<String>,
    /// What decrypts the key, filled in from the Keychain at connect as the
    /// password is. Blank is none, and ssh runs exactly as it does without one.
    #[serde(skip)]
    pub passphrase: String,
}

/// By hand, so the passphrase reaches no log line or panic message.
impl std::fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnel")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("identity_file", &self.identity_file)
            .field("run_as", &self.run_as)
            .finish_non_exhaustive()
    }
}

impl SshTunnel {
    /// Absolute or under `~/`, which ssh expands itself: a relative path
    /// resolves against wherever the app was launched from, `/` for one
    /// opened from Finder.
    pub fn identity_file_error(path: &str) -> Option<String> {
        (!path.starts_with("~/") && !std::path::Path::new(path).is_absolute())
            .then(|| "Identity file must be an absolute path to the key file.".to_owned())
    }
}

/// What an engine needs to reach a server. SQLite has none of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// Blank is valid and must never be warned about — cloud IAM auth issues a
    /// short-lived token as the password, or none at all.
    pub password: String,
    /// Run for the password in its place at every login, where the target
    /// [`Target::takes_password_command`].
    pub password_command: Option<PasswordCommand>,
    pub sslmode: SslMode,
    /// libpq's `sslrootcert`. Replaces the platform's trust store rather than
    /// adding to it, and only consulted by the two verifying modes.
    pub root_certificate: Option<String>,
    /// How long a statement may run, in seconds, or 0 for no limit.
    ///
    /// A number here and nothing else: how it is expressed is a question each
    /// engine module answers for itself (AGENTS.md, hard rule 4). What the
    /// answers cost is worth knowing, because they are not the same bargain:
    ///
    /// - Postgres sets `statement_timeout`, which bounds any statement.
    /// - MySQL sets `max_execution_time`, which bounds **read-only `SELECT`s
    ///   only** -- a runaway `UPDATE` or `ALTER` runs to completion and Cancel
    ///   is the only recourse against it. The server has also only had the
    ///   variable since 5.7.8, so asking for a timeout on an older one fails the
    ///   connect rather than the statement.
    /// - MariaDB sets `max_statement_time`, in seconds, which bounds any
    ///   statement, as Postgres's does. Before 10.1 the variable does not
    ///   exist, so asking for a timeout there fails the connect as it does on
    ///   an old MySQL.
    /// - SQLite has no such setting and gets a wall-clock timer firing
    ///   [`Connection::cancel`]'s interrupt instead, so it counts time a
    ///   statement spent blocked on a lock as readily as time it spent scanning.
    ///
    /// Applied once at connect, as a session default, never spliced into the
    /// user's submission -- rewriting what they typed is hard rule 1, and on
    /// Postgres a `SET` inside their submission would be scoped to the implicit
    /// transaction around it and change what their own `BEGIN` means. The other
    /// side of that: a user who runs their own `SET statement_timeout = 0`
    /// silently wins for the rest of the session, which is correct.
    pub statement_timeout: u32,
    /// A local port forward to dial through instead of connecting to `host`
    /// directly. None is the ordinary direct connection.
    pub ssh: Option<SshTunnel>,
    pub ssm: Option<SsmTunnel>,
    /// The schema unqualified names resolve in first, where
    /// [`Target::picks_schema`]. None leaves the server's
    /// `search_path` alone.
    pub schema: Option<String>,
    /// The session time zone, where [`Target::takes_time_zone`]. Applied by
    /// each engine module as a session default, never spliced into the user's
    /// submission, and None sends nothing, so the server's own zone stands.
    pub time_zone: Option<String>,
    /// The driver settings the profile set, each mapped by the engine module
    /// onto the one driver field it names. Config, so every connection opened
    /// from it carries them.
    pub properties: Properties,
}

/// The longest a session time zone may be.
const TIME_ZONE_CAP: usize = 64;

/// Whether `zone` may be sent as a session time zone: a name (`Europe/Paris`,
/// `Etc/GMT+5`, `UTC`) or an offset (`+05:30`), which is letters, digits and
/// `_ / + - :`. Nothing else is admitted, so no quote or backslash can end the
/// literal it is written into.
pub fn check_time_zone(zone: &str) -> Result<(), String> {
    let zone = zone.trim();
    if zone.is_empty() {
        return Err("The time zone is blank.".to_string());
    }
    if zone.chars().count() > TIME_ZONE_CAP {
        return Err(format!(
            "The time zone is longer than {TIME_ZONE_CAP} characters."
        ));
    }
    if let Some(bad) = zone
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '+' | '-' | ':')))
    {
        return Err(format!(
            "The time zone `{zone}` holds `{bad}`, which a time zone name does not."
        ));
    }
    Ok(())
}

impl ServerConfig {
    pub fn has_tunnel(&self) -> bool {
        self.ssh.is_some() || self.ssm.is_some()
    }

    pub fn endpoint(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Where a profile connects.
///
/// An enum rather than one struct carrying an engine tag: SQLite has no host,
/// no port, no user, no password and no TLS. Six permanently-empty fields would
/// be six dead inputs on the form, six dead keys in `profiles.toml`, and a
/// blank host that every layer below has to keep deciding is fine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionConfig {
    Postgres(PostgresConfig),
    MySql(ServerConfig),
    MariaDb(ServerConfig),
    SqlServer(ServerConfig),
    Sqlite {
        path: String,
        statement_timeout: u32,
    },
    Turso {
        path: String,
        statement_timeout: u32,
    },
    /// A file, or [`DUCKDB_IN_MEMORY`] for none. `database` is the attached
    /// database picked to work in, blank for the one the path opens, and
    /// `schema` the schema picked in it.
    DuckDb {
        path: String,
        statement_timeout: u32,
        database: String,
        schema: Option<String>,
    },
    Snowflake(SnowflakeConfig),
    ClickHouse(ServerConfig),
    MongoDb(MongoConfig),
    /// `database` is the database's number, blank for 0.
    Redis(ServerConfig),
    /// Through a plugin driver. Its host, port and database are inside its
    /// connection string, so it has no [`ServerConfig`].
    External(ExternalConfig),
}

impl ConnectionConfig {
    /// A Postgres profile that names no connection service.
    pub fn postgres(server: ServerConfig) -> Self {
        Self::Postgres(PostgresConfig {
            server,
            service: None,
        })
    }

    /// A DuckDB profile on the database `path` opens, with nothing picked.
    pub fn duckdb(path: String, statement_timeout: u32) -> Self {
        Self::DuckDb {
            path,
            statement_timeout,
            database: String::new(),
            schema: None,
        }
    }

    pub fn engine(&self) -> Engine {
        match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(_) => Engine::MySql,
            Self::MariaDb(_) => Engine::MariaDb,
            Self::SqlServer(_) => Engine::SqlServer,
            Self::Sqlite { .. } => Engine::Sqlite,
            Self::Turso { .. } => Engine::Turso,
            Self::DuckDb { .. } => Engine::DuckDb,
            Self::Snowflake(_) => Engine::Snowflake,
            Self::ClickHouse(_) => Engine::ClickHouse,
            Self::MongoDb(_) => Engine::MongoDb,
            Self::Redis(_) => Engine::Redis,
            Self::External(external) => external.speaks,
        }
    }

    pub fn target(&self) -> Target {
        match self {
            Self::External(external) => Target {
                engine: external.speaks,
                driver: Driver::Plugin(external.plugin),
            },
            _ => Target::built_in(self.engine()),
        }
    }

    /// The server half, for the callers that only have something to say when
    /// there is one — the credential fields, and the Keychain.
    pub fn server(&self) -> Option<&ServerConfig> {
        match self {
            Self::Postgres(PostgresConfig { server, .. })
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server)
            | Self::Redis(server) => Some(server),
            Self::MongoDb(mongo) => Some(&mongo.server),
            Self::Sqlite { .. }
            | Self::Turso { .. }
            | Self::DuckDb { .. }
            | Self::Snowflake(_)
            | Self::External(_) => None,
        }
    }

    /// The same, for the one caller that fills the password in: connecting
    /// reads it from the Keychain, which the profile on disk never holds.
    pub fn server_mut(&mut self) -> Option<&mut ServerConfig> {
        match self {
            Self::Postgres(PostgresConfig { server, .. })
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server)
            | Self::Redis(server) => Some(server),
            Self::MongoDb(mongo) => Some(&mut mongo.server),
            Self::Sqlite { .. }
            | Self::Turso { .. }
            | Self::DuckDb { .. }
            | Self::Snowflake(_)
            | Self::External(_) => None,
        }
    }

    /// The tunnel the profile connects through, where it has one.
    pub fn ssh(&self) -> Option<&SshTunnel> {
        match self {
            Self::External(external) => external.ssh.as_ref(),
            _ => self.server()?.ssh.as_ref(),
        }
    }

    pub fn ssm(&self) -> Option<&SsmTunnel> {
        match self {
            Self::External(c) => c.ssm.as_ref(),
            _ => self.server()?.ssm.as_ref(),
        }
    }

    pub fn set_connect_attempt(&mut self, attempt: ConnectAttempt) {
        let ssm = match self {
            Self::External(c) => c.ssm.as_mut(),
            _ => self.server_mut().and_then(|s| s.ssm.as_mut()),
        };
        if let Some(ssm) = ssm {
            ssm.attempt = Some(attempt);
        }
    }

    /// The same, for the callers that fill its passphrase in from the Keychain
    /// or take it out to save there.
    pub fn ssh_mut(&mut self) -> Option<&mut SshTunnel> {
        match self {
            Self::External(external) => external.ssh.as_mut(),
            _ => self.server_mut()?.ssh.as_mut(),
        }
    }

    /// The command a login runs in place of the Keychain's password.
    pub fn password_command(&self) -> Option<&PasswordCommand> {
        self.server()?.password_command.as_ref()
    }

    /// The password the Keychain holds for this profile, where it has one.
    pub fn password(&self) -> Option<&str> {
        match self {
            Self::External(external) => Some(&external.password),
            _ => self.server().map(|server| server.password.as_str()),
        }
    }

    /// The same, for the callers that fill it in from the Keychain or take it
    /// out to save there.
    pub fn password_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::External(external) => Some(&mut external.password),
            _ => self.server_mut().map(|server| &mut server.password),
        }
    }

    /// Move the profile onto another database on the same server.
    pub fn set_database(&mut self, name: String) {
        match self {
            // Schemas belong to one database, so the active one doesn't carry over.
            Self::Postgres(postgres) => {
                postgres.server.database = name;
                postgres.server.schema = None;
            }
            Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server) => server.database = name,
            Self::MongoDb(mongo) => mongo.set_database(name),
            Self::Snowflake(account) => account.set_database(name),
            // Listed as `db3`, stored as the number a URL's path holds.
            Self::Redis(server) => {
                server.database = name.strip_prefix("db").unwrap_or(&name).to_string();
            }
            Self::DuckDb {
                database, schema, ..
            } => {
                *database = name;
                *schema = None;
            }
            Self::Sqlite { .. } | Self::Turso { .. } | Self::External(_) => {}
        }
    }

    /// The connection service file section the profile names.
    pub fn service(&self) -> Option<&str> {
        match self {
            Self::Postgres(postgres) => postgres.service.as_deref(),
            _ => None,
        }
    }

    /// The database as the profile holds it, which may be blank.
    pub fn database(&self) -> Option<&str> {
        match self {
            Self::Snowflake(account) => Some(&account.database),
            Self::DuckDb { database, .. } => Some(database),
            _ => self.server().map(|server| server.database.as_str()),
        }
    }

    /// The database the profile is on, spelled as [`Connection::databases`]
    /// lists it. None where a blank one leaves the server to pick.
    pub fn database_name(&self) -> Option<String> {
        let database = self.database()?;
        match self {
            Self::Snowflake(account) => Some(account.stored_database()),
            Self::Postgres(postgres) if database.is_empty() && postgres.service.is_none() => {
                Some("postgres".into())
            }
            Self::Redis(_) => Some(format!(
                "db{}",
                if database.is_empty() { "0" } else { database }
            )),
            _ => Some(database.to_string()).filter(|name| !name.is_empty()),
        }
    }

    /// The schema picked as the active one, where [`Target::picks_schema`].
    /// None until one is picked.
    pub fn schema(&self) -> Option<&str> {
        match self {
            Self::Postgres(postgres) => postgres.server.schema.as_deref(),
            Self::Snowflake(account) => account.schema.as_deref(),
            Self::DuckDb { schema, .. } => schema.as_deref(),
            _ => None,
        }
    }

    pub fn set_schema(&mut self, schema: Option<String>) {
        match self {
            Self::Postgres(postgres) => postgres.server.schema = schema,
            Self::Snowflake(account) => account.schema = schema,
            Self::DuckDb { schema: picked, .. } => *picked = schema,
            _ => {}
        }
    }

    /// The scheme picks the engine, and the engine parses the rest. dbdelve never
    /// guesses from the shape of a URL: a host-looking string is a host to
    /// three different drivers.
    pub fn from_url(url: &str) -> Result<Self, String> {
        let scheme = url
            .split_once("://")
            .or_else(|| url.split_once(':'))
            .map(|(scheme, _)| scheme)
            .filter(|scheme| !scheme.is_empty())
            .ok_or_else(|| {
                "Connection URL must start with a scheme, such as postgresql:// or sqlite://."
                    .to_string()
            })?;

        match Engine::parse(scheme).map_err(|_| {
            format!("Connection URL scheme {scheme}:// is not a database dbdelve speaks.")
        })? {
            Engine::Postgres => postgres::config_from_url(url).map(Self::Postgres),
            Engine::MySql => mysql::config_from_url(url).map(Self::MySql),
            Engine::MariaDb => server_from_url(url, "MariaDB").map(Self::MariaDb),
            Engine::SqlServer => mssql::config_from_url(url).map(Self::SqlServer),
            Engine::MongoDb => mongo::config_from_url(url).map(Self::MongoDb),
            Engine::Redis => redis::config_from_url(url).map(Self::Redis),
            Engine::ClickHouse => server_from_url(url, "ClickHouse").map(Self::ClickHouse),
            Engine::Sqlite => sqlite::path_from_url(url).map(|path| Self::Sqlite {
                path,
                // A URL has nowhere to say it; the form is where it is set.
                statement_timeout: 0,
            }),
            Engine::Turso => sqlite::path_from_url(url).map(|path| Self::Turso {
                path,
                statement_timeout: 0,
            }),
            Engine::DuckDb => duckdb::path_from_url(url).map(|path| Self::duckdb(path, 0)),
            // Nobody pastes a Snowflake URL, because there is no such form.
            Engine::Snowflake => {
                Err("Snowflake has no connection URL. Fill the fields in instead.".to_string())
            }
            Engine::Generic => Err(GENERIC_REFUSAL.to_string()),
        }
    }

    /// The statement timeout in seconds, or 0 for none. One accessor rather
    /// than a match at every call site, since no caller above `src/db/` cares
    /// which variant is carrying it.
    pub fn statement_timeout(&self) -> u32 {
        match self {
            Self::Postgres(PostgresConfig { server, .. })
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server)
            | Self::Redis(server) => server.statement_timeout,
            Self::MongoDb(mongo) => mongo.server.statement_timeout,
            Self::Sqlite {
                statement_timeout, ..
            }
            | Self::Turso {
                statement_timeout, ..
            }
            | Self::DuckDb {
                statement_timeout, ..
            } => *statement_timeout,
            Self::Snowflake(account) => account.statement_timeout,
            Self::External(external) => external.statement_timeout,
        }
    }

    /// Whether replacing `self` with `edited` has to be reconnected for.
    ///
    /// A blank password in `edited` is not a change: the edit form never shows
    /// what the Keychain holds, so blank there means "leave it alone" -- taken
    /// literally it would drop and reopen the connection every time a colour
    /// was saved. A password that was typed does count, since applying it is
    /// the only reason to type one. The same goes for the key passphrase.
    pub fn needs_reconnect(&self, edited: &Self) -> bool {
        let mut current = self.clone();
        if let Some(password) = current.password_mut()
            && edited.password().is_some_and(str::is_empty)
        {
            password.clear();
        }
        if let Some(ssh) = current.ssh_mut()
            && edited.ssh().is_some_and(|ssh| ssh.passphrase.is_empty())
        {
            ssh.passphrase.clear();
        }
        current != *edited
    }

    /// Whether `edited` reaches another server, or another database on it,
    /// than `self` does: what rows read under `self`, and edits staged on
    /// them, could name something else under. Only what picks the target
    /// counts, a tunnel's way in included, since the host it forwards to is
    /// read from where it lands: a password, a timeout or TLS is the same
    /// target reached another way. The user counts, since a blank database
    /// is the login's default on SQL Server and Postgres, and an unqualified
    /// name resolves in the user's own default schema. An edit that may move
    /// it, a service or a plugin's connection string, counts as moving it.
    pub fn retargets(&self, edited: &Self) -> bool {
        std::mem::discriminant(self) != std::mem::discriminant(edited)
            || self.engine() != edited.engine()
            || self.reaches() != edited.reaches()
    }

    /// What [`Self::retargets`] compares, one string per part of the target.
    fn reaches(&self) -> Vec<String> {
        let tunnel = [
            self.ssh()
                .map(|ssh| format!("ssh {}:{:?}", ssh.host, ssh.port)),
            self.ssm()
                .map(|ssm| format!("ssm {} {:?} {:?}", ssm.target, ssm.region, ssm.profile)),
        ];
        let place: Vec<String> = match self {
            Self::Postgres(PostgresConfig { server, service }) => vec![
                server.endpoint(),
                server.database.clone(),
                server.user.clone(),
                format!("{service:?}"),
            ],
            Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server)
            | Self::Redis(server) => vec![
                server.endpoint(),
                server.database.clone(),
                server.user.clone(),
            ],
            Self::MongoDb(mongo) => vec![
                mongo.server.endpoint(),
                mongo.server.database.clone(),
                mongo.server.user.clone(),
                mongo.srv.to_string(),
                mongo::target_options(&mongo.options),
            ],
            Self::Sqlite { path, .. } | Self::Turso { path, .. } => vec![path.clone()],
            Self::DuckDb { path, database, .. } => vec![path.clone(), database.clone()],
            Self::Snowflake(account) => vec![
                account.account.clone(),
                format!("{:?}", account.host),
                account.database.clone(),
                account.user.clone(),
            ],
            Self::External(external) => {
                vec![external.plugin.id.to_string(), external.connection.clone()]
            }
        };
        place
            .into_iter()
            .chain(tunnel.into_iter().flatten())
            .collect()
    }

    /// Whether the database lives only as long as the connection holds it, as
    /// a DuckDB in memory does: closing the connection for being idle would
    /// throw away every table in it.
    pub fn lives_with_its_connection(&self) -> bool {
        matches!(self, Self::DuckDb { path, .. } if duckdb::is_in_memory(path))
    }

    /// What was being talked to, for an error or a title to name.
    pub fn endpoint(&self) -> String {
        match self {
            Self::Postgres(postgres) => postgres.endpoint(),
            Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server)
            | Self::ClickHouse(server)
            | Self::Redis(server) => server.endpoint(),
            Self::MongoDb(mongo) => mongo.endpoint(),
            Self::Sqlite { path, .. } | Self::Turso { path, .. } => path.clone(),
            Self::DuckDb { path, .. } if path == DUCKDB_IN_MEMORY => "in memory".to_string(),
            Self::DuckDb { path, .. } => path.clone(),
            Self::Snowflake(account) => account.host(),
            // The string can carry a password of its own, so only a DSN,
            // which is a name and nothing else, is shown.
            Self::External(external) if !external.connection.contains('=') => {
                external.connection.trim().to_string()
            }
            Self::External(_) => "an ODBC connection string".to_string(),
        }
    }

    /// Where a connection points, and for one through a plugin what carries it,
    /// which its logo, one plug for every plugin, cannot say.
    pub fn listed_endpoint(&self) -> String {
        let target = self.target();
        match (target.driver, self) {
            (Driver::BuiltIn, _) => self.endpoint(),
            (Driver::Plugin(_), Self::External(external)) if !external.connection.contains('=') => {
                format!("{} · {}", self.endpoint(), target.label())
            }
            (Driver::Plugin(_), _) => target.label(),
        }
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
#[derive(Clone)]
pub enum Connection {
    Postgres(postgres::Connection),
    /// MariaDB's too: one driver, which holds the engine it was opened for.
    MySql(mysql::Connection),
    SqlServer(mssql::Connection),
    Sqlite(sqlite::Connection),
    Turso(turso::Connection),
    DuckDb(duckdb::Connection),
    Snowflake(snowflake::Connection),
    ClickHouse(clickhouse::Connection),
    MongoDb(mongo::Connection),
    Redis(redis::Connection),
    External(external::Connection),
}

impl Connection {
    pub fn open(config: ConnectionConfig) -> Result<Self, DbError> {
        tunnel::selected(config.ssh(), config.ssm())?;
        match config {
            ConnectionConfig::Postgres(postgres) => postgres
                .resolved()
                .and_then(|server| postgres::Connection::open(&server))
                .map(Self::Postgres),
            ConnectionConfig::MySql(server) => {
                mysql::Connection::open(&server, Engine::MySql).map(Self::MySql)
            }
            ConnectionConfig::MariaDb(server) => {
                mysql::Connection::open(&server, Engine::MariaDb).map(Self::MySql)
            }
            ConnectionConfig::SqlServer(server) => {
                mssql::Connection::open(&server).map(Self::SqlServer)
            }
            ConnectionConfig::Sqlite {
                path,
                statement_timeout,
            } => sqlite::Connection::open(&path, statement_timeout).map(Self::Sqlite),
            ConnectionConfig::Turso {
                path,
                statement_timeout,
            } => turso::Connection::open(&path, statement_timeout).map(Self::Turso),
            ConnectionConfig::DuckDb {
                path,
                statement_timeout,
                database,
                schema,
            } => duckdb::Connection::open(&path, statement_timeout).map(|connection| {
                Self::DuckDb(
                    connection.opened_on(Some(database).filter(|name| !name.is_empty()), schema),
                )
            }),
            ConnectionConfig::Snowflake(account) => {
                snowflake::Connection::open(&account).map(Self::Snowflake)
            }
            ConnectionConfig::ClickHouse(server) => {
                clickhouse::Connection::open(&server).map(Self::ClickHouse)
            }
            ConnectionConfig::MongoDb(mongo) => mongo::Connection::open(&mongo).map(Self::MongoDb),
            ConnectionConfig::Redis(server) => redis::Connection::open(&server).map(Self::Redis),
            ConnectionConfig::External(external) => {
                external::Connection::open(&external).map(Self::External)
            }
        }
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. A
    /// limit bounds the rows kept, not the statement: past it they are read
    /// and dropped, and [`QueryResult::capped_from`] says how many there were,
    /// unless the statement could be stopped there instead
    /// ([`QueryResult::stopped`]). A feed takes the rows as they arrive
    /// instead of the result, which then holds none of them, and values
    /// still waiting on their types are the caller's to [`render`]. Postgres,
    /// MySQL, SQL Server, SQLite, Snowflake and MongoDB honour [`Fetch`]: SQL
    /// Server by `SET ROWCOUNT` around a lone read, which the server stops
    /// itself, and by draining anything else, Snowflake by not downloading the partitions
    /// past the limit, whose rows the server has already counted, and MongoDB
    /// without a feed.
    pub fn query(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        match self {
            Self::Postgres(connection) => connection.query(sql, fetch),
            Self::MySql(connection) => connection.query(sql, fetch),
            Self::SqlServer(connection) => connection.query(sql, fetch),
            Self::Sqlite(connection) => connection.query(sql, fetch),
            Self::Turso(connection) => connection.query(sql, fetch),
            Self::DuckDb(connection) => connection.query(sql, fetch),
            Self::Snowflake(connection) => connection.query_with(sql, cancel, fetch),
            Self::ClickHouse(connection) => connection.query(sql, cancel, fetch),
            Self::MongoDb(connection) => connection.query(sql, cancel, fetch),
            Self::Redis(connection) => connection.query(sql, fetch),
            Self::External(connection) => connection.query(sql, cancel, fetch),
        }
    }

    /// [`Connection::query`] on a session opened for a database
    /// ([`Connection::on_database`]), for a tab bound to it: the session is put
    /// back on the database it was opened for, and on `schema` where the
    /// engine picks one, in the same acquisition of its lock as the statement.
    /// Two tabs bound to different schemas of one database share a session,
    /// and apply and run as two acquisitions would let one run in the other's
    /// schema. The statement itself is never touched; the binding is applied
    /// through the driver's own calls and no text of the user's carries it.
    ///
    /// Postgres, DuckDB, MySQL, MariaDB, Redis and ClickHouse apply it;
    /// Snowflake sends the schema with the request. SQL Server and MongoDB run
    /// the statement as [`Connection::query`] does: the first moves a session
    /// a `USE` moved back before it returns (`held_to_database`), the second's
    /// database is fixed for the session.
    ///
    /// `cancel` is made by [`CancelToken::alongside`]: a Cancel on it stops
    /// this session only while this statement is what it is running, and a
    /// statement still waiting behind another tab's is dropped unsent instead
    /// of stopping that one.
    pub fn query_bound(
        &self,
        schema: Option<&str>,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        running_on(cancel, self, |held, let_go| {
            let fetch = Fetch {
                held: Some(held),
                let_go: Some(let_go),
                ..fetch
            };
            match self {
                Self::Postgres(connection) => connection.query_bound(schema, sql, fetch),
                Self::MySql(connection) => connection.query_bound(sql, fetch),
                Self::DuckDb(connection) => connection.query_bound(schema, sql, fetch),
                Self::Redis(connection) => connection.query_bound(sql, fetch),
                Self::Snowflake(connection) => connection.query_bound(schema, sql, cancel, fetch),
                Self::ClickHouse(connection) => connection.query_bound(sql, cancel, fetch),
                _ => self.query(sql, cancel, fetch),
            }
        })
    }

    /// [`Connection::query`] registered in `cancel` as [`Self::query_bound`]
    /// is, with nothing put back first: for a statement that ends a bound
    /// tab's transaction, which a session inside one could not be moved for.
    /// Without the registration a Cancel while it waits for the session would
    /// stop nothing.
    pub fn query_unplaced(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        running_on(cancel, self, |held, let_go| {
            self.query(
                sql,
                cancel,
                Fetch {
                    held: Some(held),
                    let_go: Some(let_go),
                    ..fetch
                },
            )
        })
    }

    /// [`Connection::generated`] for a tab bound to this session's database:
    /// where that is [`Connection::query`] (MySQL, MariaDB and DuckDB) it is
    /// [`Connection::query_bound`], and Redis's is put back on its database
    /// too. For every other engine nothing needs putting back: Postgres's
    /// session never leaves its database and the statement names its schema, a
    /// ClickHouse statement of dbdelve's goes out without the session and
    /// names the database, a Snowflake request carries its database and is
    /// sent no schema, a MongoDB statement names the database it reads, and
    /// SQL Server moves a session a `USE` moved back before any run returns.
    /// An edit applied from a bound tab's grid writes to the database the rows
    /// came from, whichever way the session has been moved since.
    pub fn generated_bound(
        &self,
        schema: Option<&str>,
        sql: &str,
        cancel: &CancelToken,
        feed: Option<&Feed>,
        limit: Option<usize>,
    ) -> Result<QueryResult, DbError> {
        match self {
            // A Postgres session cannot leave its database, and what dbdelve
            // writes names its schema, so nothing is put back: setting the
            // search path again is refused inside a transaction another tab
            // of the database opened, which would stop a table tab's preview.
            Self::Postgres(connection) => running_on(cancel, self, |held, let_go| {
                connection.query(
                    sql,
                    Fetch {
                        feed,
                        limit,
                        held: Some(held),
                        let_go: Some(let_go),
                        ..Fetch::default()
                    },
                )
            }),
            Self::MySql(_) | Self::DuckDb(_) => self.query_bound(
                schema,
                sql,
                cancel,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            // These two registered in the token as `query_bound` does, or a
            // Cancel on a bound run finds nothing to stop.
            Self::Redis(connection) => running_on(cancel, self, |held, let_go| {
                connection.generated_bound(
                    sql,
                    Fetch {
                        feed,
                        limit,
                        held: Some(held),
                        let_go: Some(let_go),
                        ..Fetch::default()
                    },
                )
            }),
            Self::SqlServer(connection) => running_on(cancel, self, |held, let_go| {
                connection.generated(
                    sql,
                    Fetch {
                        feed,
                        limit,
                        held: Some(held),
                        let_go: Some(let_go),
                        ..Fetch::default()
                    },
                )
            }),
            _ => self.generated(sql, cancel, feed, limit),
        }
    }

    /// Applies `steps` as one verified sequence held on this session: each
    /// `UPDATE`'s and `DELETE`'s row read again and compared with the row as it
    /// was fetched before anything is written, each write counted, then
    /// `COMMIT` (see [`run_verified`]). Registered in `cancel`, so a Cancel
    /// finds the session and a Cancel between statements rolls the sequence
    /// back.
    ///
    /// An explicit arm per connection, no wildcard: one that does not answer
    /// [`Engine::verifies_applies`] answers [`ApplyOutcome::Failed`] with
    /// nothing sent, and it is `Target::verifies_applies` that keeps an edit
    /// from being routed here.
    pub(crate) fn apply_verified(
        &self,
        steps: &[VerifiedStep],
        cancel: &CancelToken,
    ) -> ApplyOutcome {
        self.apply_verified_on(steps, cancel, None)
    }

    /// [`Self::apply_verified`] for a tab bound to this session's database:
    /// where [`Self::generated_bound`] puts the session back first (MySQL,
    /// MariaDB and DuckDB), the apply does too, under the lock it holds for
    /// the whole sequence, so an edit writes to the database its rows came
    /// from whichever way a `USE` has moved the session since. Every other
    /// engine that verifies an apply needs nothing put back, for the reasons
    /// [`Self::generated_bound`] gives.
    pub(crate) fn apply_verified_bound(
        &self,
        schema: Option<&str>,
        steps: &[VerifiedStep],
        cancel: &CancelToken,
    ) -> ApplyOutcome {
        self.apply_verified_on(steps, cancel, Some(schema))
    }

    /// `bound` is the schema to put the session back on first, itself `None`
    /// for its own default, when the apply is a bound tab's.
    fn apply_verified_on(
        &self,
        steps: &[VerifiedStep],
        cancel: &CancelToken,
        bound: Option<Option<&str>>,
    ) -> ApplyOutcome {
        // ponytail: `running_on` lets go of the session only once the engine
        // has released its lock, so a Cancel in that gap reaches whatever took
        // the session next, as before `Fetch::let_go`. Closing it means handing
        // `let_go` to each engine's apply to call under its own lock.
        running_on(cancel, self, |held, _| {
            // The first ask registers the run and checks for a Cancel that came
            // first; every later one only checks.
            let registered = std::cell::Cell::new(false);
            let proceed = || match registered.replace(true) {
                false => held(),
                true => {
                    !cancel
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .asked
                }
            };
            match self {
                Self::Sqlite(connection) => connection.apply_verified(steps, &proceed),
                Self::DuckDb(connection) => connection.apply_verified(steps, &proceed, bound),
                Self::Postgres(connection) => connection.apply_verified(steps, &proceed),
                Self::MySql(connection) => {
                    connection.apply_verified(steps, &proceed, bound.is_some())
                }
                Self::SqlServer(connection) => connection.apply_verified(steps, &proceed),
                // Each statement registers its own run in `cancel`, as `query`
                // does, so this arm never asks `proceed`.
                Self::MongoDb(connection) => connection.apply_verified(steps, cancel),
                Self::Turso(_)
                | Self::Snowflake(_)
                | Self::ClickHouse(_)
                | Self::Redis(_)
                | Self::External(_) => unverifiable(),
            }
        })
    }

    /// [`Connection::query`] on a connection of its own, opened the way this
    /// one was (its tunnel and side session shared), so the statement runs
    /// beside whatever this one is running rather than queued behind it, and
    /// closed once it ends. Snowflake and MongoDB need no second one: they
    /// already run statements side by side, and stop them by `cancel`. Nor
    /// does a plugin's, which queues it on its one process rather than start
    /// another per statement.
    ///
    /// `cancel` is made by [`CancelToken::alongside`]. `read_only` puts the
    /// profile's Read-only hold on the new session as well.
    pub fn query_alongside(
        &self,
        sql: &str,
        cancel: &CancelToken,
        fetch: Fetch,
        read_only: bool,
    ) -> Result<QueryResult, DbError> {
        let opened = match self {
            Self::Snowflake(_) | Self::MongoDb(_) | Self::External(_) => {
                return self.query(sql, cancel, fetch);
            }
            // Never joined to the arm above: that would put every statement
            // of a queue run at once on the session's mutex, one after another.
            Self::ClickHouse(connection) => {
                return connection.query_alongside(sql, cancel, fetch, read_only);
            }
            Self::Postgres(connection) => connection.alongside().map(Self::Postgres),
            Self::MySql(connection) => connection.alongside().map(Self::MySql),
            Self::SqlServer(connection) => Ok(Self::SqlServer(connection.alongside())),
            Self::Sqlite(connection) => connection.alongside().map(Self::Sqlite),
            Self::Turso(connection) => connection.alongside().map(Self::Turso),
            Self::DuckDb(connection) => connection.alongside().map(Self::DuckDb),
            Self::Redis(connection) => connection.alongside().map(Self::Redis),
        };
        // A server out of connections still has this one, which the statement
        // waits its turn on rather than failing. SQL Server logs in on the
        // first run, so its refusal surfaces as the statement's error instead.
        let (connection, shared) = match opened {
            Ok(connection) => {
                if read_only {
                    connection.set_read_only(true)?;
                }
                (connection, false)
            }
            Err(_) => (self.clone(), true),
        };
        running_on(cancel, &connection, |hold, let_go| match shared {
            true => connection.query(
                sql,
                cancel,
                Fetch {
                    held: Some(hold),
                    let_go: Some(let_go),
                    ..fetch
                },
            ),
            false if hold() => connection.query(sql, cancel, fetch),
            false => Err(plain_error("Cancelled before it was sent.".into())),
        })
    }

    /// A session of its own for an import, opened the way this one was, so a
    /// transaction spanning its submissions holds nobody else's statements and
    /// a Cancel on it stops only the import's. Dropping it ends the session,
    /// and the server rolls back whatever it left open. Snowflake and MongoDB
    /// keep no session to hold apart, so it is this one.
    pub fn apart(&self) -> Result<Self, DbError> {
        match self {
            Self::Snowflake(_) | Self::MongoDb(_) => Ok(self.clone()),
            Self::ClickHouse(connection) => Ok(Self::ClickHouse(connection.apart())),
            Self::Postgres(connection) => connection.alongside().map(Self::Postgres),
            Self::MySql(connection) => connection.alongside().map(Self::MySql),
            Self::SqlServer(connection) => Ok(Self::SqlServer(connection.alongside())),
            Self::Sqlite(connection) => connection.alongside().map(Self::Sqlite),
            Self::Turso(connection) => connection.alongside().map(Self::Turso),
            Self::DuckDb(connection) => connection.alongside().map(Self::DuckDb),
            Self::Redis(connection) => connection.alongside().map(Self::Redis),
            Self::External(connection) => connection.apart().map(Self::External),
        }
    }

    /// [`Self::generated`] on a session of its own ([`Self::apart`]), for a
    /// statement that must not run inside a transaction open on this one: a
    /// role change, which is the server's and not the database's. It fails
    /// rather than fall back to this session. Run under a
    /// [`CancelToken::alongside`], a Cancel stops it there.
    pub fn generated_apart(&self, sql: &str, cancel: &CancelToken) -> Result<QueryResult, DbError> {
        let side = self.apart()?;
        running_on(cancel, &side, |hold, _| match hold() {
            true => side.generated(sql, cancel, None, None),
            false => Err(plain_error("Cancelled before it was sent.".into())),
        })
    }

    /// [`Self::apart`] for reading what a file will run elsewhere: on Postgres
    /// its `search_path` is empty, so the catalog's text names every schema
    /// ([`postgres::Connection::alongside_unpathed`]). Every other
    /// engine's DDL names its objects as written, whatever the session's
    /// schema.
    pub fn apart_unpathed(&self) -> Result<Self, DbError> {
        match self {
            Self::Postgres(connection) => connection.alongside_unpathed().map(Self::Postgres),
            _ => self.apart(),
        }
    }

    /// A session for the (connection, `database`) pair, which the connections
    /// tree reads that database's catalog over and a tab bound to it will run
    /// on. Built from this live connection -- its resolved password and its
    /// tunnel -- and never from the profile's config, and sharing no mutable
    /// state with it: moving the session's database moves nothing here. It
    /// carries no read-only hold of its own; the caller applies it before the
    /// session is stored (`set_read_only`).
    pub fn on_database(&self, database: &str) -> Result<Self, DbError> {
        match self {
            Self::DuckDb(connection) => connection.on_database(database).map(Self::DuckDb),
            Self::MySql(connection) => connection.on_database(database).map(Self::MySql),
            Self::Redis(connection) => connection.on_database(database).map(Self::Redis),
            Self::Snowflake(connection) => Ok(Self::Snowflake(connection.on_database(database))),
            Self::MongoDb(connection) => Ok(Self::MongoDb(connection.on_database(database))),
            Self::Postgres(connection) => connection.on_database(database).map(Self::Postgres),
            Self::SqlServer(connection) => connection.on_database(database).map(Self::SqlServer),
            Self::ClickHouse(connection) => connection.on_database(database).map(Self::ClickHouse),
            Self::Sqlite(_) | Self::Turso(_) | Self::External(_) => Err(plain_error(
                "This connection has no other database to open a session on.".into(),
            )),
        }
    }

    /// Runs the profile's gated startup `statements` now, on this session, and
    /// keeps them for every session the driver opens beside it. Only the live
    /// connection a user works on is handed them: a connection opened from a
    /// [`ConnectionConfig`] alone holds no script.
    ///
    /// An engine without a carrier is handed none; it refuses a non-empty list
    /// rather than drop it, so a script is never silently skipped.
    pub fn with_startup(self, statements: Vec<String>) -> Result<Self, DbError> {
        match self {
            Self::Sqlite(connection) => connection.with_startup(statements).map(Self::Sqlite),
            Self::Postgres(connection) => connection.with_startup(statements).map(Self::Postgres),
            Self::MySql(connection) => connection.with_startup(statements).map(Self::MySql),
            Self::SqlServer(connection) => connection.with_startup(statements).map(Self::SqlServer),
            Self::Turso(connection) => connection.with_startup(statements).map(Self::Turso),
            Self::DuckDb(connection) => connection.with_startup(statements).map(Self::DuckDb),
            other if statements.is_empty() => Ok(other),
            _ => Err(plain_error(
                "This connection runs no startup script.".into(),
            )),
        }
    }

    /// Open now what the first run would otherwise wait on opening.
    pub fn warm(&self) {
        if let Self::Postgres(connection) = self {
            connection.warm();
        }
    }

    /// Run a statement dbdelve wrote at the user's ask -- a relation tab's
    /// preview, or an edit -- verbatim, as [`Connection::query`] does. SQL
    /// Server runs it differently: its session options are the user's to
    /// `SET`, and dbdelve's SQL is written for particular ones. So does Redis,
    /// where a write that changed nothing, a `RENAMENX` onto a name that is
    /// taken, is an error rather than a `0`. And Snowflake, which sends it
    /// without the active schema, a field only the user's statements carry.
    /// And a plugin, which runs a batch of writes as one transaction of its
    /// driver's, each statement changing exactly one row or none saved.
    ///
    /// A `feed` takes its rows as they arrive, as for [`Connection::query`];
    /// the statement carries its own `LIMIT`, so nothing else of [`Fetch`]
    /// applies -- but `limit`, the row limit of a preview whose statement has
    /// nowhere to write one (see [`Engine::pages`]), and `None` everywhere
    /// else.
    pub fn generated(
        &self,
        sql: &str,
        cancel: &CancelToken,
        feed: Option<&Feed>,
        limit: Option<usize>,
    ) -> Result<QueryResult, DbError> {
        match self {
            Self::SqlServer(connection) => connection.generated(
                sql,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            Self::Redis(connection) => connection.generated(
                sql,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            Self::Snowflake(connection) => connection.generated(
                sql,
                cancel,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            Self::ClickHouse(connection) => connection.generated(
                sql,
                cancel,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            Self::External(connection) => connection.generated(
                sql,
                cancel,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
            Self::Postgres(_)
            | Self::MySql(_)
            | Self::Sqlite(_)
            | Self::Turso(_)
            | Self::DuckDb(_)
            | Self::MongoDb(_) => self.query(
                sql,
                cancel,
                Fetch {
                    feed,
                    limit,
                    ..Fetch::default()
                },
            ),
        }
    }

    /// The relations of every schema, and no routines.
    ///
    /// Split from [`Connection::routines`] because the two are separate
    /// queries on every engine and one of them is reliably the slower: on
    /// Snowflake `INFORMATION_SCHEMA.PROCEDURES` took eight seconds to report
    /// that there were none, with the tables already in hand after two. The
    /// explorer is worth more open and incomplete than closed and correct, so
    /// what arrives first is shown first.
    ///
    /// Less the schemas `shown` leaves out, dropped before anything stores the
    /// catalog.
    pub fn catalog(&self, shown: &Shown) -> Result<Catalog, DbError> {
        let catalog = match self {
            Self::Postgres(connection) => connection.catalog(),
            Self::MySql(connection) => connection.catalog(shown),
            Self::SqlServer(connection) => connection.catalog(),
            Self::Sqlite(connection) => connection.catalog(),
            Self::Turso(connection) => connection.catalog(),
            Self::DuckDb(connection) => connection.catalog(),
            Self::Snowflake(connection) => connection.catalog(),
            Self::ClickHouse(connection) => connection.catalog(shown),
            Self::MongoDb(connection) => connection.catalog(),
            Self::Redis(connection) => connection.catalog(),
            Self::External(connection) => connection.catalog(),
        }?;
        Ok(shown.narrow_catalog(catalog))
    }

    /// The stored functions and procedures, and the sequences of an engine
    /// that lists them, as a catalog holding nothing else, for
    /// [`Catalog::merge`] to fold into the one already on screen, less the
    /// schemas `shown` leaves out: one holding only a sequence would otherwise
    /// come back through the merge.
    pub fn routines(&self, shown: &Shown) -> Result<Catalog, DbError> {
        let routines = match self {
            Self::Postgres(connection) => {
                with_sequences(Engine::Postgres, connection.routines(), || {
                    connection.sequences()
                })
            }
            Self::MySql(connection) => {
                with_sequences(connection.engine(), connection.routines(shown), || {
                    connection.sequences(shown)
                })
            }
            Self::SqlServer(connection) => {
                with_sequences(Engine::SqlServer, connection.routines(), || {
                    connection.sequences()
                })
            }
            Self::Sqlite(connection) => connection.routines(),
            Self::Turso(connection) => connection.routines(),
            Self::DuckDb(connection) => {
                with_sequences(Engine::DuckDb, connection.routines(), || {
                    connection.sequences()
                })
            }
            Self::Snowflake(connection) => {
                with_sequences(Engine::Snowflake, connection.routines(), || {
                    connection.sequences()
                })
            }
            Self::ClickHouse(_) => Ok(Catalog::default()),
            Self::MongoDb(connection) => connection.routines(),
            Self::Redis(_) | Self::External(_) => Ok(Catalog::default()),
        }?;
        Ok(shown.narrow_routines(routines))
    }

    /// Each table's on-disk bytes and row estimate, for [`Catalog::set_sizes`]
    /// to write onto the relations already on screen.
    ///
    /// Apart from [`Connection::catalog`] because on Postgres and MySQL the
    /// numbers cost locks or opened tables, and both run it on a short-lived
    /// connection of their own rather than behind the one every query on the
    /// profile waits for. Engines whose catalog already carries sizes (SQL
    /// Server's catalog views, a column Snowflake returns anyway) return none
    /// here, as does SQLite, which keeps none.
    ///
    /// ponytail: Postgres still measures every relation of the database before
    /// the schemas `shown` leaves out are dropped. The ceiling is a very large
    /// database; the upgrade is the same per-engine push-down the catalog's.
    pub fn sizes(&self, shown: &Shown) -> Result<Sizes, DbError> {
        let sizes = match self {
            Self::Postgres(connection) => connection.sizes(),
            Self::MySql(connection) => connection.sizes(shown),
            Self::MongoDb(connection) => connection.sizes(),
            Self::SqlServer(_)
            | Self::Sqlite(_)
            | Self::Turso(_)
            | Self::DuckDb(_)
            | Self::Snowflake(_)
            | Self::ClickHouse(_)
            | Self::Redis(_)
            | Self::External(_) => Ok(Sizes::new()),
        }?;
        Ok(shown.narrow_sizes(sizes))
    }

    /// The foreign keys of other relations that point at this one, for the
    /// arrow that opens them. Snowflake declares its keys and enforces none of
    /// them, and its exported-keys listing is not one this has been checked
    /// against, so it answers nothing rather than a guess.
    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        match self {
            Self::Postgres(connection) => connection.references(schema, relation),
            Self::MySql(connection) => connection.references(schema, relation),
            Self::SqlServer(connection) => connection.references(schema, relation),
            Self::Sqlite(connection) => connection.references(schema, relation),
            Self::Turso(connection) => connection.references(schema, relation),
            Self::DuckDb(connection) => connection.references(schema, relation),
            Self::External(connection) => connection.references(schema, relation),
            // Nothing declares one document's field a reference to another's.
            Self::Snowflake(_) | Self::ClickHouse(_) | Self::MongoDb(_) | Self::Redis(_) => {
                Ok(Vec::new())
            }
        }
    }

    /// The databases on the server, for a profile to be moved onto one, less
    /// those `shown` leaves out. Empty where [`Target::switches_database`] is
    /// false.
    pub fn databases(&self, engine: Engine, shown: &Shown) -> Result<Databases, DbError> {
        let mut databases = match self {
            Self::Postgres(connection) => connection.databases(),
            Self::MySql(connection) => connection.databases(),
            Self::SqlServer(connection) => connection.databases(),
            Self::MongoDb(connection) => connection.databases(),
            Self::Redis(connection) => connection.databases(),
            Self::Snowflake(connection) => connection.databases(),
            Self::ClickHouse(connection) => connection.databases(),
            Self::DuckDb(connection) => connection.databases(),
            Self::Sqlite(_) | Self::Turso(_) | Self::External(_) => Ok(Databases::default()),
        }?;
        databases.system_last(engine);
        Ok(shown.narrow_databases(databases))
    }

    /// The databases the connections tree draws when nothing is filtered,
    /// which the filter picker lists and a filtered catalog counts against.
    /// Where the catalog lists databases as schemas, these are the ones
    /// holding something it reads, the server's own left out. Elsewhere they
    /// are every database [`Self::databases`] lists, unnarrowed.
    ///
    /// ponytail: where the catalog lists databases as schemas, an empty
    /// database is not listed, since the tree draws none, so the picker's total
    /// stays the connection row's and the database cannot be ticked before it
    /// holds something, though a name typed in the pattern field still covers
    /// it. The upgrade is drawing empty databases in the tree, which would
    /// change the unfiltered count.
    pub fn catalog_databases(&self, engine: Engine) -> Result<Databases, DbError> {
        match self {
            Self::MySql(connection) => connection.catalog_databases(),
            Self::ClickHouse(connection) => connection.catalog_databases(),
            Self::Postgres(_)
            | Self::SqlServer(_)
            | Self::Sqlite(_)
            | Self::Turso(_)
            | Self::DuckDb(_)
            | Self::Snowflake(_)
            | Self::MongoDb(_)
            | Self::Redis(_)
            | Self::External(_) => self.databases(engine, &Shown::everything()),
        }
    }

    /// Make `schema` the first place unqualified names resolve in every
    /// session this connection opens from here on. Never waits, so switches
    /// made one after another are recorded in that order; the sessions
    /// already open follow on [`Self::apply_schema`]. A no-op where
    /// [`Target::picks_schema`] is false.
    pub fn set_schema(&self, schema: Option<&str>) {
        match self {
            Self::Postgres(connection) => connection.set_schema(schema),
            Self::Snowflake(connection) => connection.set_schema(schema),
            Self::DuckDb(connection) => connection.set_schema(schema),
            _ => {}
        }
    }

    /// The same for the database, None for the one the connection opened,
    /// where [`Target::switches_database_in_session`]: the schema picked in
    /// the database left behind goes with it. A no-op elsewhere, where a
    /// database is moved onto by connecting again.
    pub fn set_database(&self, database: Option<&str>) {
        if let Self::DuckDb(connection) = self {
            connection.set_database(database);
        }
    }

    /// Move the sessions already open onto the database and schema last
    /// recorded by [`Self::set_database`] and [`Self::set_schema`], which may
    /// be newer than the ones recorded when this was called. Waits for a
    /// statement running on them to finish.
    pub fn apply_schema(&self) -> Result<(), DbError> {
        match self {
            Self::Postgres(connection) => connection.apply_schema(),
            Self::DuckDb(connection) => connection.apply_pick(),
            _ => Ok(()),
        }
    }

    /// The schema the server put the session in when it connected, before any
    /// picked one was applied. None where it isn't asked or there was none.
    pub fn current_schema(&self) -> Option<&str> {
        match self {
            Self::Postgres(connection) => connection.current_schema(),
            Self::DuckDb(connection) => Some(connection.current_schema()),
            Self::SqlServer(connection) => connection.current_schema(),
            _ => None,
        }
    }

    /// The database a connection opens on before any picked one, where the
    /// profile does not name it: the database a DuckDB file opens as. None
    /// where the profile's own config says.
    pub fn current_database(&self) -> Option<&str> {
        match self {
            Self::DuckDb(connection) => Some(connection.current_database()),
            _ => None,
        }
    }

    /// The database the connection is on as the server spells it, where
    /// that can differ from the profile's: SQL Server reads a name in any
    /// case, a login naming none lands where only the server can say, and a
    /// Postgres service can name one the profile leaves blank.
    pub fn spelled_database(&self) -> Option<&str> {
        match self {
            Self::SqlServer(connection) => Some(connection.current_database()),
            Self::Postgres(connection) => Some(connection.current_database()),
            _ => None,
        }
    }

    /// The picked database or schema this connection could not move onto
    /// when it opened, because it is no longer there.
    pub fn unpicked(&self) -> Option<&Unpicked> {
        match self {
            Self::DuckDb(connection) => connection.unpicked(),
            _ => None,
        }
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        match self {
            Self::Postgres(connection) => connection.structure(schema, relation),
            Self::MySql(connection) => connection.structure(schema, relation),
            Self::SqlServer(connection) => connection.structure(schema, relation),
            Self::Sqlite(connection) => connection.structure(schema, relation),
            Self::Turso(connection) => connection.structure(schema, relation),
            Self::DuckDb(connection) => connection.structure(schema, relation),
            Self::Snowflake(connection) => connection.structure(schema, relation),
            Self::ClickHouse(connection) => connection.structure(schema, relation),
            Self::MongoDb(connection) => connection.structure(schema, relation),
            Self::Redis(connection) => connection.structure(schema, relation),
            Self::External(connection) => connection.structure(schema, relation),
        }
    }

    /// One schema's tables and foreign keys in bulk, for the ER diagram: a
    /// few catalog statements for the whole schema rather than
    /// [`Connection::structure`]'s several per relation. Run it on a
    /// connection from [`Connection::apart`], so it never queues behind the
    /// profile's statements, and so a [`Connection::cancel`] on that
    /// connection stops nothing but this read.
    ///
    /// Asked only where [`Target::draws_diagrams`].
    pub fn schema_graph(&self, schema: &str) -> Result<SchemaGraph, DbError> {
        match self {
            Self::Postgres(connection) => connection.schema_graph(schema),
            Self::MySql(connection) => connection.schema_graph(schema),
            Self::SqlServer(connection) => connection.schema_graph(schema),
            Self::Sqlite(connection) => connection.schema_graph(schema),
            Self::Turso(connection) => connection.schema_graph(schema),
            Self::DuckDb(connection) => connection.schema_graph(schema),
            Self::Snowflake(connection) => connection.schema_graph(schema),
            Self::ClickHouse(_) | Self::MongoDb(_) | Self::Redis(_) => Err(plain_error(
                "This engine declares no foreign keys to draw.".into(),
            )),
            Self::External(connection) => connection.schema_graph(schema),
        }
    }

    /// The server's logins and roles and who belongs to whom. Asked only
    /// where [`Target::manages_roles`], on a connection from
    /// [`Connection::apart`].
    pub fn roles(&self) -> Result<roles::Roles, DbError> {
        match self {
            Self::Postgres(connection) => connection.roles(),
            _ => Err(plain_error("This connection lists no roles.".into())),
        }
    }

    /// Who holds which privilege on the relation or sequence `schema.name`.
    pub fn object_grants(&self, schema: &str, name: &str) -> Result<roles::ObjectGrants, DbError> {
        match self {
            Self::Postgres(connection) => connection.object_grants(schema, name),
            _ => Err(plain_error("This connection lists no grants.".into())),
        }
    }

    /// What the role `name` can reach in the connection's database.
    pub fn role_reach(&self, name: &str) -> Result<roles::Reach, DbError> {
        match self {
            Self::Postgres(connection) => connection.role_reach(name),
            _ => Err(plain_error("This connection lists no grants.".into())),
        }
    }

    /// Every privilege the role `name` holds on a relation of the
    /// connection's database.
    pub fn role_grants(&self, name: &str) -> Result<roles::RoleGrants, DbError> {
        match self {
            Self::Postgres(connection) => connection.role_grants(name),
            _ => Err(plain_error("This connection lists no grants.".into())),
        }
    }

    /// The audit lists of the Users and roles tab's Reports.
    pub fn role_reports(&self, roles: &roles::Roles) -> Result<roles::Reports, DbError> {
        match self {
            Self::Postgres(connection) => connection.role_reports(roles),
            _ => Err(plain_error("This connection lists no roles.".into())),
        }
    }

    /// Whether [`Self::schema_graph`] answers on this connection: what
    /// `target`, the profile's, says for a built-in driver, and for a plugin
    /// whether it offered `graph` when it opened, which `target` cannot know.
    pub fn draws_diagrams(&self, target: Target) -> bool {
        match self {
            Self::External(connection) => connection.draws_diagrams(),
            _ => target.draws_diagrams(),
        }
    }

    /// The statements that would create this relation, for the clipboard:
    /// nothing dbdelve does runs them. The server's own rendering wherever it
    /// has one; Postgres and SQL Server tables are written back out of their
    /// structure ([`create_table`]).
    pub fn ddl(&self, schema: &str, relation: &str, kind: RelationKind) -> Result<String, DbError> {
        match self {
            Self::Postgres(connection) => connection.ddl(schema, relation, kind),
            Self::MySql(connection) => connection.ddl(schema, relation),
            Self::SqlServer(connection) => connection.ddl(schema, relation, kind),
            Self::Sqlite(connection) => connection.ddl(schema, relation),
            Self::Turso(connection) => connection.ddl(schema, relation),
            Self::DuckDb(connection) => connection.ddl(schema, relation),
            Self::Snowflake(connection) => connection.ddl(schema, relation, kind),
            Self::ClickHouse(connection) => connection.ddl(schema, relation, kind),
            Self::MongoDb(connection) => connection.ddl(schema, relation),
            Self::Redis(connection) => connection.ddl(schema, relation),
            Self::External(_) => Err(plain_error(
                "Copy DDL is not offered through a plugin driver.".into(),
            )),
        }
    }

    /// The statement that would create the sequence `schema.name`, for
    /// reading: nothing dbdelve does runs it, and no current value is read.
    /// Apart from [`Self::ddl`], which takes a [`RelationKind`] a sequence is
    /// not.
    pub fn sequence_ddl(&self, schema: &str, name: &str) -> Result<String, DbError> {
        match self {
            Self::Postgres(connection) => connection.sequence_ddl(schema, name),
            Self::MySql(connection) => connection.sequence_ddl(schema, name),
            Self::SqlServer(connection) => connection.sequence_ddl(schema, name),
            Self::DuckDb(connection) => connection.sequence_ddl(schema, name),
            Self::Snowflake(connection) => connection.sequence_ddl(schema, name),
            Self::Sqlite(_)
            | Self::Turso(_)
            | Self::ClickHouse(_)
            | Self::MongoDb(_)
            | Self::Redis(_)
            | Self::External(_) => Err(plain_error(
                "This connection lists no sequences to show a definition of.".into(),
            )),
        }
    }

    /// A table's DDL for a schema export. An engine that adds its foreign keys
    /// after the rows gets them as statements of their own; every other
    /// engine's is [`Connection::ddl`] whole, its keys declared with the table.
    pub fn export_ddl(&self, schema: &str, relation: &str) -> Result<ExportDdl, DbError> {
        let apart = |engine: Engine| {
            engine
                .spec()
                .abilities
                .exports_schema
                .is_some_and(|export| export.foreign_keys == spec::ForeignKeyReplay::AddedAfterRows)
        };
        match self {
            Self::Postgres(connection) if apart(Engine::Postgres) => {
                connection.export_ddl(schema, relation)
            }
            Self::SqlServer(connection) if apart(Engine::SqlServer) => {
                connection.export_ddl(schema, relation)
            }
            _ => Ok(ExportDdl {
                create: self.ddl(schema, relation, RelationKind::Table)?,
                foreign_keys: Vec::new(),
            }),
        }
    }

    /// The sequences the columns of `tables` own, for a schema export. Only
    /// Postgres has them to bring back.
    pub fn export_sequences(&self, schema: &str, tables: &[String]) -> Result<Sequences, DbError> {
        match self {
            Self::Postgres(connection) => connection.export_sequences(schema, tables),
            _ => Ok(Sequences::default()),
        }
    }

    /// Ask the server to stop the statement running under `cancel` -- on
    /// Postgres, MySQL and SQLite, whatever this connection is running.
    ///
    /// Takes `&self` and touches the connection mutex nowhere, deliberately:
    /// the runaway statement is holding that mutex, so a cancel that waited for
    /// it would deadlock against the very query it exists to stop. Every handle
    /// this needs is therefore taken in each engine's `open`, off the client,
    /// before the client is moved in behind the mutex.
    ///
    /// There is no cancelled state anywhere above this: a stopped statement
    /// comes back out of [`Connection::query`] as an ordinary `Err` carrying the
    /// server's own words, which is a truer account than dbdelve could write.
    ///
    /// What it cannot do. It reaches only the statement running on *this*
    /// connection, so a catalog or structure load queued behind it on the same
    /// mutex is untouched -- the profile is still frozen until the running
    /// statement lets go. And it is the *slow* runaway it helps with, not the
    /// fat one: Postgres buffers a whole result set before dbdelve sees a row, so
    /// a query already returning gigabytes is past the point where stopping the
    /// server helps.
    pub fn cancel(&self, cancel: &CancelToken) -> Result<(), DbError> {
        let stops_alongside = !matches!(
            self,
            Self::Snowflake(_) | Self::ClickHouse(_) | Self::MongoDb(_) | Self::External(_)
        );
        if let Some(Some(sent)) = cancel.stop_alongside(|alongside| {
            stops_alongside.then(|| {
                alongside
                    .iter()
                    .map(|connection| connection.cancel(&CancelToken::default()))
                    .fold(Ok(()), Result::and)
            })
        }) {
            return sent;
        }
        match self {
            Self::Postgres(connection) => connection.cancel(),
            Self::MySql(connection) => connection.cancel(),
            Self::SqlServer(connection) => connection.cancel(),
            Self::Sqlite(connection) => connection.cancel(),
            Self::Turso(connection) => connection.cancel(),
            Self::DuckDb(connection) => connection.cancel(),
            Self::Snowflake(connection) => connection.cancel(cancel),
            Self::ClickHouse(connection) => connection.cancel(cancel),
            Self::MongoDb(connection) => connection.cancel(cancel),
            Self::Redis(connection) => connection.cancel(),
            Self::External(connection) => connection.cancel(cancel),
        }
    }

    /// Whether the session behind a failed statement is gone, so nothing sent
    /// on it again can succeed. Asked only after a failure.
    ///
    /// Only the engines with one long-lived session can lose it: SQL Server
    /// reconnects inside its next run, MongoDB's driver redials from its pool,
    /// Snowflake speaks HTTP, and a SQLite file has no socket. A plugin's is
    /// lost with its process, or when it says its session is.
    pub fn is_lost(&self) -> bool {
        match self {
            Self::Postgres(connection) => connection.is_lost(),
            Self::MySql(connection) => connection.is_lost(),
            Self::External(connection) => connection.is_lost(),
            Self::SqlServer(_)
            | Self::Sqlite(_)
            | Self::Turso(_)
            | Self::DuckDb(_)
            | Self::Snowflake(_)
            | Self::ClickHouse(_)
            | Self::MongoDb(_)
            | Self::Redis(_) => false,
        }
    }

    /// One round trip on this session, so an idle cut-off between here and the
    /// server does not close it. Returns at once, sending nothing, when a
    /// statement holds the session, and is a no-op on an engine with no
    /// long-lived session to keep (`Target::keeps_alive`). The error is a
    /// session that did not answer.
    ///
    /// `idle` is asked once a session that runs a statement to answer is
    /// held: one a transaction was opened on since the round was set is left
    /// alone. MySQL's ping and Redis's are no statement and take no snapshot.
    pub fn ping(&self, idle: &dyn Fn() -> bool) -> Result<(), DbError> {
        match self {
            Self::Postgres(connection) => connection.ping(idle),
            Self::MySql(connection) => connection.ping(),
            Self::SqlServer(connection) => connection.ping(idle),
            Self::Redis(connection) => connection.ping(),
            Self::Sqlite(_)
            | Self::Turso(_)
            | Self::DuckDb(_)
            | Self::Snowflake(_)
            | Self::ClickHouse(_)
            | Self::MongoDb(_)
            | Self::External(_) => Ok(()),
        }
    }

    /// Ask the server to hold this session to reads, or let go of that hold.
    ///
    /// Defence in depth, not a privilege boundary: this is a session setting,
    /// the same user can flip it back with a statement of their own, and it is
    /// only ever sent from inside dbdelve. A role without write grants is the
    /// only thing that actually stops a write -- this exists so a bug in
    /// `sql::gate` (src/sql.rs:1268), the real boundary, is not the only thing
    /// standing between Read-only and a write reaching the server.
    pub fn set_read_only(&self, read_only: bool) -> Result<(), DbError> {
        match self {
            Self::ClickHouse(connection) => return connection.set_read_only(read_only),
            Self::External(connection) => return connection.set_read_only(read_only),
            _ => {}
        }
        let engine = match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(connection) => connection.engine(),
            Self::SqlServer(_) => Engine::SqlServer,
            Self::Sqlite(_) => Engine::Sqlite,
            Self::Turso(_) => Engine::Turso,
            Self::DuckDb(_) => Engine::DuckDb,
            Self::Snowflake(_) => Engine::Snowflake,
            Self::ClickHouse(_) => Engine::ClickHouse,
            Self::MongoDb(_) => Engine::MongoDb,
            Self::Redis(_) => Engine::Redis,
            Self::External(connection) => connection.speaks(),
        };
        let Some(statement) = read_only_statement(engine, read_only) else {
            return Ok(());
        };
        self.query(statement, &CancelToken::default(), Fetch::default())?;
        // Every session the driver opens or resets from here on takes it
        // before the startup script replays.
        match self {
            Self::Postgres(connection) => connection.remember_hold(read_only),
            Self::MySql(connection) => connection.remember_hold(read_only),
            _ => {}
        }
        Ok(())
    }

    /// The database the driver says it reached, where a plugin driver
    /// carries the connection: the form suggests a dialect from it.
    pub fn reported_dbms(&self) -> Option<&str> {
        match self {
            Self::External(connection) => Some(&connection.opened().dbms_name),
            _ => None,
        }
    }

    /// The plugin's driver by its own name, beside [`Self::reported_dbms`].
    pub fn reported_driver(&self) -> Option<&str> {
        match self {
            Self::External(connection) => Some(&connection.opened().driver),
            _ => None,
        }
    }

    /// Whether a read stopped at the row limit can end there rather than be
    /// drained to its end. Every built-in driver can; a plugin says.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the conformance suite reads it")
    )]
    pub fn stops_at_limit(&self) -> bool {
        match self {
            Self::External(connection) => connection.stops_at_limit(),
            _ => true,
        }
    }
}

/// One column of a result set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The server's own name for the column's type — `int4`, `jsonb`,
    /// `timestamptz` — as a dbdelve-owned string, never a driver type.
    ///
    /// Absent rather than guessed. The simple query protocol carries no type
    /// information at all, so this is learned by describing the statement, and
    /// Postgres will not describe everything (see [`column_types`]).
    pub data_type: Option<String>,
}

/// Whether a [`Column::data_type`] names a type whose values are numbers.
///
/// Exact names rather than the substrings [`Engine::is_binary_type`] can afford: the
/// numeric families collide with types that are not numbers at all -- `interval`
/// and `point` both contain `int`, and `bit` is a string of them. A wrong answer
/// here only costs one column its alignment, but a column of timestamps flushed
/// right because it answered to `int` is a worse read than one left alone.
///
/// What it is for: the grid right-aligns these, because a column of numbers that
/// do not share a last digit cannot be compared down its own length.
pub fn is_numeric_type(data_type: &str) -> bool {
    let lowered = data_type.to_ascii_lowercase();
    // A precision says how wide a number is, not whether it is one; MySQL's
    // attributes say how it is stored.
    let name = lowered
        .split_once('(')
        .map_or(lowered.as_str(), |(base, _)| base)
        .trim()
        .trim_end_matches(" zerofill")
        .trim_end_matches(" unsigned")
        .trim_end();

    matches!(
        name,
        "int"
            | "int2"
            | "int4"
            | "int8"
            | "long"
            | "integer"
            | "tinyint"
            | "smallint"
            | "mediumint"
            | "bigint"
            // DuckDB's unsigned and 128-bit widths, and its arbitrary one.
            | "utinyint"
            | "usmallint"
            | "uinteger"
            | "ubigint"
            | "hugeint"
            | "uhugeint"
            | "bignum"
            | "serial"
            | "smallserial"
            | "bigserial"
            | "float"
            | "float4"
            | "float8"
            | "real"
            | "double"
            | "double precision"
            | "numeric"
            | "decimal"
            | "dec"
            | "number"
            | "money"
            | "smallmoney"
    )
}

/// The cell type of a field its document does not have: no value at all,
/// where a null is a value.
pub const MISSING: &str = "missing";

/// The tag [`QueryResult::cell_types`] holds for `name`, for a snapshot read
/// back from disk; `None` for a name no engine gives.
pub fn cell_type(name: &str) -> Option<&'static str> {
    mongo::cell_type(name).or_else(|| crate::redis_cmd::edit::cell_type(name))
}

/// A cell value, already formatted by the server. `None` is SQL NULL, which is
/// distinct from an empty string and must stay distinguishable in the grid.
pub type Cell = Option<String>;

/// A row's cells, packed: every value's text in one buffer, and where each
/// ends. A `Vec<Cell>` spends a 24-byte header and a heap block of its own on
/// every cell, which on a wide table of small values outweighs the values --
/// a million rows of 106 columns held 7.6 GB for 3.6 GB of text.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Row {
    text: Box<str>,
    /// Each cell's end in `text`, with [`NULL`] set for a NULL, whose end is
    /// the one before it. One spelling per row, so derived equality is the
    /// cells' equality.
    ends: Box<[u32]>,
}

const NULL: u32 = 1 << 31;

impl Row {
    /// Fails past 2 GB of text in the row, which `ends` cannot address.
    pub fn new<S: AsRef<str>>(cells: impl IntoIterator<Item = Option<S>>) -> Result<Self, DbError> {
        let mut builder = RowBuilder::default();
        for cell in cells {
            builder.push(cell.as_ref().map(AsRef::as_ref));
        }
        builder.finish()
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// `None` for a NULL and for a column past the row's end alike.
    pub fn get(&self, col: usize) -> Option<&str> {
        let end = *self.ends.get(col)?;
        if end & NULL != 0 {
            return None;
        }
        let start = col
            .checked_sub(1)
            .map_or(0, |before| self.ends[before] & !NULL);
        Some(&self.text[start as usize..end as usize])
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<&str>> + '_ {
        (0..self.len()).map(|col| self.get(col))
    }

    /// Bytes of cell text, a NULL counting none.
    pub fn text_len(&self) -> usize {
        self.text.len()
    }

    /// Put each `(column, value)` in place, the row rebuilt once however many
    /// there are. A column past the row's end is skipped, and a row the edits
    /// would take past 2 GB is left as it was.
    pub fn replace(&mut self, edits: &[(usize, Option<&str>)]) {
        let mut builder = RowBuilder::default();
        for col in 0..self.len() {
            let edit = edits.iter().rev().find(|(at, _)| *at == col);
            builder.push(edit.map_or_else(|| self.get(col), |(_, value)| *value));
        }
        if let Ok(row) = builder.finish() {
            *self = row;
        }
    }
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl PartialEq<Vec<Cell>> for Row {
    fn eq(&self, other: &Vec<Cell>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b.as_deref())
    }
}

impl TryFrom<Vec<Cell>> for Row {
    type Error = DbError;

    fn try_from(cells: Vec<Cell>) -> Result<Self, DbError> {
        Self::new(cells)
    }
}

/// Written as the list of cells it holds, so a snapshot reads the same as it
/// did when rows were `Vec<Cell>`, to this build and to older ones.
impl Serialize for Row {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(Vec::<Cell>::deserialize(deserializer)?)
            .map_err(|error| serde::de::Error::custom(error.message))
    }
}

#[cfg(test)]
pub(crate) fn rows(cells: Vec<Vec<Cell>>) -> Vec<Row> {
    cells
        .into_iter()
        .map(|row| Row::try_from(row).unwrap())
        .collect()
}

/// Packs rows from cells a driver already holds as text, one push at a time.
/// Kept across rows: its buffers are reused, and each row is copied out at
/// its exact size.
#[derive(Default)]
pub struct RowBuilder {
    text: String,
    ends: Vec<u32>,
    overflowed: bool,
}

impl RowBuilder {
    pub fn push(&mut self, cell: Option<&str>) {
        let null = match cell {
            Some(value) => {
                self.text.push_str(value);
                0
            }
            None => NULL,
        };
        match u32::try_from(self.text.len()) {
            Ok(end) if end & NULL == 0 => self.ends.push(end | null),
            _ => self.overflowed = true,
        }
    }

    /// The row pushed since the last finish, and a fresh start either way.
    pub fn finish(&mut self) -> Result<Row, DbError> {
        let row = match std::mem::take(&mut self.overflowed) {
            false => Ok(Row {
                text: self.text.as_str().into(),
                ends: self.ends.as_slice().into(),
            }),
            true => Err(plain_error(
                "A row holds more than 2 GB of text, more than DBDelve can show.".into(),
            )),
        };
        self.text.clear();
        self.ends.clear();
        row
    }
}

/// Serialized because a restored tab has to know which kind it is before the
/// catalog that would say so has loaded -- and a table is what a profile
/// written before the kind was stored gets read back as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    #[default]
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
    /// ClickHouse only: dropped with `DROP DICTIONARY`, and never truncated.
    Dictionary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
    /// The relation this one is a partition of, by name, in the same schema.
    /// A name rather than an index because the catalog is assembled a row at a
    /// time and a parent can arrive after its children; a kind would not do at
    /// all, since a partition of a partitioned table is an ordinary table
    /// everywhere else it is looked at.
    pub partition_of: Option<String>,
    /// Estimated on-disk bytes, mostly from the engine's stored statistics.
    /// `None` for a view, on SQLite, and on Postgres and MySQL until
    /// [`Catalog::set_sizes`] has filled it in, which it may never do.
    pub size: Option<u64>,
    /// How many rows the engine's statistics say the table holds, from the
    /// same place `size` comes from, so it is `None` wherever `size` is and
    /// also wherever the statistics were never gathered. An estimate unless
    /// [`Target::exact_row_estimates`] says otherwise.
    pub rows: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routine {
    pub name: String,
    pub kind: RoutineKind,
    pub identity_arguments: String,
    pub result_type: String,
    pub language: String,
    pub definition: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub relations: Vec<Relation>,
    pub routines: Vec<Routine>,
    /// Names only, in the server's order. Read in the catalog's second half
    /// with the routines, so [`Connection::catalog`] never carries any.
    pub sequences: Vec<String>,
}

/// What a connection did with a picked database or schema that was not there
/// when it opened: it opened where it would have with nothing picked, or on
/// the picked database without the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unpicked {
    /// The database was dropped as well, so the connection is on another
    /// database than the profile named, not just another schema.
    pub database: bool,
    pub message: String,
}

/// On-disk bytes and row estimates by schema, then relation name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Databases {
    pub names: Vec<String>,
    /// The one this connection is in. MySQL can be in none.
    pub current: Option<String>,
    /// How many names the include filter left out of `names`. Set only by
    /// [`Shown::narrow_databases`], so zero is also what an unfiltered read
    /// says.
    pub left_out: usize,
}

impl Databases {
    /// Stable, so each half keeps the order the server listed it in.
    fn system_last(&mut self, engine: Engine) {
        self.names
            .sort_by_key(|name| engine.is_system_database(name));
    }
}

pub type Sizes = std::collections::HashMap<String, std::collections::HashMap<String, Statistics>>;

/// What the engine's statistics say about one relation. Either half can be
/// missing on its own: a never-analyzed table can still be measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    pub size: Option<u64>,
    pub rows: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
    /// The database this was read from, where a statement of the user's can
    /// move the session it is read on off the one the profile picked:
    /// DuckDB's `USE`. None where the profile says which.
    pub database: Option<String>,
    /// How many schemas, or databases where the catalog lists databases as
    /// schemas, the include filter dropped. Set only by the narrowing, so
    /// [`Catalog::merge`] and [`Catalog::set_sizes`] leave it alone.
    pub left_out: usize,
}

impl Catalog {
    /// Fold a later half into this one: the routines and sequences of a schema
    /// already here join it, and a schema that holds only those is added in its
    /// place.
    ///
    /// Assembled by name rather than by position, because the two halves are
    /// separate queries and a schema can appear in either alone -- one holding
    /// only functions is in the second and not the first.
    ///
    /// A new schema goes on the end, never in name order: explorer ids and
    /// palette targets taken before the merge address schemas by index, and a
    /// sorted insert would point them at a neighbour. [`Self::by_name`] is the
    /// order to show them in.
    ///
    /// Sequences read in another database than this catalog's are left out:
    /// matched by schema name alone they would be listed under this one.
    pub fn merge(&mut self, other: Self) {
        let elsewhere = matches!(
            (&self.database, &other.database),
            (Some(mine), Some(theirs)) if mine != theirs
        );
        for mut schema in other.schemas {
            if elsewhere {
                schema.sequences.clear();
            }
            match self
                .schemas
                .iter_mut()
                .find(|existing| existing.name == schema.name)
            {
                Some(existing) => {
                    existing.routines = schema.routines;
                    if !elsewhere {
                        existing.sequences = schema.sequences;
                    }
                }
                // A schema that held only those sequences is not this
                // database's.
                None if elsewhere && schema.routines.is_empty() => {}
                None => self.schemas.push(schema),
            }
        }
    }

    /// Put each schema's sequences on the schema of that name here, pushing a
    /// schema it does not hold, so [`Self::merge`] later brings in one that
    /// holds only sequences.
    pub fn join_sequences(&mut self, sequences: Self) {
        self.database = self.database.take().or(sequences.database);
        for schema in sequences.schemas {
            match self
                .schemas
                .iter_mut()
                .find(|existing| existing.name == schema.name)
            {
                Some(existing) => existing.sequences = schema.sequences,
                None => self.schemas.push(schema),
            }
        }
    }

    /// Write sizes and row estimates onto the relations already here, leaving
    /// any it has no number for alone. It touches nothing but those and
    /// [`Self::merge`] nothing but routines, so the two may land in either
    /// order.
    pub fn set_sizes(&mut self, sizes: &Sizes) {
        for schema in &mut self.schemas {
            let Some(sizes) = sizes.get(&schema.name) else {
                continue;
            };
            for relation in &mut schema.relations {
                if let Some(statistics) = sizes.get(&relation.name) {
                    relation.size = statistics.size.or(relation.size);
                    relation.rows = statistics.rows.or(relation.rows);
                }
            }
        }
    }

    /// What [`Catalog::set_sizes`] would write back, for carrying the numbers
    /// over to a newer catalog of the same database.
    pub fn sizes(&self) -> Sizes {
        self.schemas
            .iter()
            .map(|schema| {
                let relations = schema.relations.iter().map(|relation| {
                    let statistics = Statistics {
                        size: relation.size,
                        rows: relation.rows,
                    };
                    (relation.name.clone(), statistics)
                });
                (schema.name.clone(), relations.collect())
            })
            .collect()
    }

    /// Every schema with its index, in name order.
    pub fn by_name(&self) -> Vec<(usize, &Schema)> {
        let mut schemas = self.schemas.iter().enumerate().collect::<Vec<_>>();
        schemas.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
        schemas
    }
}

/// One relation's definition. Loaded when the relation is opened rather than at
/// connect: a database with thousands of relations would pay for every one of
/// them to show the columns of the one that was clicked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Structure {
    pub columns: Vec<ColumnDefinition>,
    pub indexes: Vec<Index>,
    pub constraints: Vec<NamedDefinition>,
    pub foreign_keys: Vec<ForeignKey>,
    /// The single-column foreign keys of other relations that point at this one.
    /// Filled from [`Connection::references`] by the callers that want it, not
    /// by [`Connection::structure`]: completion calls that one per relation it
    /// sees, and only wants who points back while it is suggesting joins.
    pub referenced_by: Vec<Reference>,
}

impl Structure {
    /// The columns that tell one row from another: the primary key's, else the
    /// first unique constraint's, read back out of their `PRIMARY KEY (a, b)`
    /// rendering. Empty when none reads back as columns the relation has --
    /// a name holding `, ` cannot be told from two names, and is not guessed at.
    pub fn row_key(&self) -> Vec<String> {
        self.key_of("PRIMARY KEY (", false)
            .or_else(|| self.key_of("UNIQUE (", false))
            .unwrap_or_default()
    }

    /// The primary key's columns alone, where [`Structure::row_key`] falls back
    /// to a unique constraint. Read through any quoting the rendering carries:
    /// SQLite writes `PRIMARY KEY ("id")`, and this is for marking columns
    /// rather than for writing SQL.
    pub fn primary_key(&self) -> Vec<String> {
        self.key_of("PRIMARY KEY (", true).unwrap_or_default()
    }

    fn key_of(&self, prefix: &str, unquote: bool) -> Option<Vec<String>> {
        self.constraints.iter().find_map(|constraint| {
            let names: Vec<String> = constraint
                .definition
                .strip_prefix(prefix)?
                .strip_suffix(')')?
                .split(", ")
                .map(|name| match unquote {
                    true => name.trim_matches(['"', '`', '[', ']']).to_string(),
                    false => name.to_string(),
                })
                .collect();
            names
                .iter()
                .all(|name| self.columns.iter().any(|column| &column.name == name))
                .then_some(names)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
}

/// A constraint, as the name plus the server's own rendering of it. Postgres
/// already prints one as readable DDL, so parsing it into fields would only
/// lose information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedDefinition {
    pub name: String,
    pub definition: String,
}

/// An index as fields read from the catalog it was rendered from, beside the
/// server's own rendering, which is what DDL is written from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub kind: IndexKind,
    /// The key, one column or expression per entry in order, each as the
    /// server renders it: `lower(email)`, `created_at DESC`, `name(10)`.
    /// Included columns are not key, and show only in the definition.
    pub columns: Vec<String>,
    /// A partial index's condition, without its `WHERE`.
    pub predicate: Option<String>,
    pub definition: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    Primary,
    Unique,
    Plain,
}

/// One column of one relation, and the column it references. Enough to write a
/// `WHERE` against the referenced relation and nothing else.
///
/// A composite key is several of these and nothing groups them: following a key
/// is a per-column gesture, so the constraint they came from is not something a
/// caller has to reassemble.
///
/// Additional to the rendered DDL in [`Structure::constraints`], not a
/// replacement for it — the text is what the structure tab shows, and parsing a
/// server's rendering back into fields would only lose information. Computing
/// the fields from the catalog that text was rendered from loses nothing.
///
/// Engine-agnostic by rule, not by accident: hard rule 4. No oid, no attribute
/// number, no `information_schema` row and no driver value reaches these four
/// owned strings, and nothing here says which engine answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub column: String,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_column: String,
}

/// A foreign key seen from the relation it points at: `column` of `schema.table`
/// holds values of this relation's `referenced_column`. Single-column keys only,
/// because a filter on one column of a composite key matches rows that do not
/// reference the row it was asked from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub referenced_column: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    /// Total bytes of returned cell text. Shown in the status bar so the cost
    /// of a wide or geometry-heavy result is visible rather than mysterious.
    pub bytes: usize,
    pub elapsed: Duration,
    /// The command's server-reported row count. The simple protocol reports
    /// zero both for commands that affected no rows and commands without a row
    /// count, so callers must not infer the command kind from this value.
    pub rows_affected: Option<u64>,
    /// How many rows the statement returned, when the row limit kept fewer of
    /// them than that. `None` when `rows` is the whole result.
    pub capped_from: Option<usize>,
    /// Set when the statement was stopped before it finished, so `rows` are
    /// what had arrived by then and how many more there were is unknown.
    pub stopped: Option<Stopped>,
    /// What running the statement cost beyond its result, for the user to be
    /// told beside it: SQL Server's session, reset by a Cancel that landed
    /// after the statement had finished.
    pub notice: Option<String>,
    /// What the server said while running the submission, in the order it said
    /// it: SQL Server's `PRINT` and informational messages, Postgres notices,
    /// MySQL warnings. The whole submission's, on its first result; the sets
    /// in `rest` carry none.
    pub messages: Vec<String>,
    /// Each cell's type, row by row, where a column's type is not every one of
    /// its cells': a MongoDB field holds whatever each document put there. The
    /// server's `$type` names (`int`, `objectId`, …), and [`MISSING`] for a
    /// field the document does not have. Empty on every SQL engine, whose
    /// columns are typed whole ([`Column::data_type`]).
    pub cell_types: Vec<Vec<&'static str>>,
    /// Where these rows can be written back to, when they can be at all.
    /// `None` is the answer for every result set dbdelve cannot address a single
    /// row of, and it is not an error — see [`Connection::edit_target`].
    pub edit: Option<EditTarget>,
    /// Why these rows cannot be written back, where the engine knows and the
    /// user would otherwise be told only that the row has no primary key.
    pub read_only_reason: Option<String>,
    /// The submission's result sets after this one, in order.
    ///
    /// One level deep and never a tree: a nested `rest` is always empty,
    /// because these are the sets of one submission and a set has no
    /// submission of its own.
    ///
    /// Empty on every engine but SQL Server. Everywhere else a submission
    /// holds one statement — that is the unit `sql::queued_statements` splits
    /// those engines on — so there is never a second set to carry. SQL Server
    /// is split on the `GO`-separated batch instead, because a batch is a
    /// scope boundary, and one batch readily returns several.
    pub rest: Vec<QueryResult>,
}

/// Why a statement's rows end before the statement did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stopped {
    /// It reached the row limit and was safe to stop there.
    AtLimit,
    /// The user cancelled it part way.
    Cancelled,
}

/// The statements a file opens and closes with to write rows past the
/// foreign keys, on an engine whose schema export turns its checks off
/// ([`ForeignKeyReplay::ChecksOff`]). The pair saves and restores the
/// session's own setting, so a session that had checks off is not left with
/// them on.
pub fn checks_off(engine: Engine) -> Option<(&'static str, &'static str)> {
    match engine {
        Engine::MySql | Engine::MariaDb => Some((
            "SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0;",
            "SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS;",
        )),
        _ => None,
    }
}

/// How many of a failed statement's own writes stay applied: the documents an
/// ordered `insertMany` wrote ahead of the one that failed. Every other engine
/// undoes the whole statement.
pub fn applied(engine: Engine, error: &DbError) -> u64 {
    match engine {
        Engine::MongoDb => error
            .message
            .rsplit_once(mongo::APPLIED)
            .and_then(|(_, n)| n.strip_suffix('.')?.parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

/// How a user statement's rows are taken: how many are kept, where they go as
/// they arrive, and whether the statement may be stopped once there are
/// enough of them.
#[derive(Clone, Copy, Default)]
pub struct Fetch<'a> {
    pub limit: Option<usize>,
    pub feed: Option<&'a Feed>,
    /// The submission is one statement that only reads, as `sql::stoppable`
    /// judges it, so stopping it at the limit loses nothing it was going to
    /// do.
    pub reads_only: bool,
    /// Asked once the statement holds its connection, before anything is
    /// sent: `false` drops it unsent. For a statement queued behind another
    /// on a connection it shares ([`Connection::query_alongside`]'s fallback),
    /// so a Cancel before then drops it rather than stopping the other.
    pub held: Option<&'a dyn Fn() -> bool>,
    /// Called once the statement is over, before it lets go of its
    /// connection: a Cancel that found it while it held the connection has
    /// been sent by then, and none that comes later reaches whatever takes the
    /// connection next.
    pub let_go: Option<&'a dyn Fn()>,
}

impl<'a> Fetch<'a> {
    /// [`Fetch::held`], as the error an engine returns before sending. What it
    /// hands back calls [`Fetch::let_go`] when dropped, so it is bound where
    /// it is dropped before the engine's lock on the connection is.
    fn hold(&self) -> Result<LetGo<'a>, DbError> {
        match self.held.is_none_or(|held| held()) {
            true => Ok(LetGo(self.let_go)),
            false => Err(plain_error(CANCELLED_UNSENT.into())),
        }
    }
}

/// See [`Fetch::hold`].
#[must_use]
struct LetGo<'a>(Option<&'a dyn Fn()>);

impl Drop for LetGo<'_> {
    fn drop(&mut self) {
        if let Some(let_go) = self.0 {
            let_go();
        }
    }
}

/// Rows handed over while a statement is still running, so the first of a
/// large result is on screen long before the last arrives. The driver pushes
/// and the grid takes; a row sits here only between the two.
#[derive(Clone, Default)]
pub struct Feed(Arc<Shared>);

#[derive(Default)]
struct Shared {
    fed: Mutex<Fed>,
    /// Signalled whenever rows leave, for a push waiting under `bound`.
    room: Condvar,
    bound: Option<usize>,
}

/// What a [`Feed`] holds since it was last taken from.
#[derive(Default)]
pub struct Fed {
    /// Counts result sets. A taker whose rows came from an earlier set than
    /// this one has rows of a statement the submission has moved past.
    pub set: usize,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    /// The first set's column types, once a describe on another connection
    /// has said, so rows can be shown by type before the statement is done.
    /// Empty until then.
    types: Vec<String>,
    /// Until when the first set's rows are held back for that describe, so
    /// they are first shown by type rather than restyled under the user's
    /// eye. Cleared once it has answered, or the statement is over.
    awaiting: Option<std::time::Instant>,
    /// When the current set's kept rows reached the limit. Whatever the run
    /// does after that is not fetching rows anyone will see.
    pub filled: Option<std::time::Instant>,
}

impl Feed {
    /// A feed that holds at most `rows` rows: a push past them waits for the
    /// next take, so a driver faster than its taker is slowed to the taker's
    /// pace rather than piling the result up here.
    ///
    /// Only for a taker that takes until the statement is over, whatever
    /// happens: a push nobody takes from waits forever, on the thread running
    /// the statement.
    pub fn bounded(rows: usize) -> Self {
        Self(Arc::new(Shared {
            bound: Some(rows),
            ..Shared::default()
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Fed> {
        self.0
            .fed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn begin(&self, columns: Vec<Column>) {
        let mut fed = self.lock();
        fed.set += 1;
        fed.columns = columns;
        fed.rows.clear();
        fed.filled = None;
        self.0.room.notify_all();
    }

    pub(crate) fn push(&self, row: Row) {
        let mut fed = self.lock();
        if let Some(bound) = self.0.bound {
            // The wait lets go of the lock, so a describe or a take is never
            // shut out by the push waiting on it.
            fed = self
                .0
                .room
                .wait_while(fed, |fed| fed.rows.len() >= bound)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        fed.rows.push(row);
    }

    /// The rows since the last take, under the set's columns, typed once the
    /// types are known. Positional, and only where the counts agree: the
    /// describe ran apart from the statement, and a type against the wrong
    /// column is worse than none.
    pub fn take(&self) -> Fed {
        let mut fed = self.lock();
        if fed.set == 1
            && fed
                .awaiting
                .is_some_and(|until| std::time::Instant::now() < until)
        {
            return Fed::default();
        }
        let mut columns = fed.columns.clone();
        if fed.set == 1 && fed.types.len() == columns.len() {
            for (column, data_type) in columns.iter_mut().zip(&fed.types) {
                column.data_type = Some(data_type.clone());
            }
        }
        let taken = Fed {
            set: fed.set,
            columns,
            rows: std::mem::take(&mut fed.rows),
            types: Vec::new(),
            awaiting: None,
            filled: fed.filled,
        };
        self.0.room.notify_all();
        taken
    }

    /// Types learned from a row rather than the description before it.
    pub(crate) fn retype(&self, columns: Vec<Column>) {
        self.lock().columns = columns;
    }

    pub(crate) fn fill(&self) {
        self.lock().filled = Some(std::time::Instant::now());
    }

    /// Hold the first set's rows until [`Feed::describe`] or `until`.
    pub(crate) fn await_types(&self, until: std::time::Instant) {
        self.lock().awaiting = Some(until);
    }

    /// The describe's answer, empty when it learned nothing. Rows held for it
    /// have their geometry rendered here, off the frame thread, as the
    /// driver renders every row after them.
    pub(crate) fn describe(&self, types: Vec<String>) {
        let mut fed = self.lock();
        fed.awaiting = None;
        if fed.set == 1 && types.len() == fed.columns.len() {
            let spatial: Vec<bool> = types
                .iter()
                .map(|name| postgres::is_spatial(name))
                .collect();
            if spatial.contains(&true) {
                for row in &mut fed.rows {
                    postgres::format_spatial_row(row, &spatial);
                }
            }
        }
        fed.types = types;
    }

    /// Let held rows go: the statement is over, and the rows are its result.
    pub(crate) fn release(&self) {
        self.lock().awaiting = None;
    }

    /// `None` until [`Feed::describe`] has been told.
    pub(crate) fn types(&self) -> Option<Vec<String>> {
        let fed = self.lock();
        (!fed.types.is_empty()).then(|| fed.types.clone())
    }
}

/// The cells of `result` still waiting to be rendered by their column's type,
/// as `(row, column, value)`: a fed result's rows can arrive before the types
/// they are rendered by. Copied out so [`render`] can work on them off the
/// frame thread.
pub fn unrendered(engine: Engine, result: &QueryResult) -> Vec<(usize, usize, String)> {
    if engine != Engine::Postgres {
        return Vec::new();
    }
    let spatial: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| {
            column
                .data_type
                .as_deref()
                .is_some_and(postgres::is_spatial)
        })
        .map(|(index, _)| index)
        .collect();
    if spatial.is_empty() {
        return Vec::new();
    }
    result
        .rows
        .iter()
        .enumerate()
        .flat_map(|(row, cells)| {
            spatial.iter().filter_map(move |&column| {
                let value = cells.get(column)?;
                // EWKB opens with its byte-order flag, `00` or `01`, which no
                // WKT keyword does: a cheap test before a copy of the value.
                (value.starts_with("00") || value.starts_with("01"))
                    .then(|| (row, column, value.to_string()))
            })
        })
        .collect()
}

/// What [`unrendered`] found, rendered: the value it was, and what it reads as.
pub fn render(cells: Vec<(usize, usize, String)>) -> Vec<(usize, usize, String, String)> {
    cells
        .into_iter()
        .filter_map(|(row, column, value)| {
            let rendered = postgres::render_spatial(&value)?;
            Some((row, column, value, rendered))
        })
        .collect()
}

impl QueryResult {
    /// The rows the statement returned, held or not.
    pub fn total_rows(&self) -> usize {
        self.capped_from.unwrap_or(self.rows.len())
    }
}

/// The table a result set's rows can be written back to, already resolved to
/// names and result-column positions.
///
/// The identity work — which oid, which attribute number — happens inside this
/// module and stops here (hard rule 4). A caller gets an answer it can build
/// SQL from, not a puzzle it has to ask the catalog about.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// The real column name behind each result column, positionally. `None`
    /// where the result column is computed rather than read from the table, so
    /// `SELECT id AS ident, count(*)` gives `[Some("id"), None]`.
    pub columns: Vec<Option<String>>,
    /// Result-column indices that together identify one row. Never empty.
    pub keys: Vec<usize>,
    /// What `keys` is: a unique key, or every value the row was read with.
    #[serde(default)]
    pub row_match: RowMatch,
    /// How an update of a row is written.
    #[serde(default)]
    pub update: UpdateSpelling,
}

/// How a row is named when it is written back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowMatch {
    /// By a unique key, whose values are never NULL.
    #[default]
    Key,
    /// By every value the row was read with, for an engine with no unique key;
    /// `keys` lists them all, and a NULL is matched as `IS NULL`. `fixed` are
    /// the result columns that cannot be set.
    EveryColumn { fixed: Vec<usize> },
}

/// The statement an edited row is written with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateSpelling {
    /// `UPDATE t SET … WHERE …`.
    #[default]
    Update,
    /// `ALTER TABLE t UPDATE … WHERE … SETTINGS mutations_sync = 2`.
    AlterUpdate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    pub message: String,
    /// Byte offset into the submitted statement, when the server reports one.
    /// Used to point at the offending token instead of the whole statement.
    pub position: Option<usize>,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

/// The second half of the catalog: the routines, with the sequences joined
/// in where the engine lists them.
///
/// A failed sequence read lists none and keeps the routines. The catalogs that
/// hold sequences are readable by every role on every engine that lists them,
/// so what fails is a server that has none (SQL Server before 2012 has no
/// `sys.sequences`), and an empty list is then the true one.
fn with_sequences(
    engine: Engine,
    routines: Result<Catalog, DbError>,
    sequences: impl FnOnce() -> Result<Catalog, DbError>,
) -> Result<Catalog, DbError> {
    let mut second = routines?;
    if Target::built_in(engine).abilities().lists_sequences
        && let Ok(sequences) = sequences()
    {
        second.join_sequences(sequences);
    }
    Ok(second)
}

/// Sequence rows, `schema_name` and `sequence_name`, grouped by schema in the
/// order the server gave them, as a catalog holding nothing else.
pub(super) fn assemble_sequences(result: QueryResult) -> Result<Catalog, DbError> {
    let mut schemas = Vec::<Schema>::new();
    for row in &result.rows {
        let schema_name = required_cell(&result, row, "schema_name")?;
        let name = required_cell(&result, row, "sequence_name")?.to_string();
        match schemas.last_mut() {
            Some(last) if last.name == schema_name => last.sequences.push(name),
            _ => schemas.push(Schema {
                name: schema_name.to_string(),
                relations: Vec::new(),
                routines: Vec::new(),
                sequences: vec![name],
            }),
        }
    }
    Ok(Catalog {
        schemas,
        database: None,
        ..Default::default()
    })
}

/// What one statement of a verified apply writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepKind {
    Insert,
    Update,
    Delete,
}

impl StepKind {
    /// The keyword a generated statement of this kind opens with.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// One column of a row as it was fetched: the text the grid held, by the real
/// column's name and not by position, copied by value when the review was
/// written, since an in-memory sort can move rows while a review is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FetchedCell {
    pub column: String,
    /// `None` for a NULL.
    pub text: Option<String>,
    /// The cell's own type where the result types each cell, as MongoDB's
    /// does; `None` on every SQL engine, whose columns are typed whole.
    pub cell_type: Option<String>,
    /// The fetched column's [`Column::data_type`]. Never compared: an arm
    /// reads it to render a re-read's cell as the grid rendered the fetch's.
    pub column_type: Option<String>,
}

/// One statement of a verified apply, with what proves the row it writes is
/// still the row the user read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedStep {
    /// One statement [`crate::sql::generated_writes`] cut out of the gated
    /// text, without its bracket.
    pub sql: String,
    pub kind: StepKind,
    /// The read of the row an `UPDATE` or a `DELETE` names, taken inside the
    /// apply's transaction before any write ([`crate::sql::row_reread`]).
    /// `None` for an `INSERT`, which has no row to read.
    pub reread: Option<String>,
    /// The row as it was fetched, empty for an `INSERT`.
    pub before: Vec<FetchedCell>,
    /// A read of the row, run after this statement's write inside the same
    /// transaction, so a Row-mode send shows what the server now holds.
    pub read_back: Option<String>,
    /// Whether the row is named by its key, so a second read that finds
    /// nothing proves it gone. A row named by every value is not: an earlier
    /// write that changed one of its values also empties that read.
    pub by_key: bool,
}

/// Where a verified apply stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplyStage {
    Begin,
    Reread(usize),
    Write(usize),
    /// The second read of a `DELETE`'s row that counted none.
    Confirm(usize),
    ReadBack(usize),
    Commit,
    /// MongoDB: write `step` was sent and nothing came back to say whether it
    /// applied: the connection failed, or a Cancel or the statement timeout
    /// stopped it on its way.
    Unanswered(usize),
    /// MongoDB: write `step` applied, and its write concern failed.
    Concern(usize),
}

/// How a row differs from the row as it was fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drift {
    /// It is there, and a column differs or is missing.
    Changed,
    /// The re-read returned no row.
    Gone,
    /// The re-read returned this many rows, where one was named.
    Ambiguous(usize),
}

/// What a statement of a verified apply is, handed to the arm's statement
/// closure with the statement so an arm reads a write's count by its kind and
/// leaves a bracket's or a read's alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sending {
    /// The apply's opening bracket. Before sending it the arm asks its server,
    /// on the session it holds, whether a transaction is already open there,
    /// and fails with [`in_a_transaction`] rather than open one inside it.
    Open,
    Bracket,
    /// The apply's `ROLLBACK` failed. The arm makes sure the session is left
    /// in no transaction of the apply's: it ends one still open, resetting
    /// the session where it cannot, and answers `Ok` once none is left.
    Abandon,
    Read,
    Write(StepKind),
    /// A read of the server's catalog the apply asks inside its transaction
    /// ([`rewrites::probe`]): sent as it is, untyped and unpaged.
    Catalog,
}

/// What a verified apply came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    /// Every statement counted the one row it was written for, and the
    /// transaction committed. `affected` is the rows written, all together.
    /// `read_backs` are the read-backs of the steps that had one, each with
    /// its step's index, in step order: what the rows hold once written, read
    /// inside the transaction that wrote them.
    Applied {
        affected: u64,
        read_backs: Vec<(usize, QueryResult)>,
    },
    /// No write was sent: these steps' rows are not the rows that were read.
    Drifted(Vec<(usize, Drift)>),
    /// A statement counted another number of rows than the one it names; the
    /// whole sequence was rolled back. An `UPDATE` and an `INSERT` must count
    /// one. A `DELETE` must count one, or none when its row is named by its key
    /// ([`VerifiedStep::by_key`]) and a second read of it inside the
    /// transaction finds it gone, which an earlier `DELETE` of the draft
    /// taking it along would leave; none with the row still there, or with the
    /// row named by every value, is a statement something skipped. `count` is
    /// `None` when the driver gave no count at all.
    ///
    /// The count is the server's own for that statement, and SQLite's
    /// `changes()` excludes what a trigger or a foreign key wrote, so a
    /// trigger's own writes never raise a count.
    ///
    /// MongoDB has no transaction to roll back. There, a `Miscounted` and a
    /// `Failed` at [`ApplyStage::Write`], [`ApplyStage::Unanswered`],
    /// [`ApplyStage::Concern`] or [`ApplyStage::ReadBack`] mean the steps
    /// before `step` (or, at a write concern or a read-back, up to and including
    /// it) were applied and none after it was sent; `Drifted` and a `Failed`
    /// at [`ApplyStage::Begin`] or [`ApplyStage::Reread`] wrote nothing. It
    /// never answers `Unknown`: there is no `COMMIT` to lose.
    Miscounted { step: usize, count: Option<u64> },
    /// The server refused something before `COMMIT`, or the apply refused to
    /// send: nothing the apply opened stayed applied.
    ///
    /// On MongoDB, which has no transaction to roll back, that is not so:
    /// see [`ApplyOutcome::Miscounted`]. A write the server answered with a
    /// refusal there ([`ApplyStage::Write`]) applied nothing; one nothing
    /// answered ([`ApplyStage::Unanswered`]) may have, so the workspace takes
    /// its outcome as unknown until the rows are read again.
    Failed { stage: ApplyStage, error: DbError },
    /// `COMMIT` was sent and its answer did not come back: whether it landed
    /// is not known.
    Unknown(DbError),
}

/// Compares the row as it was fetched with the row read again.
///
/// The re-read must hold exactly one row: none is [`Drift::Gone`], more is
/// [`Drift::Ambiguous`]. Each fetched cell is matched to the re-read's column
/// of the same name, never by position, and compared whole and byte for byte:
/// NULL equals NULL and nothing else, and no trimming, case folding or number
/// form is applied, so a difference the user cannot see still refuses. A cell
/// the re-read lacks is a difference, a column the fetch did not hold is not
/// compared, and a cell `skipped` names is left out. Where both sides type the
/// cell, a different type is a different value.
pub(crate) fn drift(
    before: &[FetchedCell],
    reread: &QueryResult,
    skipped: impl Fn(&FetchedCell) -> bool,
) -> Option<Drift> {
    match reread.rows.as_slice() {
        [] => return Some(Drift::Gone),
        [_] => {}
        rows => return Some(Drift::Ambiguous(rows.len())),
    }
    let row = &reread.rows[0];
    let changed = before.iter().filter(|cell| !skipped(cell)).any(|cell| {
        let Some(col) = reread
            .columns
            .iter()
            .position(|column| column.name == cell.column)
        else {
            return true;
        };
        let typed = reread.cell_types.first().and_then(|types| types.get(col));
        row.get(col) != cell.text.as_deref()
            || cell
                .cell_type
                .as_deref()
                .zip(typed)
                .is_some_and(|(fetched, now)| fetched != *now)
    });
    changed.then_some(Drift::Changed)
}

/// The sequence of a verified apply, held on one session: every re-read, then
/// the compare, then each write with its count, then `COMMIT`. The one place
/// its rules live; an arm hands it closures over the session it already holds.
///
/// It opens a transaction of its own and every exit before `COMMIT` sends
/// `ROLLBACK`, so nothing it opened stays half applied. Never inside the
/// user's: the caller refuses an apply while one is open on the connection, as
/// the statements it ran read, and the arm asks the server itself before it
/// sends the opening bracket ([`Sending::Open`]), under the lock it holds.
///
/// Every statement is gated again here, whoever built the steps: each write by
/// [`crate::sql::is_generated_write_on`] as the one statement it must be, each
/// re-read as the very [`crate::sql::row_reread`] of its write, and each
/// read-back as its [`crate::sql::row_read`], before anything is sent.
///
/// `send` runs one statement on the held session. `proceed` is asked before
/// each statement and says no once a Cancel has come. `skipped` and `lost` are
/// the arm's: a fetched cell the grid rendered after the fetch and so cannot
/// be compared, and whether an error at `COMMIT` means the session is gone.
///
/// Every re-read runs before the first write, so the draft's own cascades and
/// triggers do not show as drift.
///
/// That leaves the window between a step's re-read and its write spanning the
/// rest of the re-reads and the writes before it, many round trips on a long
/// draft. The arms close it where their server can:
/// Postgres opens the apply `REPEATABLE READ`, so a write to a row another
/// transaction changed since the snapshot fails with 40001; MySQL and MariaDB
/// open it `SERIALIZABLE`, so each re-read holds a shared lock on its rows to
/// the `COMMIT`; SQL Server runs each re-read `REPEATABLE READ`, which does
/// the same. SQLite has one writer, and its re-read's lock, or in WAL a write
/// on a snapshot another has moved past, keeps the window shut; DuckDB fails a
/// write that conflicts with a commit since its snapshot. There is no `FOR
/// UPDATE` on the re-read: the pinned grammar cannot parse it.
pub(super) fn run_verified(
    engine: Engine,
    steps: &[VerifiedStep],
    send: &mut dyn FnMut(&str, Sending) -> Result<QueryResult, DbError>,
    proceed: &dyn Fn() -> bool,
    skipped: &dyn Fn(&FetchedCell) -> bool,
    lost: &dyn Fn(&DbError) -> bool,
) -> ApplyOutcome {
    let refused = |message: &str| ApplyOutcome::Failed {
        stage: ApplyStage::Begin,
        error: plain_error(message.into()),
    };
    if steps.is_empty() {
        return refused("There are no statements to apply.");
    }
    for step in steps {
        let writes_one = matches!(
            crate::sql::generated_writes(engine, &step.sql).as_deref(),
            Some([one]) if *one == step.sql
        );
        let names_kind = step
            .sql
            .get(..step.kind.keyword().len())
            .is_some_and(|word| word.eq_ignore_ascii_case(step.kind.keyword()));
        // Each read is the one cut from this step's own statement, not just
        // some read dbdelve could have written: one of another row would
        // compare that row and write this one.
        let reads = match (step.kind, &step.reread) {
            (StepKind::Insert, None) => true,
            (StepKind::Insert, Some(_)) | (StepKind::Update | StepKind::Delete, None) => false,
            (StepKind::Update | StepKind::Delete, Some(read)) => {
                crate::sql::row_reread(engine, &step.sql).as_deref() == Some(read.as_str())
            }
        };
        let reads_back = step
            .read_back
            .as_deref()
            .is_none_or(|read| crate::sql::row_read(engine, &step.sql).as_deref() == Some(read));
        if !writes_one || !names_kind || !reads || !reads_back {
            return refused(
                "dbdelve refused to run a statement it wrote itself: the draft and its \
                 statements do not match.",
            );
        }
    }
    // Statements go one at a time, so even an engine whose one submission is
    // atomic (`Engine::transaction_start` answers `None` there) needs a `BEGIN`.
    let opening = engine.transaction_start().unwrap_or("BEGIN");
    if !crate::sql::is_transaction_bracket(opening) {
        return refused("The apply could not be verified on this connection.");
    }

    if !proceed() {
        return ApplyOutcome::Failed {
            stage: ApplyStage::Begin,
            error: plain_error("Cancelled before it was sent.".into()),
        };
    }
    // A failed opening sends no `ROLLBACK`: when it failed because a
    // transaction was already open, that transaction is the user's.
    if let Err(error) = send(opening, Sending::Open) {
        return ApplyOutcome::Failed {
            stage: ApplyStage::Begin,
            error,
        };
    }
    // A panic part way, in an arm's `send` or here, would leave the session
    // inside the apply's transaction, holding its locks until it closes: it is
    // rolled back first, and the panic then goes on its way.
    let sequence = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        opened(engine, steps, &mut *send, proceed, skipped, lost)
    }));
    match sequence {
        Ok(outcome) => outcome,
        Err(panic) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| roll_back(send)));
            std::panic::resume_unwind(panic)
        }
    }
}

/// [`run_verified`] once its transaction is open: the re-reads, the compare,
/// the writes and `COMMIT`, every exit before it rolling back.
fn opened(
    engine: Engine,
    steps: &[VerifiedStep],
    send: &mut dyn FnMut(&str, Sending) -> Result<QueryResult, DbError>,
    proceed: &dyn Fn() -> bool,
    skipped: &dyn Fn(&FetchedCell) -> bool,
    lost: &dyn Fn(&DbError) -> bool,
) -> ApplyOutcome {
    let cancelled = || plain_error("Cancelled before it was sent.".into());

    if let Some(error) = moved_under(engine, steps, send) {
        roll_back(send);
        return ApplyOutcome::Failed {
            stage: ApplyStage::Begin,
            error,
        };
    }

    let mut drifted = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let Some(read) = &step.reread else {
            continue;
        };
        if !proceed() {
            roll_back(send);
            return ApplyOutcome::Failed {
                stage: ApplyStage::Reread(index),
                error: cancelled(),
            };
        }
        match send(read, Sending::Read) {
            Ok(result) => {
                drifted.extend(drift(&step.before, &result, skipped).map(|drift| (index, drift)))
            }
            Err(error) => {
                roll_back(send);
                return ApplyOutcome::Failed {
                    stage: ApplyStage::Reread(index),
                    error,
                };
            }
        }
    }
    if !drifted.is_empty() {
        roll_back(send);
        return ApplyOutcome::Drifted(drifted);
    }

    let (mut affected, mut read_backs) = (0, Vec::new());
    for (index, step) in steps.iter().enumerate() {
        if !proceed() {
            roll_back(send);
            return ApplyOutcome::Failed {
                stage: ApplyStage::Write(index),
                error: cancelled(),
            };
        }
        let count = match send(&step.sql, Sending::Write(step.kind)) {
            Ok(result) => result.rows_affected,
            Err(error) => {
                roll_back(send);
                return ApplyOutcome::Failed {
                    stage: ApplyStage::Write(index),
                    error,
                };
            }
        };
        let counted = match (step.kind, count) {
            (_, Some(1)) => true,
            // A `DELETE` an earlier one of the draft took the row of counts
            // none, and so does one something skipped. The second read of its
            // row tells them apart: "cascaded" is observed, not inferred. Only
            // for a row named by its key: one named by every value reads as
            // gone once an earlier write changed any of them.
            (StepKind::Delete, Some(0)) if step.by_key => {
                let Some(read) = &step.reread else {
                    roll_back(send);
                    return ApplyOutcome::Miscounted { step: index, count };
                };
                if !proceed() {
                    roll_back(send);
                    return ApplyOutcome::Failed {
                        stage: ApplyStage::Confirm(index),
                        error: cancelled(),
                    };
                }
                match send(read, Sending::Read) {
                    Ok(result) => result.rows.is_empty(),
                    Err(error) => {
                        roll_back(send);
                        return ApplyOutcome::Failed {
                            stage: ApplyStage::Confirm(index),
                            error,
                        };
                    }
                }
            }
            _ => false,
        };
        if !counted {
            roll_back(send);
            return ApplyOutcome::Miscounted { step: index, count };
        }
        affected += count.unwrap_or_default();
        if let Some(read) = &step.read_back {
            if !proceed() {
                roll_back(send);
                return ApplyOutcome::Failed {
                    stage: ApplyStage::ReadBack(index),
                    error: cancelled(),
                };
            }
            match send(read, Sending::Read) {
                Ok(result) => read_backs.push((index, result)),
                Err(error) => {
                    roll_back(send);
                    return ApplyOutcome::Failed {
                        stage: ApplyStage::ReadBack(index),
                        error,
                    };
                }
            }
        }
    }

    // A Cancel that came after the last write still stops the commit.
    if !proceed() {
        roll_back(send);
        return ApplyOutcome::Failed {
            stage: ApplyStage::Commit,
            error: cancelled(),
        };
    }
    if let Err(error) = send("COMMIT", Sending::Bracket) {
        if lost(&error) {
            return ApplyOutcome::Unknown(error);
        }
        roll_back(send);
        return ApplyOutcome::Failed {
            stage: ApplyStage::Commit,
            error,
        };
    }
    ApplyOutcome::Applied {
        affected,
        read_backs,
    }
}

/// Why the apply is refused before any re-read, when a step names its row by
/// every value after an earlier write that something on the server could
/// follow with a rewrite of that row's table ([`rewrites::probe`]): another
/// row moved onto exactly those values would be counted as the step's one.
/// A key names the same row however the others move, and a first step writes
/// before anything else could, so neither is asked about.
fn moved_under(
    engine: Engine,
    steps: &[VerifiedStep],
    send: &mut dyn FnMut(&str, Sending) -> Result<QueryResult, DbError>,
) -> Option<DbError> {
    let refused = || plain_error(KEYLESS_AFTER_A_WRITE.into());
    for (index, step) in steps.iter().enumerate().skip(1) {
        if step.by_key || !matches!(step.kind, StepKind::Update | StepKind::Delete) {
            continue;
        }
        let Some(later) = crate::sql::written_table(engine, &step.sql) else {
            return Some(refused());
        };
        let mut earlier: Vec<Vec<String>> = Vec::new();
        for before in &steps[..index] {
            let Some(table) = crate::sql::written_table(engine, &before.sql) else {
                return Some(refused());
            };
            if !earlier.contains(&table) {
                earlier.push(table);
            }
        }
        match rewrites::probe(engine, &later, &earlier) {
            rewrites::Probe::Never => {}
            rewrites::Probe::Unknown => return Some(refused()),
            rewrites::Probe::Ask(read) => match send(&read, Sending::Catalog) {
                Ok(found) if found.rows.is_empty() => {}
                Ok(_) => return Some(refused()),
                Err(error) => return Some(error),
            },
        }
    }
    None
}

/// Why a draft that writes a row of a table without a primary key after
/// another write is refused unsent, where a trigger, a rule or a cascading
/// foreign key could have moved rows under it.
pub(crate) const KEYLESS_AFTER_A_WRITE: &str = "Nothing was applied. A row of a table without \
     a primary key is named by its values, and a trigger or a cascading foreign key fired by an \
     earlier change of this draft could give another row those values. Apply its changes one \
     row at a time.";

/// Ends the transaction a verified apply opened. Its error is not the apply's
/// to report: the session it would have ended may be the thing that failed.
/// When it fails, the arm is asked to make sure the session holds nothing of
/// the apply's ([`Sending::Abandon`]), resetting it if it must, so a later
/// `COMMIT` of the user's cannot land the apply's writes.
fn roll_back(send: &mut dyn FnMut(&str, Sending) -> Result<QueryResult, DbError>) {
    if send("ROLLBACK", Sending::Bracket).is_err() {
        let _ = send("ROLLBACK", Sending::Abandon);
    }
}

pub(super) fn assemble_catalog(
    relations: QueryResult,
    routines: QueryResult,
) -> Result<Catalog, DbError> {
    let mut schemas = std::collections::BTreeMap::<String, Schema>::new();

    for row in &relations.rows {
        let schema_name = required_cell(&relations, row, "schema_name")?;
        let name = required_cell(&relations, row, "relation_name")?;
        let kind = match required_cell(&relations, row, "relation_kind")? {
            "table" => RelationKind::Table,
            "partitioned_table" => RelationKind::PartitionedTable,
            "view" => RelationKind::View,
            "materialized_view" => RelationKind::MaterializedView,
            "foreign_table" => RelationKind::ForeignTable,
            "dictionary" => RelationKind::Dictionary,
            kind => return Err(unexpected_catalog_value("relation kind", kind)),
        };

        schema(&mut schemas, schema_name).relations.push(Relation {
            name: name.to_string(),
            kind,
            partition_of: optional_cell(&relations, row, "partition_of").map(str::to_string),
            size: optional_cell(&relations, row, "size_bytes").and_then(|v| v.parse().ok()),
            rows: optional_cell(&relations, row, "row_estimate").and_then(|v| v.parse().ok()),
        });
    }

    for row in &routines.rows {
        let schema_name = required_cell(&routines, row, "schema_name")?;
        let name = required_cell(&routines, row, "routine_name")?;
        let kind = match required_cell(&routines, row, "routine_kind")? {
            "function" => RoutineKind::Function,
            "procedure" => RoutineKind::Procedure,
            kind => return Err(unexpected_catalog_value("routine kind", kind)),
        };

        schema(&mut schemas, schema_name).routines.push(Routine {
            name: name.to_string(),
            kind,
            identity_arguments: required_cell(&routines, row, "identity_arguments")?.to_string(),
            result_type: required_cell(&routines, row, "result_type")?.to_string(),
            language: required_cell(&routines, row, "language")?.to_string(),
            definition: required_cell(&routines, row, "definition")?.to_string(),
        });
    }

    Ok(Catalog {
        schemas: schemas.into_values().collect(),
        database: None,
        ..Default::default()
    })
}

/// A row with neither number (a null `DATA_LENGTH` and `TABLE_ROWS`) is left
/// out rather than failing the rest.
pub(super) fn assemble_sizes(result: QueryResult) -> Result<Sizes, DbError> {
    let mut sizes = Sizes::new();
    for row in &result.rows {
        let number = |column| optional_cell(&result, row, column).and_then(|v| v.parse().ok());
        let statistics = Statistics {
            size: number("size_bytes"),
            rows: number("row_estimate"),
        };
        if statistics == Statistics::default() {
            continue;
        }
        sizes
            .entry(required_cell(&result, row, "schema_name")?.to_string())
            .or_default()
            .insert(
                required_cell(&result, row, "relation_name")?.to_string(),
                statistics,
            );
    }
    Ok(sizes)
}

pub(super) fn assemble_structure(
    columns: QueryResult,
    indexes: QueryResult,
    constraints: QueryResult,
) -> Result<Structure, DbError> {
    let mut structure = Structure::default();

    for row in &columns.rows {
        let default = required_cell(&columns, row, "column_default")?;
        structure.columns.push(ColumnDefinition {
            name: required_cell(&columns, row, "column_name")?.to_string(),
            data_type: required_cell(&columns, row, "data_type")?.to_string(),
            nullable: match required_cell(&columns, row, "nullable")? {
                "yes" => true,
                "no" => false,
                value => return Err(unexpected_catalog_value("nullability", value)),
            },
            default: (!default.is_empty()).then(|| default.to_string()),
        });
    }

    for row in &indexes.rows {
        structure.indexes.push(Index {
            name: required_cell(&indexes, row, "object_name")?.to_string(),
            kind: match required_cell(&indexes, row, "index_kind")? {
                "primary" => IndexKind::Primary,
                "unique" => IndexKind::Unique,
                "plain" => IndexKind::Plain,
                value => return Err(unexpected_catalog_value("index kind", value)),
            },
            // Split on the unit separator rather than `, `, which an
            // expression such as `coalesce(a, b)` holds.
            columns: optional_cell(&indexes, row, "index_columns")
                .map(|columns| columns.split('\u{1f}').map(str::to_string).collect())
                .unwrap_or_default(),
            predicate: optional_cell(&indexes, row, "index_predicate").map(str::to_string),
            definition: required_cell(&indexes, row, "definition")?.to_string(),
        });
    }
    for row in &constraints.rows {
        structure.constraints.push(NamedDefinition {
            name: required_cell(&constraints, row, "object_name")?.to_string(),
            definition: required_cell(&constraints, row, "definition")?.to_string(),
        });
    }

    Ok(structure)
}

/// The sequences a schema export has to bring back with its tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sequences {
    /// `CREATE SEQUENCE`, written before the tables whose defaults name them.
    pub before_tables: Vec<String>,
    /// `ALTER TABLE … ALTER COLUMN … SET`, which gives an identity column's
    /// sequence the options its `CREATE TABLE` does not name, before any row.
    pub identity: Vec<String>,
    /// `ALTER SEQUENCE … OWNED BY`, which ties each to the column the
    /// structure creates.
    pub owned_by: Vec<String>,
    /// `setval`, which sets each where it stood when it was read.
    pub values: Vec<String>,
    /// Sequences the role may not read where they stand, by qualified name,
    /// which no `setval` can be written for.
    pub unreadable: Vec<String>,
}

/// A table's DDL for a schema export: `create` without the foreign keys an
/// engine adds after the rows, and those keys as statements of their own.
pub struct ExportDdl {
    pub create: String,
    pub foreign_keys: Vec<String>,
}

/// The structure without its foreign keys, and each as an `ALTER TABLE` that
/// adds it back once every table it might point at exists.
pub(super) fn split_foreign_keys(
    engine: Engine,
    schema: &str,
    relation: &str,
    structure: &Structure,
) -> (Structure, Vec<String>) {
    let (keys, rest): (Vec<_>, Vec<_>) = structure
        .constraints
        .iter()
        .cloned()
        .partition(|constraint| constraint.definition.starts_with("FOREIGN KEY"));
    let statements = keys
        .iter()
        .map(|key| {
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} {};",
                engine.qualified(schema, relation),
                engine.quote_identifier(&key.name),
                key.definition
            )
        })
        .collect();
    let structure = Structure {
        constraints: rest,
        ..structure.clone()
    };
    (structure, statements)
}

/// `{head} (columns, constraints){tail};` written back out of a loaded
/// structure, then each index as a statement of its own, spelled by `index`.
/// An index sharing a key or exclusion constraint's name is that constraint's,
/// already declared with it.
///
/// ponytail: a column's collation and anything else [`Structure`] does not
/// carry is not written; reading those from the catalog is the upgrade path.
pub(super) fn create_table(
    engine: Engine,
    head: &str,
    structure: &Structure,
    tail: &str,
    index: impl Fn(&Index) -> String,
) -> String {
    let columns = structure.columns.iter().map(|column| {
        let name = engine.quote_identifier(&column.name);
        let default = column.default.as_deref().unwrap_or_default();
        // SQL Server's computed column: an expression where the type would be.
        if let Some(expression) = default.strip_prefix("AS ") {
            return format!("{name} AS {expression}");
        }
        let generated = default.starts_with("GENERATED ") || default.starts_with("IDENTITY(");
        let default = match default {
            "" => String::new(),
            _ if generated => format!(" {default}"),
            _ => format!(" DEFAULT {default}"),
        };
        let not_null = if column.nullable { "" } else { " NOT NULL" };
        format!("{name} {}{default}{not_null}", column.data_type)
    });
    let constraints = structure.constraints.iter().map(|constraint| {
        format!(
            "CONSTRAINT {} {}",
            engine.quote_identifier(&constraint.name),
            constraint.definition
        )
    });
    let body = columns
        .chain(constraints)
        .collect::<Vec<_>>()
        .join(",\n    ");
    let mut statements = vec![format!("{head} (\n    {body}\n){tail}")];
    statements.extend(
        structure
            .indexes
            .iter()
            .filter(|definition| {
                !structure.constraints.iter().any(|constraint| {
                    constraint.name == definition.name
                        && ["PRIMARY KEY", "UNIQUE", "EXCLUDE"]
                            .iter()
                            .any(|kind| constraint.definition.starts_with(kind))
                })
            })
            .map(index),
    );
    statements.join(";\n") + ";"
}

/// A statement the server rendered, ending in exactly one `;` whether or not
/// the server wrote one.
pub(super) fn terminated(statement: &str) -> String {
    format!("{};", statement.trim().trim_end_matches(';'))
}

/// Reads foreign keys out of a result whose columns are named the way dbdelve
/// names them: `column_name`, `referenced_schema`, `referenced_table`,
/// `referenced_column`.
///
/// This is shared *parsing of a dbdelve-named result shape*, not shared dispatch.
/// Postgres and MySQL each write their own catalog query and each choose these
/// four aliases, so the row-to-struct step is the same work twice; SQLite does
/// not use this at all, because `PRAGMA foreign_key_list` reports a different
/// shape. No engine branches here and no engine has to route through it.
pub(super) fn assemble_foreign_keys(result: &QueryResult) -> Result<Vec<ForeignKey>, DbError> {
    result
        .rows
        .iter()
        .map(|row| {
            Ok(ForeignKey {
                column: required_cell(result, row, "column_name")?.to_string(),
                referenced_schema: required_cell(result, row, "referenced_schema")?.to_string(),
                referenced_table: required_cell(result, row, "referenced_table")?.to_string(),
                referenced_column: required_cell(result, row, "referenced_column")?.to_string(),
            })
        })
        .collect()
}

/// The rows of a reverse foreign-key query: `source_schema`, `source_table`,
/// `column_name`, `referenced_column` and `constraint_name`, one row per column
/// of each key. Keys of more than one column are dropped here, once, for every
/// engine that reads them this way.
pub(super) fn assemble_references(result: &QueryResult) -> Result<Vec<Reference>, DbError> {
    let mut keys: Vec<((String, String, String), Vec<Reference>)> = Vec::new();
    for row in &result.rows {
        let reference = Reference {
            schema: required_cell(result, row, "source_schema")?.to_string(),
            table: required_cell(result, row, "source_table")?.to_string(),
            column: required_cell(result, row, "column_name")?.to_string(),
            referenced_column: required_cell(result, row, "referenced_column")?.to_string(),
        };
        let name = (
            reference.schema.clone(),
            reference.table.clone(),
            required_cell(result, row, "constraint_name")?.to_string(),
        );
        match keys.iter_mut().find(|(key, _)| *key == name) {
            Some((_, columns)) => columns.push(reference),
            None => keys.push((name, vec![reference])),
        }
    }
    let mut references: Vec<Reference> = keys
        .into_iter()
        .filter_map(|(_, mut columns)| (columns.len() == 1).then(|| columns.remove(0)))
        .collect();
    references.dedup();
    Ok(references)
}

fn schema<'a>(
    schemas: &'a mut std::collections::BTreeMap<String, Schema>,
    name: &str,
) -> &'a mut Schema {
    schemas.entry(name.to_string()).or_insert_with(|| Schema {
        name: name.to_string(),
        relations: Vec::new(),
        routines: Vec::new(),
        sequences: Vec::new(),
    })
}

/// A `name, is_current` listing as each engine's databases query returns it.
/// The flag is whatever the engine calls true; NULL, as MySQL answers with no
/// database selected, is not current.
pub(super) fn assemble_databases(result: &QueryResult) -> Result<Databases, DbError> {
    let mut databases = Databases::default();
    for row in &result.rows {
        let name = required_cell(result, row, "name")?.to_string();
        if matches!(
            required_cell(result, row, "is_current"),
            Ok("1" | "t" | "true")
        ) {
            databases.current = Some(name.clone());
        }
        databases.names.push(name);
    }
    Ok(databases)
}

pub(super) fn required_cell<'a>(
    result: &'a QueryResult,
    row: &'a Row,
    column_name: &str,
) -> Result<&'a str, DbError> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == column_name)
        .ok_or_else(|| plain_error(format!("Catalog query omitted column {column_name}.")))?;

    row.get(index)
        .ok_or_else(|| plain_error(format!("Catalog query returned no {column_name}.")))
}

/// A catalog column an engine may have nothing to say about. A missing column
/// and a null read the same, so an engine without the concept says so by not
/// selecting it rather than by coalescing a placeholder.
fn optional_cell<'a>(result: &'a QueryResult, row: &'a Row, column_name: &str) -> Option<&'a str> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == column_name)?;

    row.get(index).filter(|value| !value.is_empty())
}

pub(super) fn unexpected_catalog_value(label: &str, value: &str) -> DbError {
    plain_error(format!("Catalog query returned unknown {label} {value}."))
}

pub(super) fn non_utf8_error(columns: &[Column], index: usize) -> DbError {
    let column = columns
        .get(index)
        .map(|column| format!("column {}", column.name))
        .unwrap_or_else(|| format!("column {index}"));

    plain_error(format!(
        "A value in {column} is not valid UTF-8 text and cannot be displayed."
    ))
}

/// `template` with `{schema}` and `{relation}` replaced by `schema` and
/// `relation` as `engine`'s string literals, in one pass over the template:
/// replacing one placeholder and then the other would reach into the first
/// name, and a schema called `a{relation}` would splice the relation's
/// literal into its own.
pub(super) fn structure_sql(
    engine: Engine,
    template: &str,
    schema: &str,
    relation: &str,
) -> String {
    structure_sql_quoted(template, schema, relation, |value| {
        engine.quote_literal(value)
    })
}

/// [`structure_sql`] with each name written by `quote`.
pub(super) fn structure_sql_quoted(
    template: &str,
    schema: &str,
    relation: &str,
    quote: impl Fn(&str) -> String,
) -> String {
    let mut sql = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        sql.push_str(&rest[..start]);
        rest = &rest[start..];
        let (placeholder, value) = if rest.starts_with("{schema}") {
            ("{schema}", schema)
        } else if rest.starts_with("{relation}") {
            ("{relation}", relation)
        } else {
            sql.push('{');
            rest = &rest[1..];
            continue;
        };
        sql.push_str(&quote(value));
        rest = &rest[placeholder.len()..];
    }
    sql.push_str(rest);
    sql
}

/// `value` as a Postgres string literal that reads the same whatever
/// `standard_conforming_strings` is. With it off, a backslash in a plain
/// `'…'` escapes the quote after it, so a name such as `x\' OR f() --` would
/// end the literal early; one holding a backslash is written `E'…'`, where a
/// backslash always escapes and is doubled. One without stays plain, which
/// both settings read alike.
pub(super) fn postgres_literal(value: &str) -> String {
    let quoted = value.replace('\'', "''");
    match value.contains('\\') {
        true => format!("E'{}'", quoted.replace('\\', r"\\")),
        false => format!("'{quoted}'"),
    }
}

/// What an apply says when the server reports a transaction already open on
/// the session it would run on: its own `BEGIN` and `COMMIT` would end it.
pub(super) const IN_A_TRANSACTION: &str =
    "A transaction is open on this connection; the edits were not applied.";

/// [`IN_A_TRANSACTION`] as the error of an apply's opening bracket.
pub(super) fn in_a_transaction() -> DbError {
    plain_error(IN_A_TRANSACTION.into())
}

pub(super) fn plain_error(message: String) -> DbError {
    DbError {
        message,
        position: None,
    }
}

/// What a connection with no arm of [`Connection::apply_verified`] answers:
/// nothing was sent.
fn unverifiable() -> ApplyOutcome {
    ApplyOutcome::Failed {
        stage: ApplyStage::Begin,
        error: plain_error(
            "The apply could not be verified on this connection, so nothing was sent.".into(),
        ),
    }
}

/// The statement that asks the server to hold this session to reads, or to
/// let go of that hold -- the server-side backstop behind `sql::gate`'s
/// client-side one. `None` where the engine has no session-level switch to
/// send it to.
///
/// SQLite has none: `OpenFlags::SQLITE_OPEN_READ_WRITE` (sqlite.rs:91) fixes
/// read/write at open time, and `Connection::set_read_only` is a live flip
/// with no reconnect behind it.
///
/// ponytail: SQLite stays open-mode-only rather than reopening the file on a
/// mode change. Upgrade path if SQLite read-only ever needs enforcing:
/// reopen the file under `SQLITE_OPEN_READ_ONLY` when the mode lands on
/// `ReadOnly`.
fn read_only_statement(engine: Engine, read_only: bool) -> Option<&'static str> {
    match (engine.spec().sql.read_only_session, read_only) {
        (ReadOnlySession::DefaultTransaction, true) => {
            Some("SET default_transaction_read_only = on")
        }
        (ReadOnlySession::DefaultTransaction, false) => {
            Some("SET default_transaction_read_only = off")
        }
        (ReadOnlySession::SessionTransaction, true) => Some("SET SESSION TRANSACTION READ ONLY"),
        (ReadOnlySession::SessionTransaction, false) => Some("SET SESSION TRANSACTION READ WRITE"),
        // MongoDB and Redis land here too: no session setting holds a client
        // to reads; the classifier is the boundary there.
        (ReadOnlySession::None, _) => None,
        // Carried by each request's settings, not set on the session.
        (ReadOnlySession::PerRequest, _) => None,
    }
}

/// The statement that sets this session's time zone, or `None` where the
/// engine has no session statement for it: ClickHouse sends the zone as a
/// setting on each request instead, and the rest take none from a profile.
///
/// `zone` has passed [`check_time_zone`] -- the form and the restore both ask
/// it -- and is quoted here as well, so a zone that slipped past still cannot
/// end the literal.
fn time_zone_statement(engine: Engine, zone: &str) -> Option<String> {
    let spec = engine.spec();
    let literal = spec.quote_literal(zone);
    match spec.sql.session_time_zone {
        SessionTimeZone::SetTimeZone => Some(format!("SET TIME ZONE {literal}")),
        SessionTimeZone::TimeZoneVariable => Some(format!("SET time_zone = {literal}")),
        SessionTimeZone::None | SessionTimeZone::RequestSetting => None,
    }
}

/// Shared with `postgres::tests`, which exercises `assemble`'s type-matching
/// against the same synthetic result shape.
#[cfg(test)]
pub(super) fn result(columns: &[&str], rows: &[&[Option<&str>]]) -> QueryResult {
    QueryResult {
        columns: columns
            .iter()
            .map(|name| Column {
                name: (*name).to_string(),
                ..Default::default()
            })
            .collect(),
        rows: rows
            .iter()
            .map(|row| Row::new(row.iter().copied()).unwrap())
            .collect(),
        ..Default::default()
    }
}

/// The live checks every engine's `live_` tests make of a queue run at once,
/// shared because they are one claim about [`Connection::query_alongside`]:
/// the `Workspace` around it needs a window, and these need a server.
#[cfg(test)]
pub(super) mod at_once {
    use std::time::{Duration, Instant};

    use super::{CancelToken, Connection, DbError, Engine, Fetch, QueryResult, Target, is_cancel};
    use crate::{session::runs_at_once, sql::Mode};

    /// Each statement on a connection of its own, all at once, as
    /// `Workspace::run_at_once` sends a queue: how long the lot took, and each
    /// one's outcome in order.
    pub(in crate::db) fn run(
        connection: &Connection,
        statements: &[&str],
        cancel: &CancelToken,
    ) -> (Duration, Vec<Result<QueryResult, DbError>>) {
        let started = Instant::now();
        let outcomes = std::thread::scope(|scope| {
            let running: Vec<_> = statements
                .iter()
                .map(|sql| {
                    scope.spawn(|| connection.query_alongside(sql, cancel, Fetch::default(), false))
                })
                .collect();
            running
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .expect("the statement's thread should not panic")
                })
                .collect()
        });
        (started.elapsed(), outcomes)
    }

    /// Two slow reads run at once take about the time of one: well under what
    /// they take one after the other on the profile's connection. A queue
    /// with a write in it is not run at once at all.
    pub(in crate::db) fn reads_overlap(
        connection: &Connection,
        engine: Engine,
        slow: &str,
        write: &str,
    ) {
        let pair = [slow, slow];
        assert!(runs_at_once(engine, Mode::ReadWrite, &pair));
        assert!(!runs_at_once(engine, Mode::Full, &[slow, write]));

        let started = Instant::now();
        for sql in pair {
            connection
                .query(sql, &CancelToken::default(), Fetch::default())
                .expect("the read should run");
        }
        let in_turn = started.elapsed();
        let (at_once, outcomes) = run(connection, &pair, &CancelToken::alongside());
        for outcome in outcomes {
            outcome.expect("the read should run at once with the other");
        }
        eprintln!("{engine:?}: two reads in turn {in_turn:?}, at once {at_once:?}");
        assert!(
            at_once.as_secs_f64() < in_turn.as_secs_f64() * 0.8,
            "at once {at_once:?} against {in_turn:?} in turn"
        );
    }

    /// Wait for `count` statements under `cancel` to hold a connection of
    /// their own, and a moment more for them to reach the server. A fixed
    /// wait loses to a slow connect, and the cancel then lands before
    /// anything was sent.
    fn connected(cancel: &CancelToken, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline
            && cancel.0.lock().map_or(0, |running| {
                running
                    .alongside
                    .as_ref()
                    .map_or(0, |alongside| alongside.iter().flatten().count())
                    + running.handles.len()
            }) < count
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    /// A cancel under the run's token stops every statement running at once,
    /// each on its own connection, and leaves the profile's connection be.
    pub(in crate::db) fn a_cancel_stops_them_all(
        connection: &Connection,
        engine: Engine,
        runaway: &str,
    ) {
        let cancel = CancelToken::alongside();
        let (elapsed, outcomes) = std::thread::scope(|scope| {
            scope.spawn(|| {
                connected(&cancel, 2);
                connection
                    .cancel(&cancel)
                    .expect("the cancel should reach each connection");
            });
            run(connection, &[runaway, runaway], &cancel)
        });
        eprintln!("{engine:?}: two runaways cancelled after {elapsed:?}");
        for outcome in outcomes {
            let error = outcome.expect_err("the statement should have been stopped");
            assert!(
                is_cancel(Target::built_in(engine), &error),
                "{}",
                error.message
            );
        }
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        connection
            .query("SELECT 1", &CancelToken::default(), Fetch::default())
            .expect("the profile's own connection should be untouched");
    }

    /// A cancel under one statement's own token stops that statement alone:
    /// the one beside it, under a token of its own, runs to its end, as a
    /// Cancel on one chip of a queue run at once leaves the others.
    pub(in crate::db) fn a_cancel_stops_only_its_own(
        connection: &Connection,
        engine: Engine,
        runaway: &str,
        slow: &str,
    ) {
        let (theirs, mine) = (CancelToken::alongside(), CancelToken::alongside());
        let (stopped, finished) = std::thread::scope(|scope| {
            let stopped = scope
                .spawn(|| connection.query_alongside(runaway, &theirs, Fetch::default(), false));
            let finished =
                scope.spawn(|| connection.query_alongside(slow, &mine, Fetch::default(), false));
            connected(&theirs, 1);
            connected(&mine, 1);
            connection
                .cancel(&theirs)
                .expect("the cancel should reach its connection");
            let join = |thread: std::thread::ScopedJoinHandle<'_, _>| {
                thread
                    .join()
                    .expect("the statement's thread should not panic")
            };
            (join(stopped), join(finished))
        });
        let error = stopped.expect_err("the statement cancelled should have been stopped");
        assert!(
            is_cancel(Target::built_in(engine), &error),
            "{}",
            error.message
        );
        finished.expect("the statement beside it should run to its end");
    }
}

/// Every child `src/db/` starts goes through here. macOS has no `pipe2`: std
/// creates a child's pipes and only then marks them close-on-exec, so a child
/// another thread forks in between inherits them for life. A plugin whose
/// stdout an ssh watchdog holds never reads as closed, and a watchdog whose
/// stdin a plugin holds never sees DBDelve go.
///
/// The lock cannot reach a spawn inside a dependency, so it narrows the race
/// rather than closing it: the defence is that a plugin's death is read from
/// its exit, not from its stdout closing (`external::host`).
pub(crate) fn spawn(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    static SPAWNING: Mutex<()> = Mutex::new(());
    let _alone = SPAWNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mysql_and_mariadb_commit_an_open_transaction_at_a_begin() {
        for engine in [Engine::MySql, Engine::MariaDb] {
            assert!(engine.begin_ends_open_transaction(), "{engine:?}");
        }
        for engine in [
            Engine::Postgres,
            Engine::Sqlite,
            Engine::Turso,
            Engine::DuckDb,
            Engine::SqlServer,
        ] {
            assert!(!engine.begin_ends_open_transaction(), "{engine:?}");
        }
    }

    #[test]
    fn only_sqlite_keeps_a_transaction_whose_end_failed_and_not_when_none_was_active() {
        assert!(Engine::Sqlite.failed_end_keeps_transaction("database is locked"));
        assert!(
            !Engine::Sqlite
                .failed_end_keeps_transaction("cannot commit - no transaction is active")
        );
        assert!(
            !Engine::Sqlite
                .failed_end_keeps_transaction("cannot rollback - no transaction is active")
        );
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Turso,
            Engine::DuckDb,
            Engine::SqlServer,
            Engine::Snowflake,
        ] {
            assert!(
                !engine.failed_end_keeps_transaction("could not commit"),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn a_time_zone_with_a_quote_is_refused() {
        for zone in [
            "UTC",
            "SYSTEM",
            "Europe/Paris",
            "America/Port-au-Prince",
            "Etc/GMT+5",
            "+05:30",
        ] {
            assert_eq!(check_time_zone(zone), Ok(()), "{zone}");
        }
        let long = "a".repeat(65);
        for zone in ["Europe/Paris'", "a b", "UTC;", r"a\b", long.as_str()] {
            let message = check_time_zone(zone).expect_err(zone);
            assert!(
                message.contains(zone) || message.contains("64"),
                "names the value or the limit: {message}"
            );
        }
        assert!(check_time_zone("").is_err() && check_time_zone("   ").is_err());
    }

    #[test]
    fn a_cancel_before_or_between_a_graph_reads_statements_holds_back_the_rest() {
        let graph = GraphCancel::default();
        graph.ask();
        assert_eq!(graph.read(|held| [held(), held()]), [false, false]);
        assert!(
            graph.read(|held| held()),
            "none outlives the read it was for"
        );
        let sent = graph.read(|held| {
            let first = held();
            graph.ask();
            [first, held()]
        });
        assert_eq!(sent, [true, false]);
        assert!(graph.read(|held| held()));
    }

    #[test]
    fn a_write_that_reports_no_count_is_miscounted_and_rolled_back() {
        let step = VerifiedStep {
            sql: r#"UPDATE "main"."t" SET "a" = 'x' WHERE "id" = '1'"#.into(),
            kind: StepKind::Update,
            reread: Some(r#"SELECT * FROM "main"."t" WHERE "id" = '1' LIMIT 2"#.into()),
            before: vec![fetched("id", Some("1"))],
            read_back: None,
            by_key: true,
        };
        let mut sent = Vec::new();
        let mut send = |sql: &str, _: Sending| {
            sent.push(sql.to_string());
            Ok(match sql.starts_with("SELECT") {
                true => reread(&["id"], &[&[Some("1")]]),
                false => QueryResult::default(),
            })
        };
        assert_eq!(
            run_verified(
                Engine::Sqlite,
                &[step],
                &mut send,
                &|| true,
                &|_| false,
                &|_| false
            ),
            ApplyOutcome::Miscounted {
                step: 0,
                count: None
            }
        );
        assert_eq!(sent.first().map(String::as_str), Some("BEGIN"));
        assert_eq!(sent.last().map(String::as_str), Some("ROLLBACK"));
        assert!(!sent.iter().any(|sql| sql == "COMMIT"), "{sent:?}");
    }

    fn update_step() -> VerifiedStep {
        VerifiedStep {
            sql: r#"UPDATE "public"."t" SET "a" = 'x' WHERE "id" = '1'"#.into(),
            kind: StepKind::Update,
            reread: Some(r#"SELECT * FROM "public"."t" WHERE "id" = '1' LIMIT 2"#.into()),
            before: vec![fetched("id", Some("1"))],
            read_back: None,
            by_key: true,
        }
    }

    /// A step's re-read and read-back must be the very reads cut from its own
    /// statement: a read of another row, though a read dbdelve could have
    /// written, is refused before anything is sent.
    #[test]
    fn a_read_of_another_row_than_the_steps_own_is_refused() {
        let sent = std::cell::Cell::new(0);
        let mut send = |_: &str, _: Sending| {
            sent.set(sent.get() + 1);
            Ok(QueryResult::default())
        };
        let own = update_step();
        let other = crate::sql::row_reread(
            Engine::Postgres,
            r#"UPDATE "public"."t" SET "a" = 'x' WHERE "id" = '2'"#,
        );
        assert!(other.is_some());
        let steps = [
            VerifiedStep {
                reread: other.clone(),
                ..own.clone()
            },
            VerifiedStep {
                read_back: other,
                ..own.clone()
            },
        ];
        for step in steps {
            let outcome = run_verified(
                Engine::Postgres,
                &[step],
                &mut send,
                &|| true,
                &|_| false,
                &|_| false,
            );
            assert!(
                matches!(
                    outcome,
                    ApplyOutcome::Failed {
                        stage: ApplyStage::Begin,
                        ..
                    }
                ),
                "{outcome:?}"
            );
        }
        assert_eq!(sent.get(), 0, "nothing was sent");
    }

    /// A `ROLLBACK` that fails is followed by the arm's abandon, which ends
    /// or resets what the apply left open; one that succeeds is not.
    #[test]
    fn a_failed_rollback_asks_the_arm_to_abandon_the_transaction() {
        let run = |rollback_fails: bool| {
            let mut sent = Vec::new();
            let mut send = |sql: &str, sending: Sending| {
                sent.push((sql.to_string(), sending));
                match sql.split_whitespace().next() {
                    Some("SELECT") => Ok(reread(&["id"], &[&[Some("2")]])),
                    Some("ROLLBACK") if rollback_fails && sending == Sending::Bracket => {
                        Err(plain_error("the connection closed".into()))
                    }
                    _ => Ok(QueryResult::default()),
                }
            };
            let outcome = run_verified(
                Engine::Postgres,
                &[update_step()],
                &mut send,
                &|| true,
                &|_| false,
                &|_| false,
            );
            assert!(matches!(outcome, ApplyOutcome::Drifted(_)), "{outcome:?}");
            sent.into_iter()
                .filter(|(_, sending)| *sending == Sending::Abandon)
                .count()
        };
        assert_eq!(run(true), 1);
        assert_eq!(run(false), 0);
    }

    /// A panic part way through the sequence rolls the apply back before it
    /// goes on unwinding, so the session is not left inside its transaction.
    #[test]
    fn a_panic_part_way_rolls_the_apply_back_and_still_panics() {
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut send = |sql: &str, _: Sending| {
                sent.borrow_mut().push(sql.to_string());
                match sql.split_whitespace().next() {
                    Some("SELECT") => Ok(reread(&["id"], &[&[Some("1")]])),
                    Some("UPDATE") => panic!("the driver panicked"),
                    _ => Ok(QueryResult::default()),
                }
            };
            run_verified(
                Engine::Postgres,
                &[update_step()],
                &mut send,
                &|| true,
                &|_| false,
                &|_| false,
            )
        }));
        assert!(panicked.is_err(), "the panic is not swallowed");
        assert_eq!(sent.borrow().last().map(String::as_str), Some("ROLLBACK"));
    }

    /// A `DELETE` that counts none is taken as its row already gone only when
    /// the row is named by its key: a row named by every value reads as gone
    /// once an earlier write changed one of them, with the row still there.
    #[test]
    fn a_delete_counting_none_is_gone_only_for_a_row_named_by_its_key() {
        let run = |by_key: bool| {
            let step = VerifiedStep {
                sql: r#"DELETE FROM "public"."t" WHERE "code" = 'b' AND "parent" = 'a'"#.into(),
                kind: StepKind::Delete,
                reread: Some(
                    r#"SELECT * FROM "public"."t" WHERE "code" = 'b' AND "parent" = 'a' LIMIT 2"#
                        .into(),
                ),
                before: vec![fetched("code", Some("b"))],
                read_back: None,
                by_key,
            };
            let reads = std::cell::Cell::new(0);
            let mut send = |sql: &str, _: Sending| {
                Ok(match sql.split_whitespace().next() {
                    // The first read finds the row; by the second an earlier
                    // write has changed its `parent`, so it matches none.
                    Some("SELECT") => {
                        reads.set(reads.get() + 1);
                        match reads.get() {
                            1 => reread(&["code"], &[&[Some("b")]]),
                            _ => reread(&["code"], &[]),
                        }
                    }
                    Some("DELETE") => QueryResult {
                        rows_affected: Some(0),
                        ..QueryResult::default()
                    },
                    _ => QueryResult::default(),
                })
            };
            run_verified(
                Engine::Postgres,
                &[step],
                &mut send,
                &|| true,
                &|_| false,
                &|_| false,
            )
        };
        assert_eq!(
            run(false),
            ApplyOutcome::Miscounted {
                step: 0,
                count: Some(0)
            }
        );
        assert!(matches!(
            run(true),
            ApplyOutcome::Applied { affected: 0, .. }
        ));
    }

    #[test]
    fn a_cancel_after_the_last_write_rolls_back_rather_than_committing() {
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let mut send = |sql: &str, _: Sending| {
            sent.borrow_mut().push(sql.to_string());
            Ok(match sql.starts_with("SELECT") {
                true => reread(&["id"], &[&[Some("1")]]),
                false => QueryResult {
                    rows_affected: Some(1),
                    ..QueryResult::default()
                },
            })
        };
        // A Cancel comes once the write has gone out.
        let proceed = || !sent.borrow().iter().any(|sql| sql.starts_with("UPDATE"));
        let outcome = run_verified(
            Engine::Postgres,
            &[update_step()],
            &mut send,
            &proceed,
            &|_| false,
            &|_| false,
        );
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Commit,
                error: plain_error("Cancelled before it was sent.".into()),
            }
        );
        let sent = sent.into_inner();
        assert_eq!(sent.last().map(String::as_str), Some("ROLLBACK"));
        assert!(!sent.iter().any(|sql| sql == "COMMIT"), "{sent:?}");
    }

    #[test]
    fn a_cancel_between_statements_rolls_the_sequence_back() {
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let mut send = |sql: &str, _: Sending| {
            sent.borrow_mut().push(sql.to_string());
            Ok(match sql.starts_with("SELECT") {
                true => reread(&["id"], &[&[Some("1")]]),
                false => QueryResult::default(),
            })
        };
        // A Cancel comes once the re-read has gone out.
        let proceed = || !sent.borrow().iter().any(|sql| sql.starts_with("SELECT"));
        let outcome = run_verified(
            Engine::Postgres,
            &[update_step()],
            &mut send,
            &proceed,
            &|_| false,
            &|_| false,
        );
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Write(0),
                error: plain_error("Cancelled before it was sent.".into()),
            }
        );
        assert_eq!(
            sent.into_inner(),
            [
                "BEGIN",
                r#"SELECT * FROM "public"."t" WHERE "id" = '1' LIMIT 2"#,
                "ROLLBACK"
            ],
            "no write went out, and the transaction the apply opened was ended"
        );

        // A write the server stopped while it ran fails at that write.
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let mut send = |sql: &str, _: Sending| {
            sent.borrow_mut().push(sql.to_string());
            match sql.split_whitespace().next() {
                Some("SELECT") => Ok(reread(&["id"], &[&[Some("1")]])),
                Some("UPDATE") => Err(plain_error(
                    "canceling statement due to user request".into(),
                )),
                _ => Ok(QueryResult::default()),
            }
        };
        let outcome = run_verified(
            Engine::Postgres,
            &[update_step()],
            &mut send,
            &|| true,
            &|_| false,
            &|_| false,
        );
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Write(0),
                error: plain_error("canceling statement due to user request".into()),
            }
        );
        assert_eq!(sent.borrow().last().map(String::as_str), Some("ROLLBACK"));
    }

    #[test]
    fn a_lost_session_at_commit_is_an_unknown_outcome_and_before_it_is_not_applied() {
        // Answers every statement but `failing`, which gets the error.
        let run = |failing: &str, lost: bool| {
            let sent = std::cell::RefCell::new(Vec::<String>::new());
            let mut send = |sql: &str, _: Sending| {
                sent.borrow_mut().push(sql.to_string());
                match sql {
                    _ if sql.starts_with(failing) => {
                        Err(plain_error("the connection closed".into()))
                    }
                    _ if sql.starts_with("SELECT") => Ok(reread(&["id"], &[&[Some("1")]])),
                    _ if sql.starts_with("UPDATE") => Ok(QueryResult {
                        rows_affected: Some(1),
                        ..QueryResult::default()
                    }),
                    _ => Ok(QueryResult::default()),
                }
            };
            let outcome = run_verified(
                Engine::Postgres,
                &[update_step()],
                &mut send,
                &|| true,
                &|_| false,
                &|_| lost,
            );
            (outcome, sent.into_inner())
        };
        let error = || plain_error("the connection closed".into());

        let (outcome, _) = run("SELECT", true);
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Reread(0),
                error: error()
            },
            "a lost session before the write is not applied"
        );
        let (outcome, _) = run("UPDATE", true);
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Write(0),
                error: error()
            },
            "a lost session at the write is not applied either"
        );
        let (outcome, sent) = run("COMMIT", true);
        assert_eq!(outcome, ApplyOutcome::Unknown(error()));
        assert_eq!(
            sent.last().map(String::as_str),
            Some("COMMIT"),
            "nothing is sent after a COMMIT whose answer did not come"
        );
        let (outcome, sent) = run("COMMIT", false);
        assert_eq!(
            outcome,
            ApplyOutcome::Failed {
                stage: ApplyStage::Commit,
                error: error()
            },
            "a COMMIT the server refused on a live session was not applied"
        );
        assert_eq!(sent.last().map(String::as_str), Some("ROLLBACK"));
    }

    #[test]
    fn only_the_engines_with_an_arm_verify_an_apply() {
        for engine in Engine::ALL {
            assert_eq!(
                engine.verifies_applies(),
                matches!(
                    engine,
                    Engine::Sqlite
                        | Engine::DuckDb
                        | Engine::Postgres
                        | Engine::MySql
                        | Engine::MariaDb
                        | Engine::SqlServer
                        | Engine::MongoDb
                ),
                "{engine:?}"
            );
        }
    }

    fn fetched(column: &str, text: Option<&str>) -> FetchedCell {
        FetchedCell {
            column: column.into(),
            text: text.map(Into::into),
            cell_type: None,
            column_type: None,
        }
    }

    fn reread(columns: &[&str], rows: &[&[Option<&str>]]) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: (*name).into(),
                    data_type: None,
                })
                .collect(),
            rows: rows
                .iter()
                .map(|cells| Row::new(cells.iter().copied()).unwrap())
                .collect(),
            ..QueryResult::default()
        }
    }

    #[test]
    fn a_fetched_row_matches_its_reread_by_column_name_and_a_changed_or_missing_one_is_drift() {
        let before = [fetched("id", Some("7")), fetched("name", Some("wren"))];
        let same = |result: &QueryResult| drift(&before, result, |_| false);

        assert_eq!(
            same(&reread(&["id", "name"], &[&[Some("7"), Some("wren")]])),
            None
        );
        assert_eq!(
            same(&reread(&["name", "id"], &[&[Some("wren"), Some("7")]])),
            None,
            "another column order is the same row"
        );
        assert_eq!(
            same(&reread(
                &["id", "name", "added_since"],
                &[&[Some("7"), Some("wren"), Some("x")]]
            )),
            None,
            "a column the manifest does not hold is not compared"
        );
        assert_eq!(
            drift(
                &[fetched("note", None)],
                &reread(&["note"], &[&[None]]),
                |_| false
            ),
            None,
            "NULL equals NULL"
        );
        // Whole, byte for byte: no trimming, no case folding.
        for changed in ["wren ", "Wren", "", "wrens"] {
            assert_eq!(
                same(&reread(&["id", "name"], &[&[Some("7"), Some(changed)]])),
                Some(Drift::Changed),
                "{changed:?}"
            );
        }
        assert_eq!(
            same(&reread(&["id", "name"], &[&[Some("7"), None]])),
            Some(Drift::Changed),
            "a value turned NULL"
        );
        assert_eq!(
            drift(
                &[fetched("note", None)],
                &reread(&["note"], &[&[Some("")]]),
                |_| false
            ),
            Some(Drift::Changed),
            "a NULL turned the empty string"
        );
        assert_eq!(
            same(&reread(&["id"], &[&[Some("7")]])),
            Some(Drift::Changed),
            "a manifest column the re-read lacks"
        );
        assert_eq!(same(&reread(&["id", "name"], &[])), Some(Drift::Gone));
        assert_eq!(
            same(&reread(
                &["id", "name"],
                &[&[Some("7"), Some("wren")], &[Some("7"), Some("wren")]]
            )),
            Some(Drift::Ambiguous(2))
        );

        // A cell the arm's predicate names is not compared.
        let spatial = [
            fetched("id", Some("7")),
            fetched("geom", Some("POINT(1 2)")),
        ];
        assert_eq!(
            drift(
                &spatial,
                &reread(&["id", "geom"], &[&[Some("7"), Some("0101")]]),
                |cell| cell.column == "geom"
            ),
            None
        );

        // Where both sides type the cell, a different type is a different value.
        let typed = FetchedCell {
            cell_type: Some("int".into()),
            ..fetched("n", Some("1"))
        };
        let mut result = reread(&["n"], &[&[Some("1")]]);
        result.cell_types = vec![vec!["double"]];
        assert_eq!(
            drift(std::slice::from_ref(&typed), &result, |_| false),
            Some(Drift::Changed)
        );
        result.cell_types = vec![vec!["int"]];
        assert_eq!(
            drift(std::slice::from_ref(&typed), &result, |_| false),
            None
        );
        result.cell_types = Vec::new();
        assert_eq!(
            drift(std::slice::from_ref(&typed), &result, |_| false),
            None,
            "an untyped side is not compared by type"
        );
    }

    #[test]
    fn create_table_leaves_foreign_keys_for_after_the_rows() {
        let named = |name: &str, definition: &str| NamedDefinition {
            name: name.into(),
            definition: definition.into(),
        };
        let structure = Structure {
            constraints: vec![
                named("t_pkey", "PRIMARY KEY (id)"),
                named(
                    "t_parent_fk",
                    "FOREIGN KEY (parent_id) REFERENCES \"s\".\"p\"(id)",
                ),
            ],
            ..Structure::default()
        };

        let (rest, keys) = split_foreign_keys(Engine::Postgres, "s", "t", &structure);

        assert_eq!(rest.constraints, vec![named("t_pkey", "PRIMARY KEY (id)")]);
        assert_eq!(
            keys,
            vec![
                "ALTER TABLE \"s\".\"t\" ADD CONSTRAINT \"t_parent_fk\" \
                 FOREIGN KEY (parent_id) REFERENCES \"s\".\"p\"(id);"
            ]
        );
    }

    #[test]
    fn a_bounded_feed_holds_a_push_past_its_bound_until_rows_are_taken() {
        let feed = Feed::bounded(2);
        feed.begin(vec![Column {
            name: "n".into(),
            data_type: None,
        }]);
        let row = |n: &str| Row::new([Some(n)]).unwrap();
        feed.push(row("1"));
        feed.push(row("2"));
        let (pushed, done) = std::sync::mpsc::channel();
        let pusher = std::thread::spawn({
            let feed = feed.clone();
            move || {
                feed.push(row("3"));
                _ = pushed.send(());
            }
        });
        assert!(
            done.recv_timeout(Duration::from_millis(5)).is_err(),
            "a push past the bound went through untaken"
        );
        assert_eq!(feed.take().rows.len(), 2);
        done.recv_timeout(Duration::from_secs(5))
            .expect("a take makes room for the push");
        pusher.join().unwrap();
        assert_eq!(feed.take().rows.len(), 1);
    }

    #[test]
    fn system_databases_are_listed_after_the_users_own_in_server_order() {
        let listed = |engine, names: &[&str]| {
            let mut databases = Databases {
                names: names.iter().map(|name| (*name).to_string()).collect(),
                current: None,
                left_out: 0,
            };
            databases.system_last(engine);
            databases.names
        };
        assert_eq!(
            listed(
                Engine::MySql,
                &["app", "information_schema", "mysql", "orders", "sys"]
            ),
            ["app", "orders", "information_schema", "mysql", "sys"]
        );
        assert_eq!(
            listed(
                Engine::SqlServer,
                &["master", "model", "msdb", "sales", "tempdb"]
            ),
            ["sales", "master", "model", "msdb", "tempdb"]
        );
        assert_eq!(
            listed(Engine::MongoDb, &["admin", "config", "local", "shop"]),
            ["shop", "admin", "config", "local"]
        );
        // `postgres` is a database people work in.
        assert_eq!(
            listed(Engine::Postgres, &["app", "postgres", "zeta"]),
            ["app", "postgres", "zeta"]
        );
        assert_eq!(
            listed(
                Engine::Snowflake,
                &["ANALYTICS", "SNOWFLAKE", "SNOWFLAKE_SAMPLE_DATA"]
            ),
            ["ANALYTICS", "SNOWFLAKE_SAMPLE_DATA", "SNOWFLAKE"]
        );
        assert!(Engine::MariaDb.is_system_database("performance_schema"));
        assert!(!Engine::Postgres.is_system_database("mysql"));
        assert!(!Engine::Redis.is_system_database("db0"));
    }

    #[test]
    fn every_engine_answers_the_import_questions() {
        use Engine::*;
        for (engine, yes, skips, begin, new_fields) in [
            (Postgres, "true", true, Ok("BEGIN"), false),
            (MySql, "1", true, Ok("BEGIN"), false),
            (MariaDb, "1", true, Ok("BEGIN"), false),
            (Sqlite, "1", true, Ok("BEGIN"), false),
            (Turso, "1", true, Ok("BEGIN"), false),
            (DuckDb, "true", true, Ok("BEGIN"), false),
            (SqlServer, "1", false, Ok("BEGIN TRANSACTION"), false),
            (Snowflake, "true", false, Err(()), false),
            (MongoDb, "true", false, Err(()), true),
        ] {
            assert_eq!(engine.boolean_literal(true), yes);
            assert_eq!(engine.skips_duplicates(), skips);
            assert_eq!(engine.all_or_nothing().map_err(|_| ()), begin);
            assert_eq!(engine.takes_new_fields(), new_fields);
        }
    }

    #[test]
    fn a_packed_row_holds_what_its_cells_did() {
        let cells: Vec<Cell> = vec![
            Some("1".into()),
            None,
            Some(String::new()),
            Some("ü".into()),
        ];
        let mut row = Row::try_from(cells.clone()).unwrap();
        assert_eq!(row, cells);
        assert_eq!(row.get(1), None, "NULL");
        assert_eq!(row.get(2), Some(""), "an empty string, not NULL");
        assert_eq!(row.get(4), None, "past the end");
        assert_eq!(row.text_len(), 3);

        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            serde_json::to_string(&cells).unwrap(),
            "a snapshot reads as it did",
        );
        assert_eq!(
            serde_json::from_str::<Row>(r#"["1",null,"","ü"]"#).unwrap(),
            row
        );

        row.replace(&[(0, None), (1, Some("two")), (9, Some("x"))]);
        let edited = vec![
            None,
            Some("two".into()),
            Some(String::new()),
            Some("ü".into()),
        ];
        assert_eq!(row, edited);
        assert_eq!(row, Row::try_from(edited).unwrap(), "one spelling per row");
    }

    #[test]
    fn only_the_cancel_itself_reads_as_a_cancel() {
        let error = |message: &str| DbError {
            message: message.into(),
            position: None,
        };
        let reset = format!("{} The connection was reset.", mssql::CANCELLED);
        for (engine, words) in [
            (Engine::Postgres, "canceling statement due to user request"),
            (Engine::MySql, "Query execution was interrupted"),
            (Engine::MariaDb, "Query execution was interrupted"),
            (Engine::Sqlite, "interrupted"),
            (Engine::Snowflake, "SQL execution canceled"),
            (Engine::SqlServer, reset.as_str()),
        ] {
            assert!(
                is_cancel(Target::built_in(engine), &error(words)),
                "{engine:?}"
            );
            assert!(
                !is_cancel(Target::built_in(engine), &error("division by zero")),
                "{engine:?}"
            );
        }
        // The statement had finished: a whole result, not a cancelled one.
        assert!(!is_cancel(
            Target::built_in(Engine::SqlServer),
            &error("The statement finished, but Cancel arrived after that.")
        ));
        assert!(!is_cancel(
            Target::built_in(Engine::MariaDb),
            &error("Query execution was interrupted (max_statement_time exceeded)")
        ));
    }

    #[test]
    fn a_partly_applied_insert_many_says_how_many_writes_it_kept() {
        let error = |message: String| DbError {
            message,
            position: None,
        };
        let duplicate = "E11000 duplicate key error";
        let partly = error(format!("{duplicate}{}299.", mongo::APPLIED));
        assert_eq!(applied(Engine::MongoDb, &partly), 299);
        assert_eq!(applied(Engine::MongoDb, &error(duplicate.into())), 0);
        assert_eq!(applied(Engine::Postgres, &partly), 0);
    }

    #[test]
    fn every_buffer_is_highlighted_by_a_grammar_the_editor_was_built_with() {
        // An unknown name falls back to plain text without a word, so a
        // missing gpui-component feature would only show as a grey buffer.
        use gpui_component::highlighter::Language;
        for engine in Engine::ALL {
            let name = engine.syntax().highlighter();
            assert_ne!(Language::from_str(name), Language::Plain, "{name}");
        }
        assert_eq!(Engine::MongoDb.syntax(), Syntax::Mongo);
        assert_eq!(Engine::SqlServer.syntax(), Syntax::Sql);
    }

    #[test]
    fn only_an_engine_naming_rows_by_every_value_reviews_a_query_tabs_edits() {
        for engine in Engine::ALL {
            let clickhouse = engine == Engine::ClickHouse;
            assert_eq!(engine.reviews_query_edits(), clickhouse, "{engine:?}");
            assert_eq!(
                Target::built_in(engine).edits_query_rows(),
                clickhouse || engine == Engine::Redis,
                "{engine:?}"
            );
        }
    }

    #[test]
    fn new_row_is_withheld_only_from_a_mongo_view() {
        // Generic SQL is left out: it has no abilities of its own, since a
        // plugin target forces every write off.
        for engine in Target::tiles(&[]).map(|target| target.engine) {
            assert!(
                Target::built_in(engine).takes_inserts(RelationKind::Table),
                "{engine:?}"
            );
            assert_eq!(
                Target::built_in(engine).takes_inserts(RelationKind::View),
                engine != Engine::MongoDb,
                "{engine:?}"
            );
            assert_eq!(
                Target::built_in(engine).takes_imports(RelationKind::Table),
                engine != Engine::Redis,
                "{engine:?}"
            );
        }
    }

    #[test]
    fn an_emptied_new_row_field_is_sent_only_where_the_engine_writes_one() {
        for engine in Target::tiles(&[]).map(|target| target.engine) {
            let sends = engine.new_row_sends_empty("name");
            assert_eq!(sends, engine != Engine::MongoDb, "{engine:?}");
        }
        assert!(Engine::Redis.new_row_sends_empty("value"));
        assert!(Engine::Redis.new_row_sends_empty("member"));
        for column in ["ttl", "index", "score"] {
            assert!(!Engine::Redis.new_row_sends_empty(column), "{column}");
        }
    }

    #[test]
    fn clickhouse_brackets_no_batch_and_says_why_an_import_has_no_transaction() {
        let spec = Engine::ClickHouse.spec();
        assert_eq!(spec.sql.transaction_opening(), None);
        assert!(spec.abilities.chunk_refusal.is_some());
    }

    #[test]
    fn numeric_types_are_per_engine_and_clickhouse_peels_its_wrappers() {
        for numeric in [
            "Nullable(UInt64)",
            "LowCardinality(Nullable(Float64))",
            "Decimal(10, 2)",
            "Decimal256(4)",
            "Int256",
            "BFloat16",
        ] {
            assert!(Engine::ClickHouse.is_numeric_type(numeric), "{numeric}");
        }
        for other in ["Nullable(String)", "LowCardinality(String)", "Date"] {
            assert!(!Engine::ClickHouse.is_numeric_type(other), "{other}");
        }
        assert!(!Engine::Sqlite.is_numeric_type("int32"));
        assert!(!Engine::Sqlite.is_numeric_type("float64"));
    }

    #[test]
    fn only_numbers_are_numeric_across_the_three_engines_spellings() {
        for numeric in [
            "int4",
            "INTEGER",
            "bigint",
            "tinyint(1)",
            "int unsigned",
            "decimal(10,2)",
            "double precision",
            "numeric",
            "money",
            "REAL",
            // MongoDB's `$type` names.
            "int",
            "long",
            "double",
            "decimal",
            // DuckDB's.
            "UBIGINT",
            "hugeint",
        ] {
            assert!(is_numeric_type(numeric), "{numeric}");
        }
        // The near misses this predicate exists to get right: three of them
        // contain `int`, and a bit is a string of them.
        for other in [
            "interval",
            "point",
            "bit(8)",
            "text",
            "timestamptz",
            "uuid",
            "jsonb",
            "bytea",
            "mixed",
            "timestamp",
        ] {
            assert!(!is_numeric_type(other), "{other}");
        }
    }

    #[test]
    fn an_engine_round_trips_through_the_spelling_it_is_stored_as() {
        // `as_str` is what lands in `profiles.toml`. If `parse` ever stopped
        // accepting one of them, every profile written with it would fail to
        // load with no way back.
        for engine in Engine::ALL {
            assert_eq!(Engine::parse(engine.as_str()), Ok(engine));
        }
    }

    #[test]
    fn a_url_scheme_picks_the_engine_and_an_unknown_one_is_named() {
        assert_eq!(
            ConnectionConfig::from_url("postgresql://someone@db.example.test/dbdelve_test")
                .unwrap()
                .engine(),
            Engine::Postgres
        );
        assert_eq!(
            ConnectionConfig::from_url("sqlite:///tmp/dbdelve.db").unwrap(),
            ConnectionConfig::Sqlite {
                path: "/tmp/dbdelve.db".into(),
                statement_timeout: 0
            }
        );

        assert_eq!(
            ConnectionConfig::from_url("mongodb://db.example.test/dbdelve")
                .unwrap()
                .engine(),
            Engine::MongoDb
        );
        // Named, not merely rejected: "invalid URL" leaves the user guessing
        // which part of it dbdelve objected to.
        let error = ConnectionConfig::from_url("cassandra://db.example.test/ks").unwrap_err();
        assert!(error.contains("cassandra"), "{error}");
        assert!(ConnectionConfig::from_url("db.example.test/dbdelve").is_err());
    }

    #[test]
    fn a_sqlite_profile_has_no_server_half_and_a_postgres_one_does() {
        assert!(
            ConnectionConfig::Sqlite {
                path: "/tmp/dbdelve.db".into(),
                statement_timeout: 0
            }
            .server()
            .is_none()
        );
        assert!(
            ConnectionConfig::postgres(ServerConfig::default())
                .server()
                .is_some()
        );
    }

    #[test]
    fn an_endpoint_names_whatever_was_being_talked_to() {
        assert_eq!(
            ConnectionConfig::Sqlite {
                path: "/tmp/dbdelve.db".into(),
                statement_timeout: 0
            }
            .endpoint(),
            "/tmp/dbdelve.db"
        );
        assert_eq!(
            ServerConfig {
                host: "db.example.test".into(),
                port: Some(8432),
                ..ServerConfig::default()
            }
            .endpoint(),
            "db.example.test:8432"
        );
        assert_eq!(
            ServerConfig {
                host: "db.example.test".into(),
                port: None,
                ..ServerConfig::default()
            }
            .endpoint(),
            "db.example.test"
        );
    }

    #[test]
    fn an_edit_reconnects_for_a_new_destination_but_not_for_a_blank_password() {
        let stored = ConnectionConfig::postgres(ServerConfig {
            host: "db.example.test".into(),
            port: Some(5432),
            database: "dbdelve".into(),
            user: "someone".into(),
            password: "secret".into(),
            ..ServerConfig::default()
        });
        let edited = |change: fn(&mut ServerConfig)| {
            let mut server = stored.server().unwrap().clone();
            // What the form hands back: it never fills the password in.
            server.password.clear();
            change(&mut server);
            ConnectionConfig::postgres(server)
        };

        assert!(!stored.needs_reconnect(&stored.clone()));
        assert!(!stored.needs_reconnect(&edited(|_| {})));
        let mut named = stored.clone();
        if let ConnectionConfig::Postgres(postgres) = &mut named {
            postgres.service = Some("harness".into());
        }
        assert!(stored.needs_reconnect(&named));
        assert!(stored.needs_reconnect(&edited(|server| server.host = "elsewhere".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.port = Some(6432))));
        assert!(stored.needs_reconnect(&edited(|server| server.database = "other".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.user = "someone_else".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.password = "typed".into())));
        assert!(stored.needs_reconnect(&edited(|server| {
            server.ssh = Some(SshTunnel {
                host: "bastion".into(),
                ..SshTunnel::default()
            })
        })));
        assert!(stored.needs_reconnect(&ConnectionConfig::MySql(stored.server().unwrap().clone())));
        assert!(stored.needs_reconnect(&ConnectionConfig::Sqlite {
            path: "/tmp/dbdelve.db".into(),
            statement_timeout: 0
        }));
    }

    #[test]
    fn an_edit_retargets_only_for_where_it_connects() {
        let stored = ConnectionConfig::postgres(ServerConfig {
            host: "db.example.test".into(),
            port: Some(5432),
            database: "dbdelve".into(),
            user: "someone".into(),
            ..ServerConfig::default()
        });
        let edited = |change: fn(&mut ServerConfig)| {
            let mut server = stored.server().unwrap().clone();
            change(&mut server);
            ConnectionConfig::postgres(server)
        };

        // The same server and database, reached another way.
        assert!(!stored.retargets(&stored.clone()));
        assert!(!stored.retargets(&edited(|server| server.password = "typed".into())));
        assert!(!stored.retargets(&edited(|server| server.statement_timeout = 30)));
        assert!(!stored.retargets(&edited(|server| server.sslmode = SslMode::Require)));
        assert!(!stored.retargets(&edited(|server| {
            server.root_certificate = Some("/etc/ca.pem".into())
        })));

        // Another server, database or way in to them.
        assert!(stored.retargets(&edited(|server| server.user = "someone_else".into())));
        let mut named = stored.clone();
        if let ConnectionConfig::Postgres(postgres) = &mut named {
            postgres.service = Some("harness".into());
        }
        assert!(stored.retargets(&named));
        assert!(stored.retargets(&edited(|server| server.host = "elsewhere".into())));
        assert!(stored.retargets(&edited(|server| server.port = Some(6432))));
        assert!(stored.retargets(&edited(|server| server.database = "other".into())));
        assert!(stored.retargets(&edited(|server| {
            server.ssh = Some(SshTunnel {
                host: "bastion".into(),
                ..SshTunnel::default()
            })
        })));
        assert!(stored.retargets(&edited(|server| {
            server.ssm = Some(SsmTunnel {
                target: "i-0123456789abcdef0".into(),
                profile: None,
                region: None,
                aws_cli: None,
                session_manager_plugin: None,
                attempt: None,
            })
        })));
        assert!(stored.retargets(&ConnectionConfig::MySql(stored.server().unwrap().clone())));

        let tunnelled = edited(|server| {
            server.ssh = Some(SshTunnel {
                host: "bastion".into(),
                ..SshTunnel::default()
            })
        });
        let other_bastion = |change: fn(&mut SshTunnel)| {
            let mut config = tunnelled.clone();
            change(config.ssh_mut().unwrap());
            config
        };
        assert!(tunnelled.retargets(&other_bastion(|ssh| ssh.host = "jump".into())));
        assert!(tunnelled.retargets(&other_bastion(|ssh| ssh.port = Some(2222))));
        assert!(!tunnelled.retargets(&other_bastion(|ssh| ssh.user = "admin".into())));

        let file = |path: &str, statement_timeout| ConnectionConfig::Sqlite {
            path: path.into(),
            statement_timeout,
        };
        assert!(!file("/tmp/a.db", 0).retargets(&file("/tmp/a.db", 30)));
        assert!(file("/tmp/a.db", 0).retargets(&file("/tmp/b.db", 0)));
        assert!(file("/tmp/a.db", 0).retargets(&ConnectionConfig::Turso {
            path: "/tmp/a.db".into(),
            statement_timeout: 0
        }));
    }

    #[test]
    fn a_mongo_option_retargets_only_when_it_picks_the_servers() {
        let with = |options: &str| {
            ConnectionConfig::MongoDb(MongoConfig {
                server: ServerConfig {
                    host: "db.example.test".into(),
                    database: "app".into(),
                    ..ServerConfig::default()
                },
                srv: false,
                options: options.into(),
                login_database: None,
            })
        };
        let stored = with("replicaSet=rs0&connectTimeoutMS=1000");

        assert!(!stored.retargets(&with("connectTimeoutMS=5000&replicaSet=rs0")));
        assert!(!stored.retargets(&with("replicaSet=rs0&appName=report")));
        assert!(stored.retargets(&with("replicaSet=rs1&connectTimeoutMS=1000")));
        assert!(stored.retargets(&with("replicaSet=rs0&authSource=admin")));
        assert!(stored.retargets(&with("replicaSet=rs0&directConnection=true")));
    }

    #[test]
    fn a_key_passphrase_is_left_out_of_debug_output() {
        let tunnel = SshTunnel {
            host: "bastion".into(),
            passphrase: "correct horse".into(),
            ..SshTunnel::default()
        };
        let config = ConnectionConfig::MySql(ServerConfig {
            ssm: None,
            ssh: Some(tunnel),
            ..ServerConfig::default()
        });
        for printed in [format!("{config:?}"), format!("{config:#?}")] {
            assert!(printed.contains("bastion"), "{printed}");
            assert!(!printed.contains("correct horse"), "{printed}");
        }
    }

    #[test]
    fn an_edit_reconnects_for_a_typed_key_passphrase_but_not_a_blank_one() {
        let tunnelled = |passphrase: &str| {
            ConnectionConfig::MySql(ServerConfig {
                host: "db.internal".into(),
                ssm: None,
                ssh: Some(SshTunnel {
                    host: "bastion".into(),
                    passphrase: passphrase.into(),
                    ..SshTunnel::default()
                }),
                ..ServerConfig::default()
            })
        };
        let stored = tunnelled("secret");

        assert!(!stored.needs_reconnect(&tunnelled("")));
        assert!(stored.needs_reconnect(&tunnelled("typed")));
    }

    #[test]
    fn only_the_engine_with_an_implicit_transaction_needs_no_brackets() {
        // Postgres runs one submission as one transaction; the other two commit
        // each statement on its own and have to be told. `BEGIN` rather than
        // MySQL's own `START TRANSACTION` because the brackets are read back by
        // `sql::is_generated_write`, whose grammar knows only the first.
        assert_eq!(Engine::Postgres.transaction_start(), None);
        assert_eq!(Engine::MySql.transaction_start(), Some("BEGIN"));
        assert_eq!(Engine::Sqlite.transaction_start(), Some("BEGIN"));
        assert_eq!(
            Engine::SqlServer.transaction_start(),
            Some("BEGIN TRANSACTION")
        );
    }

    #[test]
    fn each_engine_quotes_the_way_its_own_server_reads() {
        // An identifier and a literal are both user data, and both reach a
        // statement dbdelve generates. The escape is what stops a table called
        // `odd"name` from ending the identifier early.
        assert_eq!(
            Engine::Postgres.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        assert_eq!(
            Engine::Sqlite.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        assert_eq!(Engine::MySql.quote_identifier("odd`name"), "`odd``name`");

        for engine in Engine::ALL
            .into_iter()
            .filter(|e| !matches!(e, Engine::SqlServer | Engine::MongoDb | Engine::Redis))
        {
            assert_eq!(
                engine.quote_literal("odd'value"),
                "'odd''value'",
                "{engine:?}"
            );
        }
        // A redis-cli word: bare where it can be, else double-quoted.
        assert_eq!(Engine::Redis.quote_literal("odd'value"), "\"odd'value\"");
        assert_eq!(Engine::Redis.quote_identifier("user:1"), "user:1");
        assert_eq!(Engine::Redis.unquote_identifier("\"a key\""), "a key");

        // Only MySQL reads a backslash as an escape, so only MySQL has to
        // double one. Getting this wrong is how a trailing backslash turns the
        // closing quote into an escaped one and swallows the rest of the
        // statement.
        assert_eq!(Engine::MySql.quote_literal(r"back\slash"), r"'back\\slash'");
        assert_eq!(
            Engine::Postgres.quote_literal(r"back\slash"),
            r"'back\slash'"
        );
        assert_eq!(Engine::Sqlite.quote_literal(r"back\slash"), r"'back\slash'");

        assert_eq!(
            Engine::Postgres.qualified("odd\"schema", "table"),
            "\"odd\"\"schema\".\"table\""
        );
        assert_eq!(
            Engine::MySql.qualified("dbdelve_dev", "table"),
            "`dbdelve_dev`.`table`"
        );
    }

    #[test]
    fn snowflake_quotes_the_standard_way_and_doubles_a_backslash() {
        // A quoted name is read exactly where a bare one is folded to upper
        // case, so the double quote is what makes the catalog's spelling the
        // one the server looks up.
        assert_eq!(
            Engine::Snowflake.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        // A backslash is an escape in a Snowflake string, as in MySQL. Left
        // single, a trailing one swallows the closing quote.
        assert_eq!(
            Engine::Snowflake.quote_literal(r"back\slash"),
            r"'back\\slash'"
        );
        assert_eq!(
            Engine::Snowflake.qualified("PUBLIC", "ORDERS"),
            "\"PUBLIC\".\"ORDERS\""
        );
    }

    #[test]
    fn a_name_is_left_bare_only_in_the_case_its_server_folds_a_bare_one_to() {
        assert_eq!(
            Engine::Snowflake.quote_identifier_if_needed("EVENTS"),
            "EVENTS"
        );
        assert_eq!(
            Engine::Snowflake.quote_identifier_if_needed("events"),
            "\"events\""
        );
        assert_eq!(
            Engine::Postgres.quote_identifier_if_needed("events"),
            "events"
        );
        assert_eq!(
            Engine::Postgres.quote_identifier_if_needed("EVENTS"),
            "\"EVENTS\""
        );
    }

    #[test]
    fn sql_server_quotes_the_standard_way_and_writes_unicode_literals() {
        // `"name"` rather than `[name]`: the gates' grammar reads only the
        // first, and the login's `QUOTED_IDENTIFIER` makes it an identifier.
        assert_eq!(
            Engine::SqlServer.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        assert_eq!(
            Engine::SqlServer.qualified("dbo", "accounts"),
            "\"dbo\".\"accounts\""
        );
        // `N` so a character outside the database's code page is not stored
        // as `?`; a backslash is an ordinary character in T-SQL.
        assert_eq!(Engine::SqlServer.quote_literal("李'小"), "N'李''小'");
        assert_eq!(
            Engine::SqlServer.quote_literal(r"back\slash"),
            r"N'back\slash'"
        );
    }

    #[test]
    fn sql_server_spells_a_value_for_its_column_type() {
        let quote = |value, data_type| Engine::SqlServer.quote_value(value, data_type);
        // Bytes as the bare hex literal the grid shows, so a binary key matches.
        assert_eq!(quote("0x00FF", Some("binary")), "0x00FF");
        assert_eq!(quote("0xab", Some("varbinary(16)")), "0xab");
        assert_eq!(quote("0x", Some("image")), "0x");
        // Anything that is not exactly a hex literal stays quoted: this is the
        // one unquoted path into a statement.
        // Either case of the prefix, written the way the grid shows it.
        assert_eq!(quote("0X00ff", Some("varbinary")), "0x00ff");
        // A rowversion is bytes too, whichever of its names the catalog uses.
        assert_eq!(
            quote("0x00000000000007D1", Some("rowversion")),
            "0x00000000000007D1"
        );
        assert_eq!(
            quote("0x00000000000007D1", Some("timestamp")),
            "0x00000000000007D1"
        );
        for value in [
            "0x0",
            "0xZZ",
            "0x00; DROP TABLE t",
            "00FF",
            "0X0",
            " 0x00",
            "0x0é",
        ] {
            assert_eq!(
                quote(value, Some("varbinary")),
                format!("N'{}'", value.replace('\'', "''")),
                "{value}"
            );
        }
        assert_eq!(quote("0x00FF", Some("varchar")), "'0x00FF'");
        // `N` only where the column is Unicode, or the value needs it.
        assert_eq!(quote("o'hara", Some("varchar")), "'o''hara'");
        assert_eq!(quote("o'hara", Some("CHAR(10)")), "'o''hara'");
        assert_eq!(quote("42", Some("int")), "'42'");
        assert_eq!(quote("2024-01-01", Some("datetime2")), "'2024-01-01'");
        // `datetime` in the one form no `DATEFORMAT` or language reorders.
        assert_eq!(
            quote("2024-01-02 03:04:05.000", Some("datetime")),
            "'2024-01-02T03:04:05.000'"
        );
        assert_eq!(
            quote("2024-01-02 03:04:00", Some("smalldatetime")),
            "'2024-01-02T03:04:00'"
        );
        assert_eq!(
            quote("2024-01-02", Some("datetime")),
            "'2024-01-02T00:00:00'"
        );
        // Anything else shaped differently is left as typed.
        for value in [
            "2024-01-02 03:04",
            "2024-01-02 03:04:05.",
            "yesterday",
            "%2024%",
        ] {
            assert_eq!(
                quote(value, Some("datetime")),
                format!("'{value}'"),
                "{value}"
            );
        }
        assert_eq!(quote("李'小", Some("varchar")), "N'李''小'");
        for data_type in [
            Some("nvarchar"),
            Some("nchar"),
            Some("xml"),
            Some("sql_variant"),
        ] {
            assert_eq!(quote("x", data_type), "N'x'", "{data_type:?}");
        }
        assert_eq!(quote("x", Some("dbo.custom")), "N'x'");
        assert_eq!(quote("x", None), "N'x'");
        // Every other engine is unchanged: Postgres's `\x…` round-trips quoted.
        for engine in Engine::ALL.into_iter().filter(|e| *e != Engine::SqlServer) {
            assert_eq!(
                engine.quote_value("0x00FF", Some("bytea")),
                engine.quote_literal("0x00FF")
            );
            assert_eq!(
                engine.quote_value("v", Some("varchar")),
                engine.quote_literal("v")
            );
        }
    }

    #[test]
    fn image_is_binary_on_sql_server_alone() {
        assert!(Engine::SqlServer.is_binary_type("image"));
        assert!(Engine::SqlServer.is_binary_type("rowversion"));
        assert!(Engine::SqlServer.is_binary_type("timestamp"));
        assert!(!Engine::Postgres.is_binary_type("timestamp"));
        assert!(!Engine::Sqlite.is_binary_type("image"));
        assert!(!Engine::Postgres.is_binary_type("image"));
        for engine in Engine::ALL {
            for data_type in ["bytea", "blob", "longblob", "varbinary(16)", "BINARY"] {
                assert!(engine.is_binary_type(data_type), "{engine:?} {data_type}");
            }
        }
    }

    #[test]
    fn bytes_are_written_as_each_engines_own_literal_and_spelled_as_its_driver_does() {
        let hi = b"hi\xff\x00";
        for (engine, literal, empty) in [
            (Engine::Postgres, Some(r"E'\\x6869FF00'"), Some(r"E'\\x'")),
            (Engine::MySql, Some("X'6869FF00'"), Some("X''")),
            (Engine::MariaDb, Some("X'6869FF00'"), Some("X''")),
            (Engine::Sqlite, Some("X'6869FF00'"), Some("X''")),
            (Engine::Turso, Some("X'6869FF00'"), Some("X''")),
            (Engine::SqlServer, Some("0x6869FF00"), None),
            (Engine::DuckDb, None, None),
            (Engine::Snowflake, None, None),
            (Engine::ClickHouse, None, None),
            (Engine::MongoDb, None, None),
            (Engine::Redis, None, None),
            (Engine::Generic, None, None),
        ] {
            assert_eq!(engine.binary_literal(hi).as_deref(), literal, "{engine:?}");
            assert_eq!(engine.binary_literal(b"").as_deref(), empty, "{engine:?}");
            // Whatever the driver spells, it reads back.
            if let Some(text) = engine.bytes_text(hi) {
                assert_eq!(
                    engine.bytes_of(&text, usize::MAX),
                    Some(hi.to_vec()),
                    "{text}"
                );
            }
        }
        assert_eq!(
            Engine::Postgres.bytes_text(hi).as_deref(),
            Some(r"\x6869ff00")
        );
        assert_eq!(Engine::Sqlite.bytes_text(b"").as_deref(), Some("x''"));
        assert_eq!(Engine::Redis.bytes_text(hi), None);
    }

    #[test]
    fn bytes_read_back_out_of_each_engines_own_spelling_of_them() {
        let dead = b"\xde\xad\xbe\xef".to_vec();
        for (engine, empty, upper, lower) in [
            (Engine::Postgres, "\\x", "\\xDEADBEEF", "\\xdeadbeef"),
            (Engine::MySql, "0x", "0xDEADBEEF", "0xdeadbeef"),
            (Engine::MariaDb, "0x", "0xDEADBEEF", "0xdeadbeef"),
            (Engine::SqlServer, "0x", "0xDEADBEEF", "0xdeadbeef"),
            (Engine::Generic, "0x", "0xDEADBEEF", "0xdeadbeef"),
            (Engine::Sqlite, "x''", "x'DEADBEEF'", "x'deadbeef'"),
            (Engine::Turso, "x''", "x'DEADBEEF'", "x'deadbeef'"),
            (Engine::DuckDb, "", "DEADBEEF", "deadbeef"),
            (Engine::Snowflake, "", "DEADBEEF", "deadbeef"),
        ] {
            assert_eq!(engine.bytes_of(empty, 64), Some(Vec::new()), "{engine:?}");
            assert_eq!(engine.bytes_of(upper, 64), Some(dead.clone()), "{engine:?}");
            assert_eq!(engine.bytes_of(lower, 64), Some(dead.clone()), "{engine:?}");
            // Only as much as was asked for.
            assert_eq!(engine.bytes_of(upper, 2), Some(dead[..2].to_vec()));
            for not_bytes in [
                &upper[..upper.len() - 1],
                &upper.replace("EF", "EG"),
                "a word",
            ] {
                assert_eq!(
                    engine.bytes_of(not_bytes, 64),
                    None,
                    "{engine:?} {not_bytes}"
                );
            }
        }
        // A plugin writes bytes as any plugin does, whatever dialect it
        // speaks, bare hex's among them.
        for engine in [Engine::Postgres, Engine::DuckDb, Engine::Snowflake] {
            assert_eq!(
                engine.bytes_of("0x6869", 64),
                Some(b"hi".to_vec()),
                "{engine:?}"
            );
        }
        assert_eq!(Engine::Sqlite.bytes_of("x'", 64), None);
        // Past the part read, the shape of the whole value still counts.
        let long = "41".repeat(2000);
        assert_eq!(Engine::Sqlite.bytes_of(&format!("x'{long}"), 2), None);
        assert_eq!(Engine::Sqlite.bytes_of(&format!("x'{long}4'"), 2), None);
        assert_eq!(
            Engine::Sqlite.bytes_of(&format!("x'{long}'"), 2),
            Some(b"AA".to_vec())
        );

        assert!(Engine::Postgres.shows_bytes_as_hex());

        let mongo = Engine::MongoDb;
        assert!(!mongo.shows_bytes_as_hex());
        assert_eq!(
            mongo.bytes_of("BinData(0, '3q2+7w==')", 64),
            Some(dead.clone())
        );
        assert_eq!(mongo.bytes_of("BinData(5, '')", 64), Some(Vec::new()));
        assert_eq!(
            mongo.bytes_of("BinData(0, '3q2+7w==')", 3),
            Some(dead[..3].to_vec())
        );
        for not_bytes in [
            "BinData(0, '3q2+7w=')",
            "BinData(0, '3q2+7w=*')",
            "BinData(x, '3q2+7w==')",
            "BinData(0, '3q2+7w==",
            // A UUID is shown as one, not as its bytes.
            "0e5c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b",
        ] {
            assert_eq!(mongo.bytes_of(not_bytes, 64), None, "{not_bytes}");
        }
    }

    #[test]
    fn clickhouse_is_a_server_engine_with_a_url_that_takes_only_the_tls_keys() {
        let config = ConnectionConfig::from_url(
            "clickhouse://someone%40example.com:pw@ch.example.test:8443/logs?sslmode=verify-full&sslrootcert=/certs/ca.pem",
        )
        .unwrap();
        assert_eq!(config.engine(), Engine::ClickHouse);
        let server = config.server().expect("a server half, for the Keychain");
        assert_eq!(
            (
                server.user.as_str(),
                server.password.as_str(),
                server.host.as_str(),
                server.port,
                server.database.as_str(),
            ),
            (
                "someone@example.com",
                "pw",
                "ch.example.test",
                Some(8443),
                "logs"
            )
        );
        assert_eq!(server.sslmode, SslMode::VerifyFull);
        assert_eq!(server.root_certificate.as_deref(), Some("/certs/ca.pem"));
        assert_eq!(config.endpoint(), server.endpoint());

        let error = ConnectionConfig::from_url("clickhouse://u@h/db?compress=1").unwrap_err();
        assert_eq!(
            error,
            "Connection URL parameter compress is not one dbdelve can pass to ClickHouse."
        );

        assert_eq!(Engine::parse("clickhouse"), Ok(Engine::ClickHouse));
        assert_eq!(Engine::ClickHouse.as_str(), "clickhouse");
        assert_eq!(Engine::ClickHouse.fields(), Fields::Server);
        assert_eq!(Engine::ClickHouse.default_port(), Some(8123));
        assert_eq!(
            crate::connection_form::placeholders(Target::built_in(Engine::ClickHouse)).port,
            "8123"
        );
        assert!(Engine::ClickHouse.holds_read_only());
        assert_eq!(
            read_only_statement(Engine::ClickHouse, true),
            None,
            "the hold rides on each request"
        );
    }

    #[test]
    fn sql_server_is_a_server_engine_reached_by_either_url_scheme() {
        for url in [
            "mssql://someone%40example.com@db.example.test:1433/dbdelve_dev",
            "sqlserver://someone%40example.com@db.example.test:1433/dbdelve_dev",
        ] {
            let config = ConnectionConfig::from_url(url).unwrap();
            assert_eq!(config.engine(), Engine::SqlServer, "{url}");
            let server = config.server().expect("a server half, for the Keychain");
            assert_eq!(server.user, "someone@example.com");
            assert_eq!(server.password, "", "blank is valid");
            assert_eq!(server.port, Some(1433));
        }
        assert_eq!(Engine::parse("mssql"), Ok(Engine::SqlServer));
        assert_eq!(Engine::SqlServer.fields(), Fields::Server);
        assert_eq!(
            Engine::SqlServer.transaction_start(),
            Some("BEGIN TRANSACTION")
        );
        // No prefix form to put on a copy, and no session switch for Read-only.
        for mode in ExplainMode::ALL {
            assert_eq!(Engine::SqlServer.explain_prefix(mode), None);
        }
        assert!(!Engine::SqlServer.holds_read_only());
    }

    #[test]
    fn mariadb_is_its_own_engine_and_a_mysql_profile_stays_mysql() {
        let config = ConnectionConfig::from_url("mariadb://someone@db.example.test/app").unwrap();
        assert_eq!(config.engine(), Engine::MariaDb);
        assert_eq!(Engine::parse("mysql"), Ok(Engine::MySql));
        assert_eq!(
            Engine::MariaDb.explain_prefix(ExplainMode::Plan),
            Some("EXPLAIN ")
        );
        assert_eq!(
            Engine::MariaDb.explain_prefix(ExplainMode::Analyze),
            Some("ANALYZE ")
        );
        assert_eq!(Engine::MariaDb.quote_identifier("a`b"), "`a``b`");
        assert!(Engine::MariaDb.holds_read_only());
    }

    #[test]
    fn clickhouse_plans_a_copy_and_has_nothing_to_analyze() {
        assert_eq!(
            Engine::ClickHouse.explain(ExplainMode::Plan, "SELECT 1"),
            Some("EXPLAIN SELECT 1".to_string())
        );
        assert_eq!(
            Engine::ClickHouse.explain_prefix(ExplainMode::Analyze),
            None
        );
        assert_eq!(
            Engine::ClickHouse.explain(ExplainMode::Analyze, "SELECT 1"),
            None
        );
    }

    #[test]
    fn mongodb_explains_by_suffix_and_the_sql_engines_by_prefix() {
        for mode in ExplainMode::ALL {
            assert_eq!(Engine::MongoDb.explain_prefix(mode), Some(""));
            assert_eq!(Engine::Postgres.explain_suffix(mode), "");
        }
        assert_eq!(
            Engine::MongoDb.explain_suffix(ExplainMode::Plan),
            r#".explain("queryPlanner")"#
        );
        assert_eq!(
            Engine::MongoDb.explain_suffix(ExplainMode::Analyze),
            r#".explain("executionStats")"#
        );
    }

    #[test]
    fn snowflake_is_stored_under_its_own_name_and_has_no_url() {
        assert_eq!(Engine::parse("snowflake"), Ok(Engine::Snowflake));
        assert_eq!(Engine::Snowflake.as_str(), "snowflake");
        assert!(ConnectionConfig::from_url("snowflake://account/db").is_err());
    }

    #[test]
    fn generic_sql_is_an_engine_with_no_way_to_connect_to_it() {
        assert_eq!(Engine::parse("generic"), Ok(Engine::Generic));
        assert_eq!(
            ConnectionConfig::from_url("generic://x").unwrap_err(),
            GENERIC_REFUSAL
        );
        assert!(!Target::tiles(&[]).any(|target| target.engine == Engine::Generic));
        assert_eq!(
            Engine::from_dbms_name("PostgreSQL 16.2"),
            Some(Engine::Postgres)
        );
        assert_eq!(
            Engine::from_dbms_name("microsoft sql server"),
            Some(Engine::SqlServer)
        );
        assert_eq!(Engine::from_dbms_name("DB2/LINUXX8664"), None);
    }

    #[test]
    fn snowflake_offers_no_explain_and_sets_nothing_for_read_only() {
        for mode in ExplainMode::ALL {
            assert_eq!(Engine::Snowflake.explain_prefix(mode), None);
        }
        // There is no session for a setting to live on.
        assert_eq!(read_only_statement(Engine::Snowflake, true), None);
        assert_eq!(read_only_statement(Engine::Snowflake, false), None);
    }

    #[test]
    fn merging_the_second_half_brings_its_sequences_with_its_routines() {
        let mut screen = Catalog {
            schemas: vec![schema("main", &["t"], &[])],
            database: None,
            ..Default::default()
        };
        let mut second = schema("main", &[], &["f"]);
        second.sequences = vec!["s".to_string()];
        screen.merge(Catalog {
            schemas: vec![second],
            database: None,
            ..Default::default()
        });

        let main = &screen.schemas[0];
        assert_eq!(main.relations.len(), 1);
        assert_eq!(main.relations[0].name, "t");
        assert_eq!(main.routines.len(), 1);
        assert_eq!(main.routines[0].name, "f");
        assert_eq!(main.sequences, ["s"]);
    }

    #[test]
    fn the_sequences_join_the_routines_where_the_engine_lists_them() {
        let routines = Ok(Catalog {
            schemas: vec![schema("main", &[], &["f"])],
            database: None,
            ..Default::default()
        });
        let mut read = schema("main", &[], &[]);
        read.sequences = vec!["a".to_string(), "b".to_string()];
        let mut other = schema("other", &[], &[]);
        other.sequences = vec!["n".to_string()];
        let second = with_sequences(Engine::DuckDb, routines, || {
            Ok(Catalog {
                schemas: vec![read, other],
                database: None,
                ..Default::default()
            })
        })
        .expect("the second half is read");

        assert_eq!(second.schemas[0].routines.len(), 1);
        assert_eq!(second.schemas[0].sequences, ["a", "b"]);
        // A schema holding only sequences is pushed, so it can be listed.
        assert_eq!(second.schemas[1].name, "other");
        assert_eq!(second.schemas[1].sequences, ["n"]);
    }

    #[test]
    fn a_failed_sequence_read_leaves_the_routines() {
        let routines = Ok(Catalog {
            schemas: vec![schema("main", &[], &["f"])],
            database: None,
            ..Default::default()
        });
        let second = with_sequences(Engine::DuckDb, routines, || {
            Err(plain_error("no sequence catalog".to_string()))
        })
        .expect("the routines survive a failed sequence read");

        assert_eq!(second.schemas.len(), 1);
        assert_eq!(second.schemas[0].routines.len(), 1);
        assert!(second.schemas[0].sequences.is_empty());
    }

    #[test]
    fn sequences_are_read_only_where_the_engine_lists_them() {
        let routines = Ok(Catalog::default());
        let second = with_sequences(Engine::Sqlite, routines, || {
            panic!("SQLite has no sequence catalog to read")
        })
        .expect("nothing was read");
        assert!(second.schemas.is_empty());
    }

    #[test]
    fn only_the_engines_that_have_sequences_list_them() {
        let plugin: &'static plugin::Plugin = Box::leak(Box::new(plugin::Plugin {
            id: "odbc".into(),
            name: "ODBC".into(),
            version: "1".into(),
            executable: "dbdelve-odbc".into(),
        }));
        for engine in Engine::ALL {
            let lists = matches!(
                engine,
                Engine::Postgres
                    | Engine::SqlServer
                    | Engine::Snowflake
                    | Engine::DuckDb
                    | Engine::MariaDb
            );
            assert_eq!(
                Target::built_in(engine).abilities().lists_sequences,
                lists,
                "{engine:?}"
            );
            let through_a_plugin = Target {
                engine,
                driver: Driver::Plugin(plugin),
            };
            assert!(!through_a_plugin.abilities().lists_sequences, "{engine:?}");
        }
    }

    fn schema(name: &str, relations: &[&str], routines: &[&str]) -> Schema {
        Schema {
            name: name.to_string(),
            relations: relations
                .iter()
                .map(|name| Relation {
                    name: (*name).to_string(),
                    kind: RelationKind::Table,
                    partition_of: None,
                    size: None,
                    rows: None,
                })
                .collect(),
            routines: routines
                .iter()
                .map(|name| Routine {
                    name: (*name).to_string(),
                    kind: RoutineKind::Function,
                    identity_arguments: String::new(),
                    result_type: String::new(),
                    language: String::new(),
                    definition: String::new(),
                })
                .collect(),
            sequences: Vec::new(),
        }
    }

    #[test]
    fn the_second_half_of_a_catalog_joins_the_first_by_name() {
        let mut catalog = Catalog {
            schemas: vec![
                schema("ops", &["jobs"], &[]),
                schema("public", &["accounts"], &[]),
            ],
            database: None,
            ..Default::default()
        };
        catalog.merge(Catalog {
            schemas: vec![
                schema("public", &[], &["digest"]),
                // A schema holding only functions is in the second half alone,
                // and is a schema the explorer has to show.
                schema("audit", &[], &["trail"]),
            ],
            database: None,
            ..Default::default()
        });

        let named = |name: &str| {
            catalog
                .schemas
                .iter()
                .find(|schema| schema.name == name)
                .unwrap_or_else(|| panic!("{name} is listed"))
        };
        // The relations it already had are untouched, and its routines arrive.
        assert_eq!(named("public").relations.len(), 1);
        assert_eq!(named("public").routines[0].name, "digest");
        assert_eq!(named("audit").relations, []);
        assert_eq!(named("ops").routines, []);
        // A schema keeps the index it had before the merge, so a target taken
        // then still names it; name order is the presentation's to impose.
        assert_eq!(
            catalog
                .schemas
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["ops", "public", "audit"]
        );
        assert_eq!(
            catalog
                .by_name()
                .into_iter()
                .map(|(index, s)| (index, s.name.as_str()))
                .collect::<Vec<_>>(),
            [(2, "audit"), (0, "ops"), (1, "public")]
        );
    }

    #[test]
    fn each_engine_names_the_fields_its_connection_is_made_of() {
        // Two engines sharing a set is what lets the form keep focus where it
        // was when the chip moves between them.
        assert_eq!(Engine::Postgres.fields(), Fields::Server);
        assert_eq!(Engine::MySql.fields(), Fields::Server);
        assert_eq!(Engine::Sqlite.fields(), Fields::File);
        assert_eq!(Engine::Snowflake.fields(), Fields::Account);
    }

    #[test]
    fn a_snowflake_config_has_no_server_half() {
        // No password, so nothing for the credential fields or the Keychain
        // to be asked about.
        let mut account = SnowflakeConfig {
            account: "myorg-myaccount".into(),
            database: "ANALYTICS".into(),
            statement_timeout: 30,
            ..Default::default()
        };
        let config = ConnectionConfig::Snowflake(account.clone());
        assert_eq!(config.engine(), Engine::Snowflake);
        assert!(config.server().is_none());
        assert_eq!(config.statement_timeout(), 30);
        assert_eq!(config.endpoint(), "myorg-myaccount.snowflakecomputing.com");

        // A host that was given wins over the one the account implies.
        account.host = Some("myorg.privatelink.example".into());
        assert_eq!(
            ConnectionConfig::Snowflake(account).endpoint(),
            "myorg.privatelink.example"
        );
    }

    #[test]
    fn a_service_leaves_a_blank_database_for_the_connection_to_say() {
        let config = ConnectionConfig::Postgres(PostgresConfig {
            service: Some("harness".into()),
            ..PostgresConfig::default()
        });
        assert_eq!(config.database_name(), None);
    }

    #[test]
    fn a_snowflake_profile_picks_its_schema_and_drops_it_with_the_database() {
        let mut config = ConnectionConfig::Snowflake(SnowflakeConfig {
            database: "analytics".into(),
            ..Default::default()
        });
        assert_eq!(config.database_name().as_deref(), Some("ANALYTICS"));
        assert_eq!(config.schema(), None);

        config.set_schema(Some("Staging".into()));
        assert_eq!(config.schema(), Some("Staging"));
        config.set_database("ANALYTICS".into());
        assert_eq!(config.schema(), Some("Staging"), "the same database");

        config.set_database("lower".into());
        assert_eq!(config.database(), Some(r#""lower""#));
        assert_eq!(config.database_name().as_deref(), Some("lower"));
        assert_eq!(config.schema(), None);
    }

    #[test]
    fn a_duckdb_profile_picks_an_attached_database_and_a_schema_in_it() {
        let mut config = ConnectionConfig::duckdb("/data/dev.duckdb".into(), 0);
        assert_eq!(config.database(), Some(""));
        assert_eq!(config.database_name(), None, "the one the file opens as");
        assert_eq!(config.schema(), None);

        config.set_schema(Some("analytics".into()));
        assert_eq!(config.schema(), Some("analytics"));
        config.set_database("other".into());
        assert_eq!(config.database_name().as_deref(), Some("other"));
        assert_eq!(config.schema(), None, "a schema of the database left");
        assert!(Target::built_in(Engine::DuckDb).switches_database());
        assert!(Target::built_in(Engine::DuckDb).switches_database_in_session());
        assert!(Target::built_in(Engine::DuckDb).picks_schema());
        assert!(!Target::built_in(Engine::Postgres).switches_database_in_session());
    }

    #[test]
    fn only_a_catalog_of_one_database_names_its_objects_by_database() {
        let per_database: Vec<_> = Engine::ALL
            .into_iter()
            .filter(|&engine| Target::built_in(engine).catalog_is_per_database())
            .collect();
        for engine in [Engine::Postgres, Engine::SqlServer, Engine::DuckDb] {
            assert!(per_database.contains(&engine), "{engine:?}");
        }
        // Every database is a schema of the catalog already, and a file holds
        // one database whatever it is called.
        for engine in [
            Engine::MySql,
            Engine::MariaDb,
            Engine::ClickHouse,
            Engine::Sqlite,
        ] {
            assert!(!per_database.contains(&engine), "{engine:?}");
        }
    }

    #[test]
    fn a_quoted_identifier_reads_back_as_the_name_it_was() {
        // The two halves have to agree or dbdelve cannot recognise its own
        // output: a sort key it wrote would not match the header it came from.
        for engine in Engine::ALL {
            for name in ["id", "odd\"name", "odd`name", "spaced name", ""] {
                assert_eq!(
                    engine.unquote_identifier(&engine.quote_identifier(name)),
                    name,
                    "{engine:?} {name}"
                );
            }

            // Not a quoted identifier, so not a name. A bare position and a
            // function call both have to survive untouched.
            assert_eq!(engine.unquote_identifier("3"), "3");
            assert_eq!(engine.unquote_identifier("lower(name)"), "lower(name)");
        }
    }

    #[test]
    fn catalog_groups_relations_and_routines_by_schema() {
        let relations = result(
            &[
                "schema_name",
                "relation_name",
                "relation_kind",
                "partition_of",
                "size_bytes",
                "row_estimate",
            ],
            &[
                &[
                    Some("analytics"),
                    Some("events"),
                    Some("partitioned_table"),
                    None,
                    None,
                    None,
                ],
                &[
                    Some("analytics"),
                    Some("events_2026"),
                    Some("table"),
                    Some("events"),
                    Some("8192"),
                    Some("1200"),
                ],
                &[
                    Some("public"),
                    Some("accounts"),
                    Some("table"),
                    None,
                    Some("24576000"),
                    None,
                ],
                &[
                    Some("public"),
                    Some("account_overview"),
                    Some("view"),
                    None,
                    Some(""),
                    None,
                ],
            ],
        );
        let routines = result(
            &[
                "schema_name",
                "routine_name",
                "routine_kind",
                "identity_arguments",
                "result_type",
                "language",
                "definition",
            ],
            &[
                &[
                    Some("analytics"),
                    Some("refresh_events"),
                    Some("procedure"),
                    Some("full boolean"),
                    Some(""),
                    Some("plpgsql"),
                    Some("CREATE PROCEDURE analytics.refresh_events(full boolean)"),
                ],
                &[
                    Some("public"),
                    Some("account_name"),
                    Some("function"),
                    Some("account_id bigint"),
                    Some("text"),
                    Some("sql"),
                    Some("CREATE FUNCTION public.account_name(account_id bigint)"),
                ],
            ],
        );

        let catalog = assemble_catalog(relations, routines).unwrap();

        assert_eq!(catalog.schemas.len(), 2);
        assert_eq!(catalog.schemas[0].name, "analytics");
        assert_eq!(
            catalog.schemas[0].relations,
            vec![
                Relation {
                    name: "events".into(),
                    kind: RelationKind::PartitionedTable,
                    partition_of: None,
                    size: None,
                    rows: None,
                },
                Relation {
                    name: "events_2026".into(),
                    kind: RelationKind::Table,
                    partition_of: Some("events".into()),
                    size: Some(8192),
                    rows: Some(1200),
                },
            ]
        );
        assert_eq!(catalog.schemas[0].routines[0].kind, RoutineKind::Procedure);
        assert_eq!(catalog.schemas[1].name, "public");
        assert_eq!(catalog.schemas[1].relations[1].kind, RelationKind::View);
        assert_eq!(catalog.schemas[1].relations[0].size, Some(24_576_000));
        assert_eq!(catalog.schemas[1].relations[1].size, None);
        assert_eq!(catalog.schemas[1].routines[0].result_type, "text");
    }

    #[test]
    fn sizes_land_on_their_relations_and_nowhere_else() {
        let mut catalog = assemble_catalog(
            result(
                &["schema_name", "relation_name", "relation_kind"],
                &[
                    &[Some("public"), Some("accounts"), Some("table")],
                    &[Some("public"), Some("account_overview"), Some("view")],
                    &[Some("archive"), Some("accounts"), Some("table")],
                    &[Some("archive"), Some("unmeasured"), Some("table")],
                ],
            ),
            QueryResult::default(),
        )
        .unwrap();
        let sizes = assemble_sizes(result(
            &["schema_name", "relation_name", "size_bytes", "row_estimate"],
            &[
                &[Some("public"), Some("accounts"), Some("8192"), Some("40")],
                &[Some("archive"), Some("accounts"), None, None],
                &[Some("archive"), Some("unmeasured"), None, Some("7")],
                &[Some("public"), Some("dropped_since"), Some("16384"), None],
            ],
        ))
        .unwrap();
        assert!(!sizes["archive"].contains_key("accounts"));

        catalog.set_sizes(&sizes);

        let relation = |schema: &str, name: &str| {
            let schema = catalog.schemas.iter().find(|s| s.name == schema).unwrap();
            let relation = schema.relations.iter().find(|r| r.name == name).unwrap();
            (relation.size, relation.rows)
        };
        assert_eq!(relation("public", "accounts"), (Some(8192), Some(40)));
        assert_eq!(relation("public", "account_overview"), (None, None));
        assert_eq!(relation("archive", "accounts"), (None, None));
        assert_eq!(relation("archive", "unmeasured"), (None, Some(7)));

        let mut carried = catalog.clone();
        for relation in carried.schemas.iter_mut().flat_map(|s| &mut s.relations) {
            (relation.size, relation.rows) = (None, None);
        }
        carried.set_sizes(&catalog.sizes());
        assert_eq!(carried, catalog);
    }

    #[test]
    fn an_engine_that_selects_no_parent_column_assembles_anyway() {
        // What MySQL and SQLite send: neither has partitions to report, and
        // neither should have to coalesce a placeholder to say so. SQLite has
        // no sizes either.
        let relations = result(
            &["schema_name", "relation_name", "relation_kind"],
            &[&[Some("public"), Some("accounts"), Some("table")]],
        );
        let routines = result(&["schema_name", "routine_name", "routine_kind"], &[]);

        let catalog = assemble_catalog(relations, routines).unwrap();

        assert_eq!(catalog.schemas[0].relations[0].partition_of, None);
        assert_eq!(catalog.schemas[0].relations[0].size, None);
    }

    #[test]
    fn structure_reads_nullability_and_treats_a_blank_default_as_absent() {
        let columns = result(
            &["column_name", "data_type", "nullable", "column_default"],
            &[
                &[Some("id"), Some("bigint"), Some("no"), Some("nextval('s')")],
                &[Some("label"), Some("text"), Some("yes"), Some("")],
            ],
        );
        let indexes = result(
            &[
                "object_name",
                "index_kind",
                "index_columns",
                "index_predicate",
                "definition",
            ],
            &[
                &[
                    Some("accounts_pkey"),
                    Some("primary"),
                    Some("id"),
                    None,
                    Some("CREATE UNIQUE INDEX …"),
                ],
                &[
                    Some("accounts_label"),
                    Some("plain"),
                    Some("coalesce(label, id::text)\u{1f}id DESC"),
                    Some("label IS NOT NULL"),
                    Some("CREATE INDEX …"),
                ],
            ],
        );
        let constraints = result(
            &["object_name", "definition"],
            &[&[Some("accounts_pkey"), Some("PRIMARY KEY (id)")]],
        );

        let structure = assemble_structure(columns, indexes, constraints).unwrap();

        assert_eq!(
            structure.columns,
            vec![
                ColumnDefinition {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    default: Some("nextval('s')".into()),
                },
                ColumnDefinition {
                    name: "label".into(),
                    data_type: "text".into(),
                    nullable: true,
                    default: None,
                },
            ]
        );
        assert_eq!(
            structure.indexes,
            vec![
                Index {
                    name: "accounts_pkey".into(),
                    kind: IndexKind::Primary,
                    columns: vec!["id".into()],
                    predicate: None,
                    definition: "CREATE UNIQUE INDEX …".into(),
                },
                Index {
                    name: "accounts_label".into(),
                    kind: IndexKind::Plain,
                    columns: vec!["coalesce(label, id::text)".into(), "id DESC".into()],
                    predicate: Some("label IS NOT NULL".into()),
                    definition: "CREATE INDEX …".into(),
                },
            ]
        );
        assert_eq!(structure.constraints[0].definition, "PRIMARY KEY (id)");
    }

    #[test]
    fn a_composite_foreign_key_arrives_as_one_row_per_column() {
        let keys = result(
            &[
                "column_name",
                "referenced_schema",
                "referenced_table",
                "referenced_column",
            ],
            &[
                &[
                    Some("tenant_id"),
                    Some("public"),
                    Some("accounts"),
                    Some("tenant_id"),
                ],
                &[
                    Some("account_id"),
                    Some("public"),
                    Some("accounts"),
                    Some("id"),
                ],
            ],
        );

        assert_eq!(
            assemble_foreign_keys(&keys).unwrap(),
            vec![
                ForeignKey {
                    column: "tenant_id".into(),
                    referenced_schema: "public".into(),
                    referenced_table: "accounts".into(),
                    referenced_column: "tenant_id".into(),
                },
                ForeignKey {
                    column: "account_id".into(),
                    referenced_schema: "public".into(),
                    referenced_table: "accounts".into(),
                    referenced_column: "id".into(),
                },
            ]
        );
    }

    #[test]
    fn a_row_key_is_the_primary_key_else_a_unique_one_else_nothing() {
        let structure = |constraints: &[&str]| Structure {
            columns: ["id", "line, no", "code"]
                .map(|name| ColumnDefinition {
                    name: name.into(),
                    data_type: "int".into(),
                    nullable: false,
                    default: None,
                })
                .to_vec(),
            indexes: Vec::new(),
            constraints: constraints
                .iter()
                .map(|definition| NamedDefinition {
                    name: "k".into(),
                    definition: definition.to_string(),
                })
                .collect(),
            foreign_keys: Vec::new(),
            referenced_by: Vec::new(),
        };
        assert_eq!(
            structure(&["UNIQUE (code)", "PRIMARY KEY (\"id\", `code`)"]).primary_key(),
            ["id", "code"]
        );
        assert!(structure(&["UNIQUE (code)"]).primary_key().is_empty());
        assert_eq!(
            structure(&["UNIQUE (code)", "PRIMARY KEY (id, code)"]).row_key(),
            ["id", "code"]
        );
        assert_eq!(structure(&["UNIQUE (code)"]).row_key(), ["code"]);
        // `line, no` reads as two columns the relation does not have.
        assert!(
            structure(&["PRIMARY KEY (id, line, no)"])
                .row_key()
                .is_empty()
        );
        assert!(structure(&["CHECK (id > 0)"]).row_key().is_empty());
    }

    #[test]
    fn a_foreign_key_query_missing_a_column_is_an_error_not_a_guess() {
        let keys = result(
            &["column_name", "referenced_schema", "referenced_table"],
            &[&[Some("account_id"), Some("public"), Some("accounts")]],
        );

        let error = assemble_foreign_keys(&keys).unwrap_err();

        assert_eq!(
            error.message,
            "Catalog query omitted column referenced_column."
        );
    }

    #[test]
    fn the_rendered_constraints_survive_the_structured_form_arriving() {
        let columns = result(
            &["column_name", "data_type", "nullable", "column_default"],
            &[&[Some("id"), Some("bigint"), Some("no"), Some("")]],
        );
        let constraints = result(
            &["object_name", "definition"],
            &[&[
                Some("orders_account_id_fkey"),
                Some("FOREIGN KEY (account_id) REFERENCES public.accounts(id)"),
            ]],
        );

        let structure = assemble_structure(columns, QueryResult::default(), constraints).unwrap();

        assert_eq!(
            structure.constraints,
            vec![NamedDefinition {
                name: "orders_account_id_fkey".into(),
                definition: "FOREIGN KEY (account_id) REFERENCES public.accounts(id)".into(),
            }]
        );
        assert!(structure.foreign_keys.is_empty());
    }

    #[test]
    fn catalog_rejects_unknown_object_kinds() {
        let relations = result(
            &["schema_name", "relation_name", "relation_kind"],
            &[&[Some("public"), Some("mystery"), Some("unknown")]],
        );

        let error = assemble_catalog(relations, QueryResult::default()).unwrap_err();

        assert_eq!(
            error.message,
            "Catalog query returned unknown relation kind unknown."
        );
    }

    #[test]
    fn only_postgres_and_mysql_hold_read_only() {
        assert!(Engine::Postgres.holds_read_only());
        assert!(Engine::MySql.holds_read_only());
        assert!(!Engine::Sqlite.holds_read_only());
        assert!(!Engine::Snowflake.holds_read_only());
    }

    #[test]
    fn holding_read_only_is_unchanged_for_the_ten() {
        let held = |engine: Engine| engine.holds_read_only();
        for engine in [Engine::Postgres, Engine::MySql, Engine::MariaDb] {
            assert!(held(engine), "{engine:?}");
        }
        for engine in [
            Engine::Sqlite,
            Engine::Turso,
            Engine::DuckDb,
            Engine::Snowflake,
            Engine::SqlServer,
            Engine::MongoDb,
            Engine::Redis,
        ] {
            assert!(!held(engine), "{engine:?}");
        }
    }

    #[test]
    fn only_clickhouse_gives_another_reason_for_refusing_the_unreadable() {
        for engine in Engine::ALL {
            let expected = match engine {
                Engine::ClickHouse => {
                    "the server's read-only setting does not stop every statement of this kind"
                }
                _ => "this database has no server-side read-only setting to stop it if it writes",
            };
            assert_eq!(engine.unreadable_reason(), expected, "{engine:?}");
        }
    }

    #[test]
    fn a_cancelled_clickhouse_mutation_is_said_to_keep_running() {
        let error = plain_error(clickhouse::CANCELLED.into());
        let mutation = "ALTER TABLE t DELETE WHERE a = 1 SETTINGS mutations_sync = 2;";
        assert_eq!(
            cancel_cost(
                crate::db::Target::built_in(Engine::ClickHouse),
                &error,
                false,
                mutation
            )
            .as_deref(),
            Some("The mutation keeps running in the background; KILL MUTATION stops it.")
        );
        assert_eq!(
            cancel_cost(
                crate::db::Target::built_in(Engine::ClickHouse),
                &error,
                false,
                "INSERT INTO t SELECT 1"
            )
            .as_deref(),
            Some(
                "ClickHouse has no transaction to roll back: what the statement had already \
                 written stays."
            )
        );
        assert_eq!(
            cancel_cost(
                crate::db::Target::built_in(Engine::ClickHouse),
                &error,
                true,
                "SELECT 1"
            ),
            None
        );
        for engine in [Engine::MySql, Engine::MariaDb, Engine::Snowflake] {
            assert_eq!(
                cancel_cost(Target::built_in(engine), &error, false, mutation),
                None,
                "{engine:?}"
            );
        }
    }

    #[test]
    fn only_sqlite_has_no_read_only_statement() {
        assert_eq!(
            read_only_statement(Engine::Postgres, true),
            Some("SET default_transaction_read_only = on")
        );
        assert_eq!(
            read_only_statement(Engine::Postgres, false),
            Some("SET default_transaction_read_only = off")
        );
        assert_eq!(
            read_only_statement(Engine::MySql, true),
            Some("SET SESSION TRANSACTION READ ONLY")
        );
        assert_eq!(
            read_only_statement(Engine::MySql, false),
            Some("SET SESSION TRANSACTION READ WRITE")
        );
        assert_eq!(read_only_statement(Engine::Sqlite, true), None);
        assert_eq!(read_only_statement(Engine::Sqlite, false), None);
    }

    #[test]
    fn a_table_is_written_back_with_its_constraints_and_only_its_own_indexes() {
        let column =
            |name: &str, data_type: &str, nullable, default: Option<&str>| ColumnDefinition {
                name: name.into(),
                data_type: data_type.into(),
                nullable,
                default: default.map(str::to_string),
            };
        let named = |name: &str, definition: &str| NamedDefinition {
            name: name.into(),
            definition: definition.into(),
        };
        let structure = Structure {
            columns: vec![
                column("id", "bigint", false, Some("IDENTITY(1,1)")),
                column("note", "nvarchar(40)", true, Some("(N'x')")),
                column("twice", "int", true, Some("AS ([id]*(2))")),
            ],
            indexes: [("pk", IndexKind::Primary), ("by_note", IndexKind::Plain)]
                .map(|(name, kind)| Index {
                    name: name.into(),
                    kind,
                    columns: Vec::new(),
                    predicate: None,
                    definition: String::new(),
                })
                .to_vec(),
            constraints: vec![
                named("pk", "PRIMARY KEY (id)"),
                named("by_note", "CHECK (note <> N'')"),
            ],
            ..Structure::default()
        };

        assert_eq!(
            create_table(
                Engine::SqlServer,
                "CREATE TABLE \"dbo\".\"t\"",
                &structure,
                "",
                |index| format!("CREATE INDEX {}", index.name),
            ),
            "CREATE TABLE \"dbo\".\"t\" (\n    \"id\" bigint IDENTITY(1,1) NOT NULL,\n    \
             \"note\" nvarchar(40) DEFAULT (N'x'),\n    \"twice\" AS ([id]*(2)),\n    \
             CONSTRAINT \"pk\" PRIMARY KEY (id),\n    \
             CONSTRAINT \"by_note\" CHECK (note <> N'')\n);\nCREATE INDEX by_note;"
        );
    }

    #[test]
    fn a_plugin_is_offered_only_dialects_whose_hold_it_can_carry() {
        let offered: Vec<_> = Engine::ALL
            .into_iter()
            .filter(|engine| engine.speaks_through_plugins())
            .collect();
        for engine in &offered {
            assert_eq!(
                engine.holds_read_only(),
                read_only_statement(*engine, true).is_some(),
                "{engine:?}"
            );
        }
        for engine in [Engine::ClickHouse, Engine::MongoDb, Engine::Redis] {
            assert!(!offered.contains(&engine), "{engine:?}");
        }
        assert!(offered.contains(&Engine::Generic));
    }

    #[test]
    fn read_only_statements_are_recorded_per_engine() {
        let mut out = String::new();
        for engine in Engine::ALL {
            for read_only in [true, false] {
                crate::sql_snapshots::fragment(
                    &mut out,
                    engine,
                    &format!("read_only={read_only}"),
                    read_only_statement(engine, read_only),
                );
            }
        }
        crate::sql_snapshots::check("spellings_read_only", &out);
    }

    /// Only the engines whose session takes a time zone by a statement have
    /// one; ClickHouse sends it on each request instead, so it has none here.
    #[test]
    fn time_zone_statements_are_recorded_per_engine() {
        for engine in Engine::ALL {
            let expected = match engine {
                Engine::Postgres => Some("SET TIME ZONE 'Europe/Paris'"),
                Engine::MySql | Engine::MariaDb => Some("SET time_zone = 'Europe/Paris'"),
                _ => None,
            };
            assert_eq!(
                time_zone_statement(engine, "Europe/Paris").as_deref(),
                expected,
                "{engine:?}"
            );
        }
    }

    /// The script is the user's session's: every session the driver opens
    /// beside theirs replays it, and a connection opened from the config alone
    /// never sees it.
    #[test]
    fn a_startup_script_replays_on_every_user_session() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-startup-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the directory is creatable");
        let path = directory.join("startup.db");
        rusqlite::Connection::open(&path).expect("the file is creatable");
        let config = ConnectionConfig::Sqlite {
            path: path.display().to_string(),
            statement_timeout: 0,
        };
        let cache_size = |connection: &Connection| {
            let result = connection
                .query(
                    "PRAGMA cache_size",
                    &CancelToken::default(),
                    Fetch::default(),
                )
                .expect("the pragma reads");
            result.rows[0].get(0).map(str::to_string)
        };

        let plain = Connection::open(config.clone()).expect("it opens");
        let default = cache_size(&plain);
        assert_ne!(default.as_deref(), Some("4000"));
        let unchanged = Connection::open(config.clone())
            .and_then(|connection| connection.with_startup(vec![]))
            .expect("an empty script runs nothing");
        assert_eq!(cache_size(&unchanged), default);

        let scripted = Connection::open(config.clone())
            .and_then(|connection| connection.with_startup(vec!["PRAGMA cache_size = 4000".into()]))
            .expect("the script runs");
        assert_eq!(cache_size(&scripted).as_deref(), Some("4000"));
        let apart = scripted.apart().expect("a session beside it opens");
        assert_eq!(cache_size(&apart).as_deref(), Some("4000"));
        let beside = scripted
            .query_alongside(
                "PRAGMA cache_size",
                &CancelToken::default(),
                Fetch::default(),
                false,
            )
            .expect("a statement runs beside it");
        assert_eq!(beside.rows[0].get(0), Some("4000"));

        let fresh = Connection::open(config).expect("it opens");
        assert_eq!(
            cache_size(&fresh),
            default,
            "a connection of its own has none"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}

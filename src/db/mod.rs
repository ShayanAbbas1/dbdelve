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

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::tls::SslMode;

mod mysql;
mod postgres;
mod snowflake;
mod sqlite;

pub use snowflake::{SnowflakeConfig, account_identifier};

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
    Sqlite,
    Snowflake,
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
}

/// How much the server should be asked to do to answer "how would you run
/// this?".
///
/// The distinction is not a detail of presentation: `Plan` only plans, while
/// `Analyze` *runs the statement* to report what it really cost. Explaining a
/// `DELETE` under `Analyze` deletes. That is why the two are a choice the user
/// makes in front of the button rather than a mode dbdelve picks for them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
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

impl Engine {
    /// Presentation order, which is the order the form's chips appear in.
    pub const ALL: [Self; 4] = [Self::Postgres, Self::MySql, Self::Sqlite, Self::Snowflake];

    pub fn label(self) -> &'static str {
        match self {
            Self::Postgres => "Postgres",
            Self::MySql => "MySQL",
            Self::Sqlite => "SQLite",
            Self::Snowflake => "Snowflake",
        }
    }

    /// The spelling stored in `profiles.toml`. Changing one of these strings
    /// orphans every profile already written with it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
            Self::Sqlite => "sqlite",
            Self::Snowflake => "snowflake",
        }
    }

    /// Accepts the URL schemes as well as the stored spellings, so one function
    /// serves both the profile reader and the URL box.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlite" | "sqlite3" | "file" => Ok(Self::Sqlite),
            "snowflake" => Ok(Self::Snowflake),
            other => Err(format!("{other} is not a database engine dbdelve speaks.")),
        }
    }

    /// Which set of fields makes a connection to this engine, which is what
    /// the form asks before drawing any. A server's host, credentials and TLS
    /// are absent for a file, and an account has a key where a server has a
    /// password and no transport to choose.
    pub fn fields(self) -> Fields {
        match self {
            Self::Postgres | Self::MySql => Fields::Server,
            Self::Sqlite => Fields::File,
            Self::Snowflake => Fields::Account,
        }
    }

    /// What `mode` is spelled as here, or `None` where the engine has no such
    /// mode. Returned as a prefix because that is the whole of the difference:
    /// the statement it is put in front of is the user's, unchanged.
    ///
    /// SQLite has no `ExplainMode::Analyze`. Its bare `EXPLAIN` lists bytecode
    /// rather than a plan, so the plan-only mode is `EXPLAIN QUERY PLAN`, and
    /// there is no form that reports what a run actually cost. `None` is what
    /// keeps the menu from offering a mode that would only produce an error.
    ///
    /// MySQL's `EXPLAIN ANALYZE` arrived in 8.0.18, and MariaDB spells it
    /// `ANALYZE` with no `EXPLAIN`. Neither is detected: an older server
    /// refuses the statement and says so, which is the error the user needs and
    /// is hard rule 6's business rather than a version check's.
    pub fn explain_prefix(self, mode: ExplainMode) -> Option<&'static str> {
        match (self, mode) {
            (Self::Postgres | Self::MySql, ExplainMode::Plan) => Some("EXPLAIN "),
            (Self::Postgres | Self::MySql, ExplainMode::Analyze) => Some("EXPLAIN ANALYZE "),
            (Self::Sqlite, ExplainMode::Plan) => Some("EXPLAIN QUERY PLAN "),
            (Self::Sqlite, ExplainMode::Analyze) => None,
            // Its plan is a fourth shape `explain.rs` does not read yet.
            (Self::Snowflake, _) => None,
        }
    }

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` for the engine that already makes one submission
    /// atomic. `COMMIT` and `ROLLBACK` are spelled the same everywhere, so the
    /// opening word is the whole of the difference.
    ///
    /// MySQL's own spelling is `START TRANSACTION`, and `BEGIN` is its
    /// documented alias outside a stored program. The alias is what dbdelve
    /// writes because the brackets go into the statement text, where
    /// `sql::is_generated_write` has to read them back: the tree-sitter
    /// grammar has no `START TRANSACTION`, so the gate would refuse dbdelve's own
    /// batch.
    pub fn transaction_start(self) -> Option<&'static str> {
        match self {
            // One simple-query submission is already one implicit transaction.
            Self::Postgres => None,
            Self::MySql => Some("BEGIN"),
            Self::Sqlite => Some("BEGIN"),
            // Every statement autocommits unless the submission brackets it.
            Self::Snowflake => Some("BEGIN"),
        }
    }

    /// Whether `SET column = DEFAULT` is an assignment this engine accepts.
    ///
    /// SQLite's `UPDATE` takes an expression on the right and `DEFAULT` is not
    /// one there, so the gesture has to be withheld rather than attempted. It
    /// answers here for the reason [`Engine::transaction_start`] does: the
    /// question is about which engine is connected, and rule 4 keeps every one
    /// of those inside `src/db/` — the caller asks, and never matches.
    pub fn assigns_default(self) -> bool {
        match self {
            Self::Postgres | Self::MySql | Self::Snowflake => true,
            Self::Sqlite => false,
        }
    }

    /// Postgres and SQLite take the standard's double quote. MySQL takes a
    /// backtick, which it accepts whether or not `ANSI_QUOTES` is set — a double
    /// quote there is a *string literal*, so quoting a MySQL identifier the
    /// standard way produces a statement that runs and means something else.
    fn identifier_quote(self) -> char {
        match self {
            // Snowflake folds an unquoted name to upper case and reads a quoted
            // one exactly, and the catalog reports names as stored -- so quoting
            // what the catalog said is always the name it meant.
            Self::Postgres | Self::Sqlite | Self::Snowflake => '"',
            Self::MySql => '`',
        }
    }

    pub fn quote_identifier(self, identifier: &str) -> String {
        let quote = self.identifier_quote();
        format!(
            "{quote}{}{quote}",
            identifier.replace(quote, &format!("{quote}{quote}"))
        )
    }

    /// The inverse, for reading back a name dbdelve wrote — matching a sort key in
    /// a statement to the column header it belongs to, say.
    ///
    /// Anything that is not a quoted identifier comes back unchanged: a bare
    /// position or a function call names no column, and pretending otherwise
    /// would light up the wrong header.
    pub fn unquote_identifier(self, expression: &str) -> String {
        let quote = self.identifier_quote();
        match expression
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            Some(inner) => inner.replace(&format!("{quote}{quote}"), &quote.to_string()),
            None => expression.to_string(),
        }
    }

    /// Doubling the quote is enough for two of them: neither Postgres nor
    /// SQLite reads a backslash as an escape, the first because
    /// `standard_conforming_strings` is on by default and the second because it
    /// has no such notion at all. MySQL does, unless `NO_BACKSLASH_ESCAPES` is
    /// set — which is again not dbdelve's to set — so a literal backslash has to
    /// survive as two.
    pub fn quote_literal(self, value: &str) -> String {
        match self {
            Self::Postgres | Self::Sqlite => format!("'{}'", value.replace('\'', "''")),
            // Snowflake reads a backslash as an escape too, and unconditionally.
            Self::MySql | Self::Snowflake => {
                format!("'{}'", value.replace('\\', r"\\").replace('\'', "''"))
            }
        }
    }

    pub fn qualified(self, schema: &str, name: &str) -> String {
        format!(
            "{}.{}",
            self.quote_identifier(schema),
            self.quote_identifier(name)
        )
    }
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
    ///   variable since 5.7.8, and MariaDB spells it differently, so asking for
    ///   a timeout there fails the connect rather than the statement.
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
}

impl ServerConfig {
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
    Postgres(ServerConfig),
    MySql(ServerConfig),
    Sqlite {
        path: String,
        statement_timeout: u32,
    },
    Snowflake(SnowflakeConfig),
}

impl ConnectionConfig {
    pub fn engine(&self) -> Engine {
        match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(_) => Engine::MySql,
            Self::Sqlite { .. } => Engine::Sqlite,
            Self::Snowflake(_) => Engine::Snowflake,
        }
    }

    /// The server half, for the callers that only have something to say when
    /// there is one — the credential fields, and the Keychain.
    pub fn server(&self) -> Option<&ServerConfig> {
        match self {
            Self::Postgres(server) | Self::MySql(server) => Some(server),
            Self::Sqlite { .. } | Self::Snowflake(_) => None,
        }
    }

    /// The same, for the one caller that fills the password in: connecting
    /// reads it from the Keychain, which the profile on disk never holds.
    pub fn server_mut(&mut self) -> Option<&mut ServerConfig> {
        match self {
            Self::Postgres(server) | Self::MySql(server) => Some(server),
            Self::Sqlite { .. } | Self::Snowflake(_) => None,
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
            Engine::Sqlite => sqlite::path_from_url(url).map(|path| Self::Sqlite {
                path,
                // A URL has nowhere to say it; the form is where it is set.
                statement_timeout: 0,
            }),
            // Nobody pastes a Snowflake URL, because there is no such form.
            Engine::Snowflake => {
                Err("Snowflake has no connection URL. Fill the fields in instead.".to_string())
            }
        }
    }

    /// The statement timeout in seconds, or 0 for none. One accessor rather
    /// than a match at every call site, since no caller above `src/db/` cares
    /// which variant is carrying it.
    pub fn statement_timeout(&self) -> u32 {
        match self {
            Self::Postgres(server) | Self::MySql(server) => server.statement_timeout,
            Self::Sqlite {
                statement_timeout, ..
            } => *statement_timeout,
            Self::Snowflake(account) => account.statement_timeout,
        }
    }

    /// Whether replacing `self` with `edited` has to be reconnected for.
    ///
    /// A blank password in `edited` is not a change: the edit form never shows
    /// what the Keychain holds, so blank there means "leave it alone" -- taken
    /// literally it would drop and reopen the connection every time a colour
    /// was saved. A password that was typed does count, since applying it is
    /// the only reason to type one.
    pub fn needs_reconnect(&self, edited: &Self) -> bool {
        let mut current = self.clone();
        if let Some(server) = current.server_mut()
            && edited
                .server()
                .is_some_and(|server| server.password.is_empty())
        {
            server.password.clear();
        }
        current != *edited
    }

    /// What was being talked to, for an error or a title to name.
    pub fn endpoint(&self) -> String {
        match self {
            Self::Postgres(server) | Self::MySql(server) => server.endpoint(),
            Self::Sqlite { path, .. } => path.clone(),
            Self::Snowflake(account) => account.host(),
        }
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
#[derive(Clone)]
pub enum Connection {
    Postgres(postgres::Connection),
    MySql(mysql::Connection),
    Sqlite(sqlite::Connection),
    Snowflake(snowflake::Connection),
}

impl Connection {
    pub fn open(config: ConnectionConfig) -> Result<Self, DbError> {
        match config {
            ConnectionConfig::Postgres(server) => {
                postgres::Connection::open(&server).map(Self::Postgres)
            }
            ConnectionConfig::MySql(server) => mysql::Connection::open(&server).map(Self::MySql),
            ConnectionConfig::Sqlite {
                path,
                statement_timeout,
            } => sqlite::Connection::open(&path, statement_timeout).map(Self::Sqlite),
            ConnectionConfig::Snowflake(account) => {
                snowflake::Connection::open(&account).map(Self::Snowflake)
            }
        }
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. Row
    /// limits belong to the caller that *generated* a query, never to one the
    /// user typed.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Self::Postgres(connection) => connection.query(sql),
            Self::MySql(connection) => connection.query(sql),
            Self::Sqlite(connection) => connection.query(sql),
            Self::Snowflake(connection) => connection.query(sql),
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
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        match self {
            Self::Postgres(connection) => connection.catalog(),
            Self::MySql(connection) => connection.catalog(),
            Self::Sqlite(connection) => connection.catalog(),
            Self::Snowflake(connection) => connection.catalog(),
        }
    }

    /// The stored functions and procedures, as a catalog holding nothing else,
    /// for [`Catalog::merge`] to fold into the one already on screen.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        match self {
            Self::Postgres(connection) => connection.routines(),
            Self::MySql(connection) => connection.routines(),
            Self::Sqlite(connection) => connection.routines(),
            Self::Snowflake(connection) => connection.routines(),
        }
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        match self {
            Self::Postgres(connection) => connection.structure(schema, relation),
            Self::MySql(connection) => connection.structure(schema, relation),
            Self::Sqlite(connection) => connection.structure(schema, relation),
            Self::Snowflake(connection) => connection.structure(schema, relation),
        }
    }

    /// Ask the server to stop whatever this connection is running.
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
    pub fn cancel(&self) -> Result<(), DbError> {
        match self {
            Self::Postgres(connection) => connection.cancel(),
            Self::MySql(connection) => connection.cancel(),
            Self::Sqlite(connection) => connection.cancel(),
            Self::Snowflake(connection) => connection.cancel(),
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
        let engine = match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(_) => Engine::MySql,
            Self::Sqlite(_) => Engine::Sqlite,
            Self::Snowflake(_) => Engine::Snowflake,
        };
        let Some(statement) = read_only_statement(engine, read_only) else {
            return Ok(());
        };
        self.query(statement).map(|_| ())
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

/// Whether a [`Column::data_type`] names a type whose values are bytes.
///
/// Three engines' spellings in one predicate rather than three, because nothing
/// above this module is allowed to know which engine answered (AGENTS.md, hard
/// rule 4). Substrings because the families are open-ended in two directions:
/// MySQL prefixes its blobs and binaries, and SQLite gives BLOB affinity to any
/// declared type merely *containing* `blob`.
///
/// What it is for: a blob is rendered as the engine's own literal — `x'AB'`,
/// `0xAB` — and [`Engine::quote_literal`] would quote that back as the six
/// characters it looks like, so an edited blob column becomes text. The grid
/// refuses the edit instead. Absent here means unknown, not text: a driver that
/// could not name the type says nothing, and treating silence as binary would
/// make ordinary columns read-only.
pub fn is_binary_type(data_type: &str) -> bool {
    let name = data_type.to_ascii_lowercase();
    name == "bytea" || name.contains("blob") || name.contains("binary")
}

/// Whether a [`Column::data_type`] names a type whose values are numbers.
///
/// Exact names rather than the substrings [`is_binary_type`] can afford: the
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
            | "integer"
            | "tinyint"
            | "smallint"
            | "mediumint"
            | "bigint"
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
    )
}

/// A cell value, already formatted by the server. `None` is SQL NULL, which is
/// distinct from an empty string and must stay distinguishable in the grid.
pub type Cell = Option<String>;

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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
}

impl Catalog {
    /// Fold a later half into this one: the routines of a schema already here
    /// join it, and a schema that holds only routines is added in its place.
    ///
    /// Assembled by name rather than by position, because the two halves are
    /// separate queries and a schema can appear in either alone -- one holding
    /// only functions is in the second and not the first.
    pub fn merge(&mut self, other: Self) {
        for schema in other.schemas {
            match self
                .schemas
                .iter_mut()
                .find(|existing| existing.name == schema.name)
            {
                Some(existing) => existing.routines = schema.routines,
                None => self.schemas.push(schema),
            }
        }
        self.schemas.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

/// One relation's definition. Loaded when the relation is opened rather than at
/// connect: a database with thousands of relations would pay for every one of
/// them to show the columns of the one that was clicked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Structure {
    pub columns: Vec<ColumnDefinition>,
    pub indexes: Vec<NamedDefinition>,
    pub constraints: Vec<NamedDefinition>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
}

/// An index or a constraint, as the name plus the server's own rendering of it.
/// Postgres already prints both as readable DDL, so parsing them into fields
/// would only lose information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedDefinition {
    pub name: String,
    pub definition: String,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// Total bytes of returned cell text. Shown in the status bar so the cost
    /// of a wide or geometry-heavy result is visible rather than mysterious.
    pub bytes: usize,
    pub elapsed: Duration,
    /// The command's server-reported row count. The simple protocol reports
    /// zero both for commands that affected no rows and commands without a row
    /// count, so callers must not infer the command kind from this value.
    pub rows_affected: Option<u64>,
    /// Where these rows can be written back to, when they can be at all.
    /// `None` is the answer for every result set dbdelve cannot address a single
    /// row of, and it is not an error — see [`Connection::edit_target`].
    pub edit: Option<EditTarget>,
}

/// The table a result set's rows can be written back to, already resolved to
/// names and result-column positions.
///
/// The identity work — which oid, which attribute number — happens inside this
/// module and stops here (hard rule 4). A caller gets an answer it can build
/// SQL from, not a puzzle it has to ask the catalog about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// The real column name behind each result column, positionally. `None`
    /// where the result column is computed rather than read from the table, so
    /// `SELECT id AS ident, count(*)` gives `[Some("id"), None]`.
    pub columns: Vec<Option<String>>,
    /// Result-column indices that together identify one row. Never empty.
    pub keys: Vec<usize>,
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
            kind => return Err(unexpected_catalog_value("relation kind", kind)),
        };

        schema(&mut schemas, schema_name).relations.push(Relation {
            name: name.to_string(),
            kind,
            partition_of: optional_cell(&relations, row, "partition_of").map(str::to_string),
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
    })
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

    for (result, into) in [
        (&indexes, &mut structure.indexes),
        (&constraints, &mut structure.constraints),
    ] {
        for row in &result.rows {
            into.push(NamedDefinition {
                name: required_cell(result, row, "object_name")?.to_string(),
                definition: required_cell(result, row, "definition")?.to_string(),
            });
        }
    }

    Ok(structure)
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

fn schema<'a>(
    schemas: &'a mut std::collections::BTreeMap<String, Schema>,
    name: &str,
) -> &'a mut Schema {
    schemas.entry(name.to_string()).or_insert_with(|| Schema {
        name: name.to_string(),
        relations: Vec::new(),
        routines: Vec::new(),
    })
}

pub(super) fn required_cell<'a>(
    result: &'a QueryResult,
    row: &'a [Cell],
    column_name: &str,
) -> Result<&'a str, DbError> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == column_name)
        .ok_or_else(|| plain_error(format!("Catalog query omitted column {column_name}.")))?;

    row.get(index)
        .and_then(Option::as_deref)
        .ok_or_else(|| plain_error(format!("Catalog query returned no {column_name}.")))
}

/// A catalog column an engine may have nothing to say about. A missing column
/// and a null read the same, so an engine without the concept says so by not
/// selecting it rather than by coalescing a placeholder.
fn optional_cell<'a>(
    result: &'a QueryResult,
    row: &'a [Cell],
    column_name: &str,
) -> Option<&'a str> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == column_name)?;

    row.get(index)?.as_deref().filter(|value| !value.is_empty())
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

pub(super) fn plain_error(message: String) -> DbError {
    DbError {
        message,
        position: None,
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
    match (engine, read_only) {
        (Engine::Postgres, true) => Some("SET default_transaction_read_only = on"),
        (Engine::Postgres, false) => Some("SET default_transaction_read_only = off"),
        (Engine::MySql, true) => Some("SET SESSION TRANSACTION READ ONLY"),
        (Engine::MySql, false) => Some("SET SESSION TRANSACTION READ WRITE"),
        (Engine::Sqlite, _) => None,
        // There is no session to set anything on.
        (Engine::Snowflake, _) => None,
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
            .map(|row| row.iter().map(|cell| cell.map(str::to_string)).collect())
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Named, not merely rejected: "invalid URL" leaves the user guessing
        // which part of it dbdelve objected to.
        let error = ConnectionConfig::from_url("mongodb://db.example.test/dbdelve").unwrap_err();
        assert!(error.contains("mongodb"), "{error}");
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
            ConnectionConfig::Postgres(ServerConfig::default())
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
        let stored = ConnectionConfig::Postgres(ServerConfig {
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
            ConnectionConfig::Postgres(server)
        };

        assert!(!stored.needs_reconnect(&stored.clone()));
        assert!(!stored.needs_reconnect(&edited(|_| {})));
        assert!(stored.needs_reconnect(&edited(|server| server.host = "elsewhere".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.port = Some(6432))));
        assert!(stored.needs_reconnect(&edited(|server| server.database = "other".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.user = "someone_else".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.password = "typed".into())));
        assert!(stored.needs_reconnect(&ConnectionConfig::MySql(stored.server().unwrap().clone())));
        assert!(stored.needs_reconnect(&ConnectionConfig::Sqlite {
            path: "/tmp/dbdelve.db".into(),
            statement_timeout: 0
        }));
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

        for engine in Engine::ALL {
            assert_eq!(
                engine.quote_literal("odd'value"),
                "'odd''value'",
                "{engine:?}"
            );
        }

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
    fn snowflake_is_stored_under_its_own_name_and_has_no_url() {
        assert_eq!(Engine::parse("snowflake"), Ok(Engine::Snowflake));
        assert_eq!(Engine::Snowflake.as_str(), "snowflake");
        assert!(ConnectionConfig::from_url("snowflake://account/db").is_err());
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

    fn schema(name: &str, relations: &[&str], routines: &[&str]) -> Schema {
        Schema {
            name: name.to_string(),
            relations: relations
                .iter()
                .map(|name| Relation {
                    name: (*name).to_string(),
                    kind: RelationKind::Table,
                    partition_of: None,
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
        }
    }

    #[test]
    fn the_second_half_of_a_catalog_joins_the_first_by_name() {
        let mut catalog = Catalog {
            schemas: vec![
                schema("public", &["accounts"], &[]),
                schema("ops", &["jobs"], &[]),
            ],
        };
        catalog.merge(Catalog {
            schemas: vec![
                schema("public", &[], &["digest"]),
                // A schema holding only functions is in the second half alone,
                // and is a schema the explorer has to show.
                schema("audit", &[], &["trail"]),
            ],
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
        // Still in one order, wherever each schema came from.
        assert_eq!(
            catalog
                .schemas
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["audit", "ops", "public"]
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
            ],
            &[
                &[
                    Some("analytics"),
                    Some("events"),
                    Some("partitioned_table"),
                    None,
                ],
                &[
                    Some("analytics"),
                    Some("events_2026"),
                    Some("table"),
                    Some("events"),
                ],
                &[Some("public"), Some("accounts"), Some("table"), None],
                &[Some("public"), Some("account_overview"), Some("view"), None],
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
                },
                Relation {
                    name: "events_2026".into(),
                    kind: RelationKind::Table,
                    partition_of: Some("events".into()),
                },
            ]
        );
        assert_eq!(catalog.schemas[0].routines[0].kind, RoutineKind::Procedure);
        assert_eq!(catalog.schemas[1].name, "public");
        assert_eq!(catalog.schemas[1].relations[1].kind, RelationKind::View);
        assert_eq!(catalog.schemas[1].routines[0].result_type, "text");
    }

    #[test]
    fn an_engine_that_selects_no_parent_column_assembles_anyway() {
        // What MySQL and SQLite send: neither has partitions to report, and
        // neither should have to coalesce a placeholder to say so.
        let relations = result(
            &["schema_name", "relation_name", "relation_kind"],
            &[&[Some("public"), Some("accounts"), Some("table")]],
        );
        let routines = result(&["schema_name", "routine_name", "routine_kind"], &[]);

        let catalog = assemble_catalog(relations, routines).unwrap();

        assert_eq!(catalog.schemas[0].relations[0].partition_of, None);
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
            &["object_name", "definition"],
            &[&[Some("accounts_pkey"), Some("CREATE UNIQUE INDEX …")]],
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
        assert_eq!(structure.indexes[0].name, "accounts_pkey");
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
}

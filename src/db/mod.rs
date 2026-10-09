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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::tls::SslMode;

mod mongo;
mod mssql;
mod mysql;
mod postgres;
mod snowflake;
mod sqlite;
mod ssh;

pub use mongo::MongoConfig;
pub use snowflake::{SnowflakeConfig, account_identifier, normalize_host};

/// One run's claim on Cancel: [`Connection::query`] runs under it, and
/// [`Connection::cancel`] stops only what ran under it.
///
/// Snowflake is why it exists. Every tab and every catalog load there is a
/// statement of its own in flight on the one connection at once, so a cancel
/// has to know whose to stop. Postgres, MySQL and SQLite run one statement at
/// a time behind their mutex and stop that one, whoever's it is.
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
}

impl CancelToken {
    /// For a run whose statements each go out on a connection of their own
    /// ([`Connection::query_alongside`]). A cancel under it stops those, and
    /// never what the connection it is asked on is running, which is some
    /// other tab's statement or nothing.
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

    /// Mark the run cancelled and hand back the connections it is running on,
    /// or `None` for a run on the profile's own.
    fn stop_alongside(&self) -> Option<Vec<Connection>> {
        let mut running = self.0.lock().ok()?;
        let connections = running
            .alongside
            .as_ref()?
            .iter()
            .flatten()
            .cloned()
            .collect();
        running.asked = true;
        Some(connections)
    }
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
    Snowflake,
    SqlServer,
    MongoDb,
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

/// What a query buffer is written in: what the editor highlights it as, what
/// completion reads it as, and whether a row can be copied as a statement that
/// recreates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Sql,
    /// mongosh statements, which are JavaScript expressions.
    Mongo,
}

impl Syntax {
    /// gpui-component's name for the grammar it highlights with.
    pub fn highlighter(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::Mongo => "javascript",
        }
    }

    pub fn line_comment(self) -> &'static str {
        match self {
            Self::Sql => "--",
            Self::Mongo => "//",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Sql => "Write SQL…",
            Self::Mongo => "Write a query, like db.collection.find({})…",
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
    pub const ALL: [Self; 7] = [
        Self::Postgres,
        Self::MySql,
        Self::MariaDb,
        Self::Sqlite,
        Self::Snowflake,
        Self::SqlServer,
        Self::MongoDb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Postgres => "Postgres",
            Self::MySql => "MySQL",
            Self::MariaDb => "MariaDB",
            Self::Sqlite => "SQLite",
            Self::Snowflake => "Snowflake",
            Self::SqlServer => "SQL Server",
            Self::MongoDb => "MongoDB",
        }
    }

    /// The spelling stored in `profiles.toml`. Changing one of these strings
    /// orphans every profile already written with it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
            Self::MariaDb => "mariadb",
            Self::Sqlite => "sqlite",
            Self::Snowflake => "snowflake",
            Self::SqlServer => "mssql",
            Self::MongoDb => "mongodb",
        }
    }

    /// Accepts the URL schemes as well as the stored spellings, so one function
    /// serves both the profile reader and the URL box.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Ok(Self::Postgres),
            "mysql" => Ok(Self::MySql),
            "mariadb" => Ok(Self::MariaDb),
            "sqlite" | "sqlite3" | "file" => Ok(Self::Sqlite),
            "snowflake" => Ok(Self::Snowflake),
            "mssql" | "sqlserver" => Ok(Self::SqlServer),
            "mongodb" | "mongodb+srv" => Ok(Self::MongoDb),
            other => Err(format!("{other} is not a database engine dbdelve speaks.")),
        }
    }

    /// The port a blank one means, where the engine has one.
    pub fn default_port(self) -> Option<u16> {
        match self {
            Self::Postgres => Some(postgres::DEFAULT_PORT),
            Self::MySql | Self::MariaDb => Some(mysql::DEFAULT_PORT),
            Self::SqlServer => Some(mssql::DEFAULT_PORT),
            Self::MongoDb => Some(mongo::DEFAULT_PORT),
            Self::Sqlite | Self::Snowflake => None,
        }
    }

    /// Which set of fields makes a connection to this engine, which is what
    /// the form asks before drawing any. A server's host, credentials and TLS
    /// are absent for a file, and an account has a key where a server has a
    /// password and no transport to choose.
    pub fn fields(self) -> Fields {
        match self {
            Self::Postgres | Self::MySql | Self::MariaDb | Self::SqlServer | Self::MongoDb => {
                Fields::Server
            }
            Self::Sqlite => Fields::File,
            Self::Snowflake => Fields::Account,
        }
    }

    /// Whether the form offers a field of driver options passed through as
    /// typed: MongoDB's connection-string options (`authSource`, `replicaSet`,
    /// `readPreference`, …), which outnumber any set of fields worth drawing.
    pub fn takes_options(self) -> bool {
        match self {
            Self::MongoDb => true,
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::SqlServer => false,
        }
    }

    /// Whether the host may instead be a DNS name whose SRV records list the
    /// servers (`mongodb+srv`), which the form offers as a toggle.
    pub fn resolves_srv(self) -> bool {
        match self {
            Self::MongoDb => true,
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::SqlServer => false,
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
    /// MySQL's `EXPLAIN ANALYZE` arrived in 8.0.18; MariaDB has none and spells
    /// the same thing `ANALYZE` with no `EXPLAIN`. Server versions are not
    /// detected: an older server refuses the statement and says so, which is
    /// the error the user needs and is hard rule 6's business rather than a
    /// version check's.
    pub fn explain_prefix(self, mode: ExplainMode) -> Option<&'static str> {
        match (self, mode) {
            (Self::Postgres | Self::MySql | Self::MariaDb, ExplainMode::Plan) => Some("EXPLAIN "),
            (Self::Postgres | Self::MySql, ExplainMode::Analyze) => Some("EXPLAIN ANALYZE "),
            (Self::MariaDb, ExplainMode::Analyze) => Some("ANALYZE "),
            (Self::Sqlite, ExplainMode::Plan) => Some("EXPLAIN QUERY PLAN "),
            (Self::Sqlite, ExplainMode::Analyze) => None,
            // Its plan is a fourth shape `explain.rs` does not read yet.
            (Self::Snowflake, _) => None,
            // SQL Server has no prefix form. A plan comes from `SET SHOWPLAN_XML
            // ON`, a session switch that has to be a batch of its own, so
            // Explain would mean changing the session around the user's
            // statement rather than putting a word on a copy of it.
            (Self::SqlServer, _) => None,
            // A plan there is `.explain()` on the end of the statement, which
            // is `explain_suffix`; nothing goes in front.
            (Self::MongoDb, _) => Some(""),
        }
    }

    /// What goes after the statement to put it in `mode`: nothing on the SQL
    /// engines, whose whole difference is the prefix, and the cursor's own
    /// `.explain(verbosity)` on MongoDB.
    pub fn explain_suffix(self, mode: ExplainMode) -> &'static str {
        match (self, mode) {
            (Self::MongoDb, ExplainMode::Plan) => r#".explain("queryPlanner")"#,
            (Self::MongoDb, ExplainMode::Analyze) => r#".explain("executionStats")"#,
            (
                Self::Postgres
                | Self::MySql
                | Self::MariaDb
                | Self::Sqlite
                | Self::Snowflake
                | Self::SqlServer,
                ExplainMode::Plan | ExplainMode::Analyze,
            ) => "",
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
            Self::MySql | Self::MariaDb => Some("BEGIN"),
            Self::Sqlite => Some("BEGIN"),
            // Every statement autocommits unless the submission brackets it.
            Self::Snowflake => Some("BEGIN"),
            // A bare `BEGIN` opens a statement block in T-SQL, not a
            // transaction. The gate's grammar reads this spelling too.
            Self::SqlServer => Some("BEGIN TRANSACTION"),
            // No statement opens one: a batch applies in order, and a failure
            // says which rows already did.
            Self::MongoDb => None,
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
            Self::Postgres | Self::MySql | Self::MariaDb | Self::Snowflake | Self::SqlServer => {
                true
            }
            // A document has no column defaults to fall back to.
            Self::Sqlite | Self::MongoDb => false,
        }
    }

    /// Whether New row may insert into a relation of `kind`. A SQL view can
    /// take an insert (an updatable view, or a trigger behind it), so the
    /// server is left to say; a MongoDB view never does.
    pub fn takes_inserts(self, kind: RelationKind) -> bool {
        match self {
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::SqlServer => true,
            Self::MongoDb => kind != RelationKind::View,
        }
    }

    /// Whether a preview page is ordered by the relation's key when nothing
    /// else sorts it. SQL Server's `OFFSET … FETCH` needs an `ORDER BY`, and
    /// one that orders nothing lets a parallel plan repeat or skip rows from
    /// one page to the next, so its first page waits for the structure that
    /// names the key (`sql::paged` writes it).
    pub fn pages_by_key(self) -> bool {
        match self {
            Self::SqlServer => true,
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::MongoDb => false,
        }
    }

    /// Whether the row count the catalog carries for a table is exact rather
    /// than the statistics' estimate. Snowflake keeps an exact `ROW_COUNT` per
    /// table; the others' numbers are as of the last time statistics were
    /// gathered, and SQLite keeps none.
    pub fn exact_row_estimates(self) -> bool {
        match self {
            Self::Snowflake => true,
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::SqlServer
            | Self::MongoDb => false,
        }
    }

    /// The aggregate a relation's row count is written with. T-SQL's `COUNT`
    /// is an `int` and fails past 2,147,483,647 rows, so SQL Server's is
    /// `COUNT_BIG`; every other engine's `COUNT` is already 64-bit.
    pub fn count_all(self) -> &'static str {
        match self {
            Self::SqlServer => "COUNT_BIG(*)",
            // ponytail: MongoDB writes no SQL, so this is never what its count
            // runs; the count statement it does run arrives with browsing.
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::MongoDb => "COUNT(*)",
        }
    }

    /// Whether the server holds a Read-only session to reads -- the backstop
    /// `read_only_statement` sets. Without one, a statement `sql::classify`
    /// cannot read has nothing behind it that would stop a write, so the gate
    /// refuses it in Read-only instead of offering to run it once.
    pub fn holds_read_only(self) -> bool {
        read_only_statement(self, true).is_some()
    }

    /// What the filter bar's raw input asks for.
    pub fn raw_filter_placeholder(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "SQL…",
            Syntax::Mongo => "{ status: \"active\" }",
        }
    }

    /// The column dropdown's entry that swaps the bar for the user's own filter.
    pub fn raw_filter_label(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "Raw SQL",
            Syntax::Mongo => "Raw filter",
        }
    }

    /// What a generated statement is called where one is shown for review.
    pub fn review_label(self) -> &'static str {
        match self.syntax() {
            Syntax::Sql => "Review SQL",
            Syntax::Mongo => "Review statement",
        }
    }

    pub fn syntax(self) -> Syntax {
        match self {
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::SqlServer => Syntax::Sql,
            Self::MongoDb => Syntax::Mongo,
        }
    }

    /// Whether one server holds several databases a profile can be moved
    /// between, which [`Connection::databases`] lists.
    pub fn switches_database(self) -> bool {
        matches!(
            self,
            Self::Postgres | Self::MySql | Self::MariaDb | Self::SqlServer | Self::MongoDb
        )
    }

    /// Postgres and SQLite take the standard's double quote. MySQL takes a
    /// backtick, which it accepts whether or not `ANSI_QUOTES` is set — a double
    /// quote there is a *string literal*, so quoting a MySQL identifier the
    /// standard way produces a statement that runs and means something else.
    ///
    /// `None` for MongoDB, whose field names are JavaScript strings, escaped
    /// with a backslash rather than doubled.
    fn identifier_quote(self) -> Option<char> {
        match self {
            // Snowflake folds an unquoted name to upper case and reads a quoted
            // one exactly, and the catalog reports names as stored -- so quoting
            // what the catalog said is always the name it meant.
            Self::Postgres | Self::Sqlite | Self::Snowflake => Some('"'),
            // Not T-SQL's own `[name]`: the grammar every gate in `sql.rs`
            // parses with has no brackets, and the pin does not move. The
            // standard quote names an identifier under `QUOTED_IDENTIFIER`,
            // which the driver's login turns on.
            Self::SqlServer => Some('"'),
            Self::MySql | Self::MariaDb => Some('`'),
            Self::MongoDb => None,
        }
    }

    pub fn quote_identifier(self, identifier: &str) -> String {
        match self.identifier_quote() {
            Some(quote) => format!(
                "{quote}{}{quote}",
                identifier.replace(quote, &format!("{quote}{quote}"))
            ),
            None => json_string(identifier),
        }
    }

    /// The inverse, for reading back a name dbdelve wrote — matching a sort key in
    /// a statement to the column header it belongs to, say.
    ///
    /// Anything that is not a quoted identifier comes back unchanged: a bare
    /// position or a function call names no column, and pretending otherwise
    /// would light up the wrong header.
    pub fn unquote_identifier(self, expression: &str) -> String {
        let Some(quote) = self.identifier_quote() else {
            return serde_json::from_str::<String>(expression)
                .unwrap_or_else(|_| expression.to_string());
        };
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
            Self::MySql | Self::MariaDb | Self::Snowflake => {
                format!("'{}'", value.replace('\\', r"\\").replace('\'', "''"))
            }
            // `N` because a bare literal is converted to the database's code
            // page first, and a character it lacks arrives as `?`. No
            // backslash escape in T-SQL.
            Self::SqlServer => format!("N'{}'", value.replace('\'', "''")),
            Self::MongoDb => json_string(value),
        }
    }

    /// [`Self::quote_literal`] for a value bound for a column of `data_type`,
    /// on the one engine where the type changes how its literal is spelled.
    ///
    /// SQL Server's binary value is its hex literal, bare: quoted, it is a
    /// string, compared by its UTF-16 bytes, and matches no row. Bare only when
    /// it is exactly such a literal, because this is the one way a value enters
    /// a statement unquoted. And `N` only where a column needs it: an
    /// `nvarchar` literal against a `varchar` key converts the column rather
    /// than itself, so the edit scans the index. An ASCII value reads the same
    /// in every code page, which is what makes dropping the `N` safe.
    pub fn quote_value(self, value: &str, data_type: Option<&str>) -> String {
        let (Self::SqlServer, Some(data_type)) = (self, data_type) else {
            return self.quote_literal(value);
        };
        let lowered = data_type.to_ascii_lowercase();
        let name = lowered.split('(').next().unwrap_or_default().trim();
        let hex = value
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("0x"))
            .and_then(|_| value.get(2..))
            .filter(|digits| {
                digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        let narrow = matches!(
            name,
            "char"
                | "varchar"
                | "text"
                | "bit"
                | "tinyint"
                | "smallint"
                | "int"
                | "bigint"
                | "decimal"
                | "numeric"
                | "real"
                | "float"
                | "money"
                | "smallmoney"
                | "date"
                | "time"
                | "datetime"
                | "datetime2"
                | "smalldatetime"
                | "datetimeoffset"
                | "uniqueidentifier"
        );
        if let Some(digits) = hex.filter(|_| self.is_binary_type(name)) {
            format!("0x{digits}")
        } else if matches!(name, "datetime" | "smalldatetime")
            && let Some(iso) = iso_datetime(value)
        {
            iso
        } else if narrow && value.is_ascii() {
            format!("'{}'", value.replace('\'', "''"))
        } else {
            self.quote_literal(value)
        }
    }

    /// Whether a [`Column::data_type`] names a type whose values are bytes.
    ///
    /// Substrings because the families are open-ended in two directions: MySQL
    /// prefixes its blobs and binaries, and SQLite gives BLOB affinity to any
    /// declared type merely *containing* `blob`. `image` only on SQL Server,
    /// where it is the legacy binary type; elsewhere it is a name anyone may
    /// give a column or a domain, and a SQLite `image` column holds text.
    ///
    /// What it is for: a blob is rendered as the engine's own literal — `x'AB'`,
    /// `0xAB` — and [`Engine::quote_literal`] would quote that back as the six
    /// characters it looks like, so an edited blob column becomes text. The grid
    /// refuses the edit instead. Absent here means unknown, not text: a driver
    /// that could not name the type says nothing, and treating silence as binary
    /// would make ordinary columns read-only.
    pub fn is_binary_type(self, data_type: &str) -> bool {
        let name = data_type.to_ascii_lowercase();
        name == "bytea"
            || name.contains("blob")
            || name.contains("binary")
            || (self == Self::SqlServer
                && matches!(name.as_str(), "image" | "rowversion" | "timestamp"))
            || (self == Self::MongoDb && name == "bindata")
    }

    /// What a header click adds to a statement, as a notice names it.
    pub fn sort_clause(self) -> &'static str {
        match self {
            Self::Postgres
            | Self::MySql
            | Self::MariaDb
            | Self::Sqlite
            | Self::Snowflake
            | Self::SqlServer => "an ORDER BY",
            Self::MongoDb => "a sort",
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

/// A JavaScript string literal, which a JSON string always is.
fn json_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// A `datetime` value as `'YYYY-MM-DDThh:mm:ss…'`, the one spelling of it
/// `DATEFORMAT` and `SET LANGUAGE` never reorder: with a space instead of the
/// `T`, or as a bare date, `british` reads `2024-01-02` as the first of
/// February. `None` for anything not shaped like the grid's own rendering.
fn iso_datetime(value: &str) -> Option<String> {
    // Every byte of the shape is ASCII, so matching bytes also keeps the
    // slices below on character boundaries.
    let shaped = |text: &[u8], shape: &[u8]| {
        text.len() == shape.len()
            && text.iter().zip(shape).all(|(byte, want)| match want {
                b'9' => byte.is_ascii_digit(),
                _ => byte == want,
            })
    };
    let bytes = value.as_bytes();
    if bytes.len() < 10 || !shaped(&bytes[..10], b"9999-99-99") {
        return None;
    }
    let time = match &bytes[10..] {
        [] => b" 00:00:00".as_slice(),
        rest => rest,
    };
    let clock = time.len() >= 9 && shaped(&time[..9], b" 99:99:99");
    let fraction = &time[time.len().min(9)..];
    let fraction_ok = match fraction {
        [] => true,
        [b'.', digits @ ..] => !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    let time = value
        .get(11..)
        .filter(|time| !time.is_empty())
        .unwrap_or("00:00:00");
    (clock && fraction_ok).then(|| format!("'{}T{time}'", &value[..10]))
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
        sslmode,
        root_certificate,
        // A URL has nowhere to say it; the form is where it is set.
        statement_timeout: 0,
        // Same reason: a URL has nowhere to say it either.
        ssh: None,
    })
}

/// A local port forward opened over the system `ssh` binary before the engine
/// dials in. Nothing here is a secret: the private key stays on disk and the
/// password, if any, stays with ssh-agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnel {
    /// A hostname, or an alias `~/.ssh/config` resolves.
    pub host: String,
    /// None defers to `~/.ssh/config`, or 22.
    #[serde(default)]
    pub port: Option<u16>,
    /// Blank defers to `~/.ssh/config`, or the local user.
    #[serde(default)]
    pub user: String,
    /// None defers to `~/.ssh/config`, or ssh-agent. No password field: v1 is
    /// keys and agent only.
    #[serde(default)]
    pub identity_file: Option<String>,
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
    MariaDb(ServerConfig),
    SqlServer(ServerConfig),
    Sqlite {
        path: String,
        statement_timeout: u32,
    },
    Snowflake(SnowflakeConfig),
    MongoDb(MongoConfig),
}

impl ConnectionConfig {
    pub fn engine(&self) -> Engine {
        match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(_) => Engine::MySql,
            Self::MariaDb(_) => Engine::MariaDb,
            Self::SqlServer(_) => Engine::SqlServer,
            Self::Sqlite { .. } => Engine::Sqlite,
            Self::Snowflake(_) => Engine::Snowflake,
            Self::MongoDb(_) => Engine::MongoDb,
        }
    }

    /// The server half, for the callers that only have something to say when
    /// there is one — the credential fields, and the Keychain.
    pub fn server(&self) -> Option<&ServerConfig> {
        match self {
            Self::Postgres(server)
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server) => Some(server),
            Self::MongoDb(mongo) => Some(&mongo.server),
            Self::Sqlite { .. } | Self::Snowflake(_) => None,
        }
    }

    /// The same, for the one caller that fills the password in: connecting
    /// reads it from the Keychain, which the profile on disk never holds.
    pub fn server_mut(&mut self) -> Option<&mut ServerConfig> {
        match self {
            Self::Postgres(server)
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server) => Some(server),
            Self::MongoDb(mongo) => Some(&mut mongo.server),
            Self::Sqlite { .. } | Self::Snowflake(_) => None,
        }
    }

    /// Move the profile onto another database on the same server.
    pub fn set_database(&mut self, name: String) {
        match self {
            Self::Postgres(server)
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server) => server.database = name,
            Self::MongoDb(mongo) => mongo.set_database(name),
            Self::Sqlite { .. } | Self::Snowflake(_) => {}
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
            Self::Postgres(server)
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server) => server.statement_timeout,
            Self::MongoDb(mongo) => mongo.server.statement_timeout,
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
            Self::Postgres(server)
            | Self::MySql(server)
            | Self::MariaDb(server)
            | Self::SqlServer(server) => server.endpoint(),
            Self::MongoDb(mongo) => mongo.endpoint(),
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
    /// MariaDB's too: one driver, which holds the engine it was opened for.
    MySql(mysql::Connection),
    SqlServer(mssql::Connection),
    Sqlite(sqlite::Connection),
    Snowflake(snowflake::Connection),
    MongoDb(mongo::Connection),
}

impl Connection {
    pub fn open(config: ConnectionConfig) -> Result<Self, DbError> {
        match config {
            ConnectionConfig::Postgres(server) => {
                postgres::Connection::open(&server).map(Self::Postgres)
            }
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
            ConnectionConfig::Snowflake(account) => {
                snowflake::Connection::open(&account).map(Self::Snowflake)
            }
            ConnectionConfig::MongoDb(mongo) => mongo::Connection::open(&mongo).map(Self::MongoDb),
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
            Self::Snowflake(connection) => connection.query_with(sql, cancel, fetch),
            Self::MongoDb(connection) => connection.query(sql, cancel, fetch),
        }
    }

    /// [`Connection::query`] on a connection of its own, opened the way this
    /// one was (its tunnel and side session shared), so the statement runs
    /// beside whatever this one is running rather than queued behind it, and
    /// closed once it ends. Snowflake and MongoDB need no second one: they
    /// already run statements side by side, and stop them by `cancel`.
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
            Self::Snowflake(_) | Self::MongoDb(_) => return self.query(sql, cancel, fetch),
            Self::Postgres(connection) => connection.alongside().map(Self::Postgres),
            Self::MySql(connection) => connection.alongside().map(Self::MySql),
            Self::SqlServer(connection) => Ok(Self::SqlServer(connection.alongside())),
            Self::Sqlite(connection) => connection.alongside().map(Self::Sqlite),
        };
        // A server out of connections still has this one, which the statement
        // waits its turn on rather than failing.
        // ponytail: a Cancel does not reach a statement run this way; it is a
        // read, and ends on its own. SQL Server logs in on the first run, so
        // its refusal surfaces as the statement's error instead.
        let Ok(connection) = opened else {
            if cancel.0.lock().is_ok_and(|running| running.asked) {
                return Err(plain_error("Cancelled before it was sent.".into()));
            }
            return self.query(sql, cancel, fetch);
        };
        if read_only {
            connection.set_read_only(true)?;
        }
        let slot = {
            let mut running = cancel.0.lock().map_err(|_| {
                plain_error("The run is unavailable after an earlier internal failure.".into())
            })?;
            if running.asked {
                return Err(plain_error("Cancelled before it was sent.".into()));
            }
            let alongside = running.alongside.get_or_insert_default();
            alongside.push(Some(connection.clone()));
            alongside.len() - 1
        };
        let result = connection.query(sql, cancel, fetch);
        if let Ok(mut running) = cancel.0.lock()
            && let Some(alongside) = &mut running.alongside
        {
            alongside[slot] = None;
        }
        result
    }

    /// Run a statement dbdelve wrote at the user's ask -- a relation tab's
    /// preview, or an edit -- verbatim, as [`Connection::query`] does. SQL
    /// Server alone runs it differently: its session options are the user's to
    /// `SET`, and dbdelve's SQL is written for particular ones.
    ///
    /// A `feed` takes its rows as they arrive, as for [`Connection::query`];
    /// the statement carries its own `LIMIT`, so nothing else of [`Fetch`]
    /// applies.
    pub fn generated(
        &self,
        sql: &str,
        cancel: &CancelToken,
        feed: Option<&Feed>,
    ) -> Result<QueryResult, DbError> {
        match self {
            Self::SqlServer(connection) => connection.generated(
                sql,
                Fetch {
                    feed,
                    ..Fetch::default()
                },
            ),
            Self::Postgres(_)
            | Self::MySql(_)
            | Self::Sqlite(_)
            | Self::Snowflake(_)
            | Self::MongoDb(_) => self.query(
                sql,
                cancel,
                Fetch {
                    feed,
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
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        match self {
            Self::Postgres(connection) => connection.catalog(),
            Self::MySql(connection) => connection.catalog(),
            Self::SqlServer(connection) => connection.catalog(),
            Self::Sqlite(connection) => connection.catalog(),
            Self::Snowflake(connection) => connection.catalog(),
            Self::MongoDb(connection) => connection.catalog(),
        }
    }

    /// The stored functions and procedures, as a catalog holding nothing else,
    /// for [`Catalog::merge`] to fold into the one already on screen.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        match self {
            Self::Postgres(connection) => connection.routines(),
            Self::MySql(connection) => connection.routines(),
            Self::SqlServer(connection) => connection.routines(),
            Self::Sqlite(connection) => connection.routines(),
            Self::Snowflake(connection) => connection.routines(),
            Self::MongoDb(connection) => connection.routines(),
        }
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
    pub fn sizes(&self) -> Result<Sizes, DbError> {
        match self {
            Self::Postgres(connection) => connection.sizes(),
            Self::MySql(connection) => connection.sizes(),
            Self::MongoDb(connection) => connection.sizes(),
            Self::SqlServer(_) | Self::Sqlite(_) | Self::Snowflake(_) => Ok(Sizes::new()),
        }
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
            // Nothing declares one document's field a reference to another's.
            Self::Snowflake(_) | Self::MongoDb(_) => Ok(Vec::new()),
        }
    }

    /// The databases on the server, for a profile to be moved onto one. Empty
    /// where [`Engine::switches_database`] is false.
    pub fn databases(&self) -> Result<Databases, DbError> {
        match self {
            Self::Postgres(connection) => connection.databases(),
            Self::MySql(connection) => connection.databases(),
            Self::SqlServer(connection) => connection.databases(),
            Self::MongoDb(connection) => connection.databases(),
            Self::Sqlite(_) | Self::Snowflake(_) => Ok(Databases::default()),
        }
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        match self {
            Self::Postgres(connection) => connection.structure(schema, relation),
            Self::MySql(connection) => connection.structure(schema, relation),
            Self::SqlServer(connection) => connection.structure(schema, relation),
            Self::Sqlite(connection) => connection.structure(schema, relation),
            Self::Snowflake(connection) => connection.structure(schema, relation),
            Self::MongoDb(connection) => connection.structure(schema, relation),
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
            Self::Snowflake(connection) => connection.ddl(schema, relation, kind),
            Self::MongoDb(connection) => connection.ddl(schema, relation),
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
        if let Some(alongside) = cancel.stop_alongside()
            && !matches!(self, Self::Snowflake(_) | Self::MongoDb(_))
        {
            return alongside
                .iter()
                .map(|connection| connection.cancel(&CancelToken::default()))
                .fold(Ok(()), Result::and);
        }
        match self {
            Self::Postgres(connection) => connection.cancel(),
            Self::MySql(connection) => connection.cancel(),
            Self::SqlServer(connection) => connection.cancel(),
            Self::Sqlite(connection) => connection.cancel(),
            Self::Snowflake(connection) => connection.cancel(cancel),
            Self::MongoDb(connection) => connection.cancel(cancel),
        }
    }

    /// Whether the session behind a failed statement is gone, so nothing sent
    /// on it again can succeed. Asked only after a failure.
    ///
    /// Only the engines with one long-lived session can lose it: SQL Server
    /// reconnects inside its next run, MongoDB's driver redials from its pool,
    /// Snowflake speaks HTTP, and a SQLite file has no socket.
    pub fn is_lost(&self) -> bool {
        match self {
            Self::Postgres(connection) => connection.is_lost(),
            Self::MySql(connection) => connection.is_lost(),
            Self::SqlServer(_) | Self::Sqlite(_) | Self::Snowflake(_) | Self::MongoDb(_) => false,
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
            Self::MySql(connection) => connection.engine(),
            Self::SqlServer(_) => Engine::SqlServer,
            Self::Sqlite(_) => Engine::Sqlite,
            Self::Snowflake(_) => Engine::Snowflake,
            Self::MongoDb(_) => Engine::MongoDb,
        };
        let Some(statement) = read_only_statement(engine, read_only) else {
            return Ok(());
        };
        self.query(statement, &CancelToken::default(), Fetch::default())
            .map(|_| ())
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
    mongo::cell_type(name)
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
    /// Estimated on-disk bytes, mostly from the engine's stored statistics.
    /// `None` for a view, on SQLite, and on Postgres and MySQL until
    /// [`Catalog::set_sizes`] has filled it in, which it may never do.
    pub size: Option<u64>,
    /// How many rows the engine's statistics say the table holds, from the
    /// same place `size` comes from, so it is `None` wherever `size` is and
    /// also wherever the statistics were never gathered. An estimate unless
    /// [`Engine::exact_row_estimates`] says otherwise.
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
}

/// On-disk bytes and row estimates by schema, then relation name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Databases {
    pub names: Vec<String>,
    /// The one this connection is in. MySQL can be in none.
    pub current: Option<String>,
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
}

impl Catalog {
    /// Fold a later half into this one: the routines of a schema already here
    /// join it, and a schema that holds only routines is added in its place.
    ///
    /// Assembled by name rather than by position, because the two halves are
    /// separate queries and a schema can appear in either alone -- one holding
    /// only functions is in the second and not the first.
    ///
    /// A new schema goes on the end, never in name order: explorer ids and
    /// palette targets taken before the merge address schemas by index, and a
    /// sorted insert would point them at a neighbour. [`Self::by_name`] is the
    /// order to show them in.
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
    pub indexes: Vec<NamedDefinition>,
    pub constraints: Vec<NamedDefinition>,
    pub foreign_keys: Vec<ForeignKey>,
    /// The single-column foreign keys of other relations that point at this one.
    /// Filled by `load_structure` from [`Connection::references`], not by
    /// [`Connection::structure`]: completion calls that one per relation it
    /// sees, and has no use for who points back.
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
    pub rows: Vec<Vec<Cell>>,
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

/// Whether `error` is the user's Cancel landing, in the words each engine's
/// stop comes back in, rather than anything else that ended the statement while
/// the cancel was on its way. A server answering in another language reads as
/// not a cancel, which drops the rows instead of keeping them as a result.
pub fn is_cancel(engine: Engine, error: &DbError) -> bool {
    let message = error.message.as_str();
    match engine {
        Engine::Postgres => message == "canceling statement due to user request",
        Engine::MySql | Engine::MariaDb => message == "Query execution was interrupted",
        Engine::Sqlite => message == "interrupted",
        // The second is dbdelve's own, a cancel between partitions.
        Engine::Snowflake => matches!(message, "SQL execution canceled" | "Cancelled."),
        Engine::SqlServer => message.starts_with(mssql::CANCELLED),
        Engine::MongoDb => message == "Cancelled.",
    }
}

/// What a Cancel took beyond the statement it stopped, which the user has to
/// be told even where the rows it had are kept: on SQL Server the error
/// itself, which says the session was reset. Postgres cannot say whether a
/// transaction was open, and a cancel aborts the one it lands in; SQLite rolls
/// back the transaction an interrupted write was in. MySQL's `KILL QUERY` and
/// SQLite's interrupt of a read undo only the statement, and Snowflake has no
/// session to lose.
pub fn cancel_cost(engine: Engine, error: &DbError, reads: bool) -> Option<String> {
    match engine {
        Engine::SqlServer => Some(error.message.clone()),
        Engine::Postgres => Some(
            "A cancel aborts the transaction it lands in: if one was open, it refuses every \
             statement until a ROLLBACK."
                .into(),
        ),
        Engine::Sqlite if !reads => Some(
            "If the statement was inside a transaction, SQLite rolled the whole transaction \
             back."
                .into(),
        ),
        Engine::Sqlite | Engine::MySql | Engine::MariaDb | Engine::Snowflake | Engine::MongoDb => {
            None
        }
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
}

/// Rows handed over while a statement is still running, so the first of a
/// large result is on screen long before the last arrives. The driver pushes
/// and the grid takes; a row sits here only between the two.
#[derive(Clone, Default)]
pub struct Feed(Arc<Mutex<Fed>>);

/// What a [`Feed`] holds since it was last taken from.
#[derive(Default)]
pub struct Fed {
    /// Counts result sets. A taker whose rows came from an earlier set than
    /// this one has rows of a statement the submission has moved past.
    pub set: usize,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
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
    fn lock(&self) -> std::sync::MutexGuard<'_, Fed> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn begin(&self, columns: Vec<Column>) {
        let mut fed = self.lock();
        fed.set += 1;
        fed.columns = columns;
        fed.rows.clear();
        fed.filled = None;
    }

    pub(crate) fn push(&self, row: Vec<Cell>) {
        self.lock().rows.push(row);
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
        Fed {
            set: fed.set,
            columns,
            rows: std::mem::take(&mut fed.rows),
            types: Vec::new(),
            awaiting: None,
            filled: fed.filled,
        }
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
                let value = cells.get(column)?.as_deref()?;
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    index: impl Fn(&NamedDefinition) -> String,
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
        (Engine::MySql | Engine::MariaDb, true) => Some("SET SESSION TRANSACTION READ ONLY"),
        (Engine::MySql | Engine::MariaDb, false) => Some("SET SESSION TRANSACTION READ WRITE"),
        (Engine::Sqlite, _) => None,
        // There is no session to set anything on.
        (Engine::Snowflake, _) => None,
        // No session-level switch exists. `ApplicationIntent=ReadOnly` routes a
        // login to a readable replica and is ignored by a primary.
        (Engine::SqlServer, _) => None,
        // No session setting holds a client to reads; the classifier is the
        // boundary there.
        (Engine::MongoDb, _) => None,
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

/// The live checks every engine's `live_` tests make of a queue run at once,
/// shared because they are one claim about [`Connection::query_alongside`]:
/// the `Workspace` around it needs a window, and these need a server.
#[cfg(test)]
pub(super) mod at_once {
    use std::time::{Duration, Instant};

    use super::{CancelToken, Connection, DbError, Engine, Fetch, QueryResult, is_cancel};
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
            assert!(is_cancel(engine, &error), "{}", error.message);
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
        assert!(is_cancel(engine, &error), "{}", error.message);
        finished.expect("the statement beside it should run to its end");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(is_cancel(engine, &error(words)), "{engine:?}");
            assert!(!is_cancel(engine, &error("division by zero")), "{engine:?}");
        }
        // The statement had finished: a whole result, not a cancelled one.
        assert!(!is_cancel(
            Engine::SqlServer,
            &error("The statement finished, but Cancel arrived after that.")
        ));
        assert!(!is_cancel(
            Engine::MariaDb,
            &error("Query execution was interrupted (max_statement_time exceeded)")
        ));
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
    fn new_row_is_withheld_only_from_a_mongo_view() {
        for engine in Engine::ALL {
            assert!(engine.takes_inserts(RelationKind::Table), "{engine:?}");
            assert_eq!(
                engine.takes_inserts(RelationKind::View),
                engine != Engine::MongoDb,
                "{engine:?}"
            );
        }
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
        let error = ConnectionConfig::from_url("redis://db.example.test/0").unwrap_err();
        assert!(error.contains("redis"), "{error}");
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
            .filter(|e| !matches!(e, Engine::SqlServer | Engine::MongoDb))
        {
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
        }
    }

    #[test]
    fn the_second_half_of_a_catalog_joins_the_first_by_name() {
        let mut catalog = Catalog {
            schemas: vec![
                schema("ops", &["jobs"], &[]),
                schema("public", &["accounts"], &[]),
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
            indexes: vec![named("pk", "CLUSTERED INDEX (id)"), named("by_note", "x")],
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
}

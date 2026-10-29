//! What sets one engine apart from another, as data: one [`EngineSpec`] per
//! [`Engine`](super::Engine), read through [`Engine::spec`](super::Engine::spec).
//!
//! A field is a fact or a choice among strategies the code already has, never
//! SQL text: the statements themselves are written where they are used, and a
//! spec only says which spelling that code picks. A new engine that fits the
//! existing strategies is one more value here and its driver.

use super::{
    ColumnDefinition, Fields, RelationKind, Syntax, clickhouse, mongo, mssql, mysql, postgres,
    redis,
};

#[derive(Clone, Copy, Debug)]
pub struct EngineSpec {
    /// The spelling stored in `profiles.toml`. Changing one orphans every
    /// profile already written with it.
    pub id: &'static str,
    pub label: &'static str,
    /// Every spelling [`Engine::parse`](super::Engine::parse) reads: the URL
    /// schemes as well as `id`.
    pub schemes: &'static [&'static str],
    /// What the welcome surface tells a pasted path of this engine's by,
    /// whatever its case.
    pub file_extensions: &'static [&'static str],
    pub logo: &'static str,
    /// 0xRRGGBB. The theme reads only its hue.
    pub brand: u32,
    pub syntax: Syntax,
    pub layout: Fields,
    pub default_port: Option<u16>,
    pub takes_options: bool,
    pub resolves_srv: bool,
    pub system_databases: &'static [&'static str],
    /// Exact type names read as bytes beyond the families every engine shares.
    pub binary_types: &'static [&'static str],
    /// Exact lower-cased type names that are numbers beyond the families every
    /// engine shares, without their precision.
    pub numeric_types: &'static [&'static str],
    /// Wrappers that leave what a type holds unchanged, by the name that opens
    /// them: `nullable` for `Nullable(UInt64)`.
    pub type_wrappers: &'static [&'static str],
    pub hints: Hints,
    /// Meaningful only where `syntax` is [`Syntax::Sql`].
    pub sql: SqlSpec,
    pub abilities: Abilities,
    pub hooks: Hooks,
}

impl EngineSpec {
    pub fn quote_identifier(&self, identifier: &str) -> String {
        let quote = self.sql.identifier_quote.character();
        let identifier = match self.sql.identifier_backslash_escapes {
            true => identifier.replace('\\', r"\\"),
            false => identifier.to_string(),
        };
        format!(
            "{quote}{}{quote}",
            identifier.replace(quote, &format!("{quote}{quote}"))
        )
    }

    /// The inverse of [`Self::quote_identifier`]; anything that is not a
    /// quoted identifier comes back unchanged.
    pub fn unquote_identifier(&self, expression: &str) -> String {
        let quote = self.sql.identifier_quote.character();
        match expression
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            Some(inner) => {
                let inner = inner.replace(&format!("{quote}{quote}"), &quote.to_string());
                match self.sql.identifier_backslash_escapes {
                    true => inner.replace(r"\\", "\\"),
                    false => inner,
                }
            }
            None => expression.to_string(),
        }
    }

    pub fn qualified(&self, schema: &str, name: &str) -> String {
        format!(
            "{}.{}",
            self.quote_identifier(schema),
            self.quote_identifier(name)
        )
    }

    pub fn quote_literal(&self, value: &str) -> String {
        let value = match self.sql.backslash_escapes {
            true => value.replace('\\', r"\\"),
            false => value.to_string(),
        };
        let national = if self.sql.national_strings { "N" } else { "" };
        format!("{national}'{}'", value.replace('\'', "''"))
    }

    pub fn quote_value(&self, value: &str, data_type: Option<&str>) -> String {
        data_type
            .zip(self.hooks.quote_value)
            .and_then(|(data_type, typed)| typed(value, data_type))
            .unwrap_or_else(|| self.quote_literal(value))
    }

    /// Substrings because the families are open-ended in two directions:
    /// MySQL prefixes its blobs and binaries, and SQLite gives BLOB affinity to
    /// any declared type merely *containing* `blob`.
    pub fn is_binary_type(&self, data_type: &str) -> bool {
        let name = data_type.to_ascii_lowercase();
        name == "bytea"
            || name.contains("blob")
            || name.contains("binary")
            || self.binary_types.contains(&name.as_str())
    }

    /// [`Engine::is_numeric_type`](super::Engine::is_numeric_type). Wrappers
    /// come off first, so `Nullable(UInt64)` answers as `UInt64` does.
    pub fn is_numeric_type(&self, data_type: &str) -> bool {
        let mut name = data_type.to_ascii_lowercase();
        while let Some(inner) = self
            .type_wrappers
            .iter()
            .find_map(|wrapper| unwrap_type_wrapper(&name, wrapper))
        {
            name = inner.to_string();
        }
        let base = name
            .split_once('(')
            .map_or(name.as_str(), |(base, _)| base)
            .trim();
        super::is_numeric_type(&name) || self.numeric_types.contains(&base)
    }

    /// [`Engine::writes_back`](super::Engine::writes_back). The ability is
    /// asked apart from the types, so a cell nobody typed is no way past it.
    pub fn writes_back<'a>(&self, types: impl IntoIterator<Item = &'a str>) -> bool {
        self.abilities.edits
            && types.into_iter().all(|data_type| match self.syntax {
                Syntax::Redis => crate::redis_cmd::edit::writes_back(data_type),
                Syntax::Sql | Syntax::Mongo => !self.is_binary_type(data_type),
            })
    }

    /// [`Engine::takes_inserts`](super::Engine::takes_inserts).
    pub fn takes_inserts(&self, kind: RelationKind) -> bool {
        self.abilities.inserts && (kind != RelationKind::View || self.abilities.inserts_into_views)
    }

    /// [`Engine::generates`](super::Engine::generates).
    pub fn generates(&self, column: &ColumnDefinition) -> bool {
        let Some(default) = column.default.as_deref() else {
            return false;
        };
        self.sql
            .generated_defaults
            .iter()
            .any(|marker| match *marker {
                DefaultMarker::Exact(exact) => default == exact,
                DefaultMarker::Prefix(prefix) => default.starts_with(prefix),
            })
    }
}

fn unwrap_type_wrapper<'a>(name: &'a str, wrapper: &str) -> Option<&'a str> {
    name.strip_prefix(wrapper)?
        .strip_prefix('(')?
        .strip_suffix(')')
        .map(str::trim)
}

/// What the connection form says a blank field means.
#[derive(Clone, Copy, Debug)]
pub struct Hints {
    pub name: &'static str,
    pub host: &'static str,
    /// What a blank database connects to (`ConnectionConfig::database_name`)
    /// where the engine has one; elsewhere it leaves the server to pick.
    pub database: &'static str,
    pub user: &'static str,
    pub path: &'static str,
}

impl Hints {
    pub const DEFAULT: Self = Self {
        name: "Named after the database",
        host: "localhost",
        database: "Optional",
        user: "Required",
        path: "/path/to/app.sqlite",
    };
}

#[derive(Clone, Copy, Debug)]
pub struct SqlSpec {
    pub identifier_quote: IdentifierQuote,
    /// Whether a backslash in a single-quoted literal escapes the character
    /// after it, so a literal one has to survive as two. Postgres reads none
    /// because `standard_conforming_strings` is on by default, and SQLite has
    /// no such notion at all.
    pub backslash_escapes: bool,
    /// Whether a backslash inside a quoted identifier escapes the character
    /// after it, so a literal one has to survive as two.
    pub identifier_backslash_escapes: bool,
    /// String literals are written `N'…'`, because a bare one is converted to
    /// the database's code page first, and a character it lacks arrives as `?`.
    pub national_strings: bool,
    /// Whether a date literal may carry a UTC offset.
    pub literal_offsets: bool,
    /// How a boolean is spelled as an Import Rows value. MySQL reads `'true'`
    /// into a `tinyint` as an error in strict mode, and SQL Server's `bit` and
    /// SQLite's integers take `1` and `0` only.
    pub boolean_literal: BooleanLiteral,
    pub params: Params,
    pub paging: Paging,
    pub select_top: SelectTop,
    pub truncate: Truncate,
    pub foreign_table: ForeignTable,
    pub materialized_view: MaterializedViewDrop,
    pub count_all: CountAll,
    /// The word that opens a transaction.
    pub transaction: Transaction,
    /// One submission is already one transaction, so a generated batch
    /// needs no brackets.
    pub implicit_transaction: bool,
    pub skip_duplicates: SkipDuplicates,
    /// The markers the structure queries write into a column's default for
    /// one the server fills itself.
    pub generated_defaults: &'static [DefaultMarker],
    pub statement_split: StatementSplit,
    pub explain_plan: Option<ExplainPrefix>,
    pub explain_analyze: Option<ExplainPrefix>,
    pub typed_explain: TypedExplain,
    pub parser_dialect: ParserDialect,
    pub format_dialect: FormatDialect,
    pub regex: RegexMatch,
    pub substring: SubstringMatch,
    pub read_only_session: ReadOnlySession,
    pub unreadable_floor: UnreadableFloor,
    /// The table engines a `CREATE TABLE … ENGINE =` may name and stay a
    /// write: those that keep their rows in the database. Any other engine
    /// reads or writes somewhere else (a URL, a file, another server), so it
    /// needs Full. `None` where the `ENGINE` option is not read at all.
    pub local_table_engines: Option<&'static [&'static str]>,
    /// Nothing outlives one request, so no statement leaves state behind.
    pub stateless_requests: bool,
    /// Whether a transaction can be opened at all. Without one a generated
    /// multi-row batch is not bracketed, whatever `transaction` says.
    pub transactions: bool,
    /// Whether the write gate reads the terms a row named by every value
    /// carries: `toString(c) = '…'` and `c IS NULL`.
    pub row_terms: bool,
}

impl SqlSpec {
    pub const ANSI: Self = Self {
        identifier_quote: IdentifierQuote::Double,
        backslash_escapes: false,
        identifier_backslash_escapes: false,
        national_strings: false,
        literal_offsets: true,
        boolean_literal: BooleanLiteral::TrueFalse,
        params: Params::Question,
        paging: Paging::LimitOffset,
        select_top: SelectTop::Limit,
        truncate: Truncate::Truncate,
        foreign_table: ForeignTable::Foreign,
        materialized_view: MaterializedViewDrop::MaterializedView,
        count_all: CountAll::Count,
        transaction: Transaction::Begin,
        implicit_transaction: false,
        skip_duplicates: SkipDuplicates::None,
        generated_defaults: &[],
        statement_split: StatementSplit::Semicolon,
        explain_plan: None,
        explain_analyze: None,
        typed_explain: TypedExplain::None,
        parser_dialect: ParserDialect::Generic,
        format_dialect: FormatDialect::Generic,
        regex: RegexMatch::None,
        substring: SubstringMatch::Like,
        read_only_session: ReadOnlySession::None,
        unreadable_floor: UnreadableFloor::None,
        local_table_engines: None,
        stateless_requests: false,
        transactions: true,
        row_terms: false,
    };

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` where one submission is already atomic.
    pub fn transaction_opening(&self) -> Option<&'static str> {
        (self.transactions && !self.implicit_transaction).then(|| self.transaction.opening())
    }
}

/// No brackets, not even for SQL Server: the grammar every gate in `sql.rs`
/// parses with has none, and the pin does not move. The standard quote names
/// an identifier there under `QUOTED_IDENTIFIER`, which the driver's login
/// turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentifierQuote {
    Double,
    /// MySQL's, which it accepts whether or not `ANSI_QUOTES` is set — a
    /// double quote there is a *string literal*, so quoting a MySQL identifier
    /// the standard way produces a statement that runs and means something
    /// else.
    Backtick,
}

impl IdentifierQuote {
    fn character(self) -> char {
        match self {
            Self::Double => '"',
            Self::Backtick => '`',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BooleanLiteral {
    TrueFalse,
    OneZero,
}

impl BooleanLiteral {
    pub fn spelled(self, value: bool) -> &'static str {
        match (self, value) {
            (Self::TrueFalse, true) => "true",
            (Self::TrueFalse, false) => "false",
            (Self::OneZero, true) => "1",
            (Self::OneZero, false) => "0",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Params {
    Question,
    Dollar,
    AtP,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paging {
    LimitOffset,
    /// Needs an `ORDER BY`, and one that orders nothing lets a parallel plan
    /// repeat or skip rows from one page to the next, so a preview's first
    /// page waits for the structure that names the key.
    OffsetFetch,
    /// `LIMIT … OFFSET …` as the default, but a page of an unsorted preview is
    /// not stable without an `ORDER BY` (parallel reads, background merges), so
    /// the relation's key orders it and the first page waits for the structure.
    LimitOffsetByKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectTop {
    Limit,
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncate {
    Truncate,
    /// SQLite has no `TRUNCATE`.
    DeleteFrom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignTable {
    Foreign,
    External,
    /// ClickHouse has no foreign table, so the kind drops as a table.
    Table,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterializedViewDrop {
    MaterializedView,
    /// ClickHouse's `DROP MATERIALIZED VIEW` is a syntax error; its
    /// materialized views drop with `DROP VIEW`.
    View,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountAll {
    Count,
    /// T-SQL's `COUNT` is an `int` and fails past 2,147,483,647 rows; every
    /// other engine's is already 64-bit.
    CountBig,
}

impl CountAll {
    pub fn aggregate(self) -> &'static str {
        match self {
            Self::Count => "COUNT(*)",
            Self::CountBig => "COUNT_BIG(*)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transaction {
    /// MySQL's own spelling is `START TRANSACTION`, and `BEGIN` is its
    /// documented alias outside a stored program. The alias is what dbdelve
    /// writes because the brackets go into the statement text, where
    /// `sql::is_generated_write` has to read them back: the tree-sitter
    /// grammar has no `START TRANSACTION`, so the gate would refuse dbdelve's
    /// own batch.
    Begin,
    BeginTransaction,
}

impl Transaction {
    pub fn opening(self) -> &'static str {
        match self {
            Self::Begin => "BEGIN",
            Self::BeginTransaction => "BEGIN TRANSACTION",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipDuplicates {
    /// SQL Server and Snowflake have no such clause short of `MERGE`, and
    /// MongoDB's `insertMany` would stop at the first collision.
    None,
    OnConflictDoNothing {
        /// DuckDB refuses `ON CONFLICT DO NOTHING` on a table with no primary
        /// key, unique constraint or unique index, where the others take it as
        /// a clause with nothing to do.
        needs_unique_key: bool,
    },
    OnDuplicateKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultMarker {
    Exact(&'static str),
    Prefix(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementSplit {
    Semicolon,
    /// T-SQL batches separated by `GO` lines.
    Batches,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainPrefix {
    Explain,
    ExplainAnalyze,
    /// `ANALYZE <statement>` runs the statement, so `sql::classify` reads it as
    /// the statement it runs.
    Analyze,
    ExplainQueryPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedExplain {
    /// Turso and DuckDB have no Explain at all, and Snowflake's and SQL
    /// Server's plans are shapes the parser does not read.
    None,
    Postgres,
    MySql,
    Sqlite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserDialect {
    Generic,
    PostgreSql,
    MySql,
    Sqlite,
    DuckDb,
    Snowflake,
    MsSql,
    ClickHouse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatDialect {
    Generic,
    SqlServer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexMatch {
    /// The filter bar does not offer the operator. SQLite ships no `REGEXP`
    /// implementation, so the operator is a syntax error until an application
    /// registers the function (spec §7); SQL Server has none before 2025's
    /// `REGEXP_LIKE`.
    None,
    Tilde,
    RegexpLike,
    RegexpInstr,
    RegexpMatches,
    RegexpCount,
    /// ClickHouse's `match` reads text only, so the column goes through
    /// `toString`. Unanchored, like `~`.
    Match,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstringMatch {
    Like,
    LikeBracketEscape,
    Instr,
    /// DuckDB, unlike SQLite, reads no other type as text on its own: a date
    /// or a number has no `instr`.
    InstrCast,
    Contains,
    /// `LIKE` over `toString(column)`: ClickHouse's `LIKE` takes only a string
    /// or a `FixedString`, and `toString` keeps `Nullable`, so a NULL stays NULL.
    LikeToString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnlySession {
    None,
    DefaultTransaction,
    SessionTransaction,
    /// Sent as settings on each request rather than on a session. The user's
    /// own statement can still try to lift it, so the server's refusal is
    /// what stops that.
    PerRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnreadableFloor {
    /// A statement the parser cannot read is `Unreadable` and nothing more.
    None,
    /// A statement the parser cannot read is placed by its first keyword, and
    /// one whose first keyword is unknown fails closed.
    FirstKeyword,
}

#[derive(Clone, Copy, Debug)]
pub struct Abilities {
    pub edits: bool,
    pub inserts: bool,
    /// A SQL view can take an insert (an updatable view, or a trigger behind
    /// it), so the server is left to say; a MongoDB view never does.
    pub inserts_into_views: bool,
    pub imports: bool,
    pub assigns_default: bool,
    pub copies_ddl: bool,
    pub switches_database: bool,
    pub switches_database_in_session: bool,
    pub picks_schema: bool,
    pub exact_row_estimates: bool,
    pub schema_switch_waits_for_transaction: bool,
    /// Why no transaction spans an import's chunks.
    pub chunk_refusal: Option<&'static str>,
}

impl Abilities {
    /// The read-only tier.
    pub const NONE: Self = Self {
        edits: false,
        inserts: false,
        inserts_into_views: false,
        imports: false,
        assigns_default: false,
        copies_ddl: false,
        switches_database: false,
        switches_database_in_session: false,
        picks_schema: false,
        exact_row_estimates: false,
        schema_switch_waits_for_transaction: false,
        chunk_refusal: None,
    };
}

/// The answers no strategy expresses, as code.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// A literal for a value bound for a column of `data_type`, or `None` to
    /// fall back to the plain literal.
    pub quote_value: Option<fn(value: &str, data_type: &str) -> Option<String>>,
    /// The whole predicate term naming a row's value in `column` (`None` for a
    /// NULL), for an engine whose rows are named by every value. `None` from the
    /// hook means the value cannot be matched, so the row is not written.
    pub match_value: Option<MatchValue>,
}

/// A hook spelling one column's term in a row's predicate: the column, its value
/// (`None` for NULL) and its type.
pub type MatchValue = fn(column: &str, value: Option<&str>, data_type: &str) -> Option<String>;

impl Hooks {
    pub const NONE: Self = Self {
        quote_value: None,
        match_value: None,
    };
}

pub(crate) const POSTGRES: EngineSpec = EngineSpec {
    id: "postgres",
    label: "Postgres",
    schemes: &["postgres", "postgresql"],
    file_extensions: &[],
    logo: "icons/engine-postgres.svg",
    brand: 0x4169E1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(postgres::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    // `postgres` is an ordinary database people work in.
    system_databases: &[],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        database: "postgres",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        params: Params::Dollar,
        // One simple-query submission is already one implicit transaction.
        implicit_transaction: true,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        generated_defaults: &[DefaultMarker::Prefix("GENERATED ALWAYS AS ")],
        explain_plan: Some(ExplainPrefix::Explain),
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::Postgres,
        parser_dialect: ParserDialect::PostgreSql,
        regex: RegexMatch::Tilde,
        read_only_session: ReadOnlySession::DefaultTransaction,
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        picks_schema: true,
        schema_switch_waits_for_transaction: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const MYSQL: EngineSpec = EngineSpec {
    id: "mysql",
    label: "MySQL",
    schemes: &["mysql"],
    file_extensions: &[],
    logo: "icons/engine-mysql.svg",
    brand: 0x4479A1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mysql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        // Unless `NO_BACKSLASH_ESCAPES` is set, which is not dbdelve's to know
        // or to set.
        backslash_escapes: true,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
        // Since 8.0.18. Server versions are not detected: an older server
        // refuses the statement and says so, which is the error the user needs
        // and is the server's to word rather than a version check's.
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::MySql,
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpLike,
        read_only_session: ReadOnlySession::SessionTransaction,
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        // No `picks_schema`: the database is the schema.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const MARIADB: EngineSpec = EngineSpec {
    id: "mariadb",
    label: "MariaDB",
    schemes: &["mariadb"],
    file_extensions: &[],
    logo: "icons/engine-mariadb.svg",
    // The sea lion's colour rather than the wordmark's near-black teal.
    brand: 0xC0765A,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mysql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        backslash_escapes: true,
        // Refused even into a `timestamp`, which MariaDB reads in the
        // session's zone.
        literal_offsets: false,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
        // MariaDB has no `EXPLAIN ANALYZE` and spells the same thing `ANALYZE`
        // with no `EXPLAIN`.
        explain_analyze: Some(ExplainPrefix::Analyze),
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpInstr,
        read_only_session: ReadOnlySession::SessionTransaction,
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SQLITE: EngineSpec = EngineSpec {
    id: "sqlite",
    label: "SQLite",
    schemes: &["sqlite", "sqlite3", "file"],
    file_extensions: &["db", "sqlite", "sqlite3", "db3"],
    logo: "icons/engine-sqlite.svg",
    brand: 0x003B57,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        // Its bare `EXPLAIN` lists bytecode rather than a plan, and no form
        // reports what a run actually cost, so there is no `explain_analyze`:
        // its absence keeps the menu from offering a mode that would only
        // produce an error.
        explain_plan: Some(ExplainPrefix::ExplainQueryPlan),
        typed_explain: TypedExplain::Sqlite,
        parser_dialect: ParserDialect::Sqlite,
        substring: SubstringMatch::Instr,
        // No `generated_defaults`: `table_info` leaves generated columns out
        // altogether.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        // No `assigns_default`: `UPDATE` takes an expression on the right and
        // `DEFAULT` is not one there.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const TURSO: EngineSpec = EngineSpec {
    id: "turso",
    label: "Turso",
    schemes: &["turso"],
    file_extensions: &[],
    logo: "icons/engine-turso.svg",
    brand: 0x4FF8D2,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        parser_dialect: ParserDialect::Sqlite,
        substring: SubstringMatch::Instr,
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const DUCKDB: EngineSpec = EngineSpec {
    id: "duckdb",
    label: "DuckDB",
    schemes: &["duckdb"],
    file_extensions: &["duckdb", "ddb"],
    logo: "icons/engine-duckdb.svg",
    brand: 0xFFF000,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        path: "Blank for a database in memory",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: true,
        },
        parser_dialect: ParserDialect::DuckDb,
        regex: RegexMatch::RegexpMatches,
        substring: SubstringMatch::InstrCast,
        // No Explain: a plan comes as box-drawn text in one column, a shape
        // `explain.rs` does not read yet. No `read_only_session`: an open
        // database has no session switch; read-only is a way to open the
        // file, which a profile does not ask for yet.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        // The databases attached to the instance the profile opened.
        switches_database: true,
        // Reopening a file detaches everything attached to it, and an
        // in-memory database opened again is a new, empty one.
        switches_database_in_session: true,
        picks_schema: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SNOWFLAKE: EngineSpec = EngineSpec {
    id: "snowflake",
    label: "Snowflake",
    schemes: &["snowflake"],
    file_extensions: &[],
    logo: "icons/engine-snowflake.svg",
    brand: 0x29B5E8,
    syntax: Syntax::Sql,
    layout: Fields::Account,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    // The account usage share every account has. `SNOWFLAKE_SAMPLE_DATA` is a
    // share too, but one people query.
    system_databases: &["SNOWFLAKE"],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        host: "Implied by the account",
        database: "Required",
        ..Hints::DEFAULT
    },
    // The standard quote: Snowflake folds an unquoted name to upper case and
    // reads a quoted one exactly, and the catalog reports names as stored --
    // so quoting what the catalog said is always the name it meant.
    sql: SqlSpec {
        // Unconditionally.
        backslash_escapes: true,
        foreign_table: ForeignTable::External,
        parser_dialect: ParserDialect::Snowflake,
        regex: RegexMatch::RegexpCount,
        substring: SubstringMatch::Contains,
        stateless_requests: true,
        // Every statement autocommits unless the submission brackets it, so
        // there is no `implicit_transaction`. No Explain: its plan is a fourth
        // shape `explain.rs` does not read yet. No `read_only_session`: there
        // is no session to set anything on. No `generated_defaults`: an
        // identity takes a supplied value.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        picks_schema: true,
        // An exact `ROW_COUNT` per table, where the others' numbers are as of
        // the last time statistics were gathered, and SQLite keeps none.
        exact_row_estimates: true,
        chunk_refusal: Some(
            "Snowflake runs each request on its own, so no transaction can span the chunks.",
        ),
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SQL_SERVER: EngineSpec = EngineSpec {
    id: "mssql",
    label: "SQL Server",
    schemes: &["mssql", "sqlserver"],
    file_extensions: &[],
    // A generic server glyph, because Simple Icons carries no Microsoft logos.
    logo: "icons/engine-sqlserver.svg",
    brand: 0xCC2927,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mssql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["master", "tempdb", "model", "msdb"],
    // `image` is the legacy binary type here; elsewhere it is a name anyone
    // may give a column or a domain, and a SQLite `image` column holds text.
    binary_types: &["image", "rowversion", "timestamp"],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        national_strings: true,
        boolean_literal: BooleanLiteral::OneZero,
        params: Params::AtP,
        paging: Paging::OffsetFetch,
        select_top: SelectTop::Top,
        count_all: CountAll::CountBig,
        // A bare `BEGIN` opens a statement block in T-SQL, not a transaction.
        // The gate's grammar reads this spelling too.
        transaction: Transaction::BeginTransaction,
        // A default constraint's definition always comes wrapped in
        // parentheses, so neither prefix can be one.
        generated_defaults: &[
            DefaultMarker::Prefix("IDENTITY("),
            DefaultMarker::Prefix("AS "),
        ],
        statement_split: StatementSplit::Batches,
        parser_dialect: ParserDialect::MsSql,
        format_dialect: FormatDialect::SqlServer,
        substring: SubstringMatch::LikeBracketEscape,
        // No Explain: there is no prefix form. A plan comes from `SET
        // SHOWPLAN_XML ON`, a session switch that has to be a batch of its
        // own, so Explain would mean changing the session around the user's
        // statement rather than putting a word on a copy of it. No
        // `read_only_session`: no session-level switch exists.
        // `ApplicationIntent=ReadOnly` routes a login to a readable replica
        // and is ignored by a primary.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        // No `picks_schema`: the default schema is the login's.
        ..Abilities::NONE
    },
    hooks: Hooks {
        quote_value: Some(mssql::typed_literal),
        match_value: None,
    },
};

pub(crate) const MONGODB: EngineSpec = EngineSpec {
    id: "mongodb",
    label: "MongoDB",
    schemes: &["mongodb", "mongodb+srv"],
    file_extensions: &[],
    logo: "icons/engine-mongodb.svg",
    brand: 0x47A248,
    syntax: Syntax::Mongo,
    layout: Fields::Server,
    default_port: Some(mongo::DEFAULT_PORT),
    takes_options: true,
    resolves_srv: true,
    system_databases: &["admin", "local", "config"],
    binary_types: &["bindata"],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        user: "Optional",
        ..Hints::DEFAULT
    },
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        imports: true,
        copies_ddl: true,
        switches_database: true,
        chunk_refusal: Some(
            "MongoDB has no transaction on a standalone server, so each chunk is kept as it goes.",
        ),
        // No `assigns_default`: a document has no column defaults to fall
        // back to.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const REDIS: EngineSpec = EngineSpec {
    id: "redis",
    label: "Redis",
    schemes: &["redis", "rediss"],
    file_extensions: &[],
    logo: "icons/engine-redis.svg",
    brand: 0xFF4438,
    syntax: Syntax::Redis,
    layout: Fields::Server,
    default_port: Some(redis::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        name: "Named after the host",
        database: "0",
        user: "Optional",
        ..Hints::DEFAULT
    },
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        switches_database: true,
        chunk_refusal: Some(
            "Redis has no transaction that rolls back, so each chunk is kept as it goes.",
        ),
        // No `imports`: its new rows are keys that each name their own value,
        // not rows under a file's columns. No `copies_ddl`: a keyspace is
        // declared nowhere.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const CLICKHOUSE: EngineSpec = EngineSpec {
    id: "clickhouse",
    label: "ClickHouse",
    schemes: &["clickhouse"],
    file_extensions: &[],
    logo: "icons/engine-clickhouse.svg",
    brand: 0xFFCC01,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(clickhouse::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["system", "INFORMATION_SCHEMA", "information_schema"],
    binary_types: &[],
    numeric_types: &[
        "int8",
        "int16",
        "int32",
        "int64",
        "int128",
        "int256",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uint128",
        "uint256",
        "float32",
        "float64",
        "bfloat16",
        "decimal32",
        "decimal64",
        "decimal128",
        "decimal256",
    ],
    type_wrappers: &["nullable", "lowcardinality"],
    hints: Hints {
        database: "default",
        user: "default",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        backslash_escapes: true,
        // Quoted identifiers read backslash escapes as strings do.
        identifier_backslash_escapes: true,
        literal_offsets: false,
        paging: Paging::LimitOffsetByKey,
        generated_defaults: &[
            DefaultMarker::Prefix("MATERIALIZED "),
            DefaultMarker::Prefix("ALIAS "),
        ],
        foreign_table: ForeignTable::Table,
        materialized_view: MaterializedViewDrop::View,
        parser_dialect: ParserDialect::ClickHouse,
        read_only_session: ReadOnlySession::PerRequest,
        unreadable_floor: UnreadableFloor::FirstKeyword,
        local_table_engines: Some(&[
            "MergeTree",
            "ReplacingMergeTree",
            "SummingMergeTree",
            "AggregatingMergeTree",
            "CollapsingMergeTree",
            "VersionedCollapsingMergeTree",
            "GraphiteMergeTree",
            "CoalescingMergeTree",
            "ReplicatedMergeTree",
            "ReplicatedReplacingMergeTree",
            "ReplicatedSummingMergeTree",
            "ReplicatedAggregatingMergeTree",
            "ReplicatedCollapsingMergeTree",
            "ReplicatedVersionedCollapsingMergeTree",
            "ReplicatedGraphiteMergeTree",
            "ReplicatedCoalescingMergeTree",
            "SharedMergeTree",
            "SharedReplacingMergeTree",
            "SharedSummingMergeTree",
            "SharedAggregatingMergeTree",
            "SharedCollapsingMergeTree",
            "SharedVersionedCollapsingMergeTree",
            "SharedGraphiteMergeTree",
            "SharedCoalescingMergeTree",
            "Log",
            "TinyLog",
            "StripeLog",
            "Memory",
            "Null",
            "Set",
            "Join",
            "Buffer",
            "Merge",
            "Dictionary",
            "EmbeddedRocksDB",
            "GenerateRandom",
        ]),
        regex: RegexMatch::Match,
        substring: SubstringMatch::LikeToString,
        row_terms: true,
        transactions: false,
        // `EXPLAIN` alone is `EXPLAIN PLAN`, which never executes its statement;
        // there is no analyzing form, and no `skip_duplicates`: there is no
        // clause that skips a row on a key, because there is no unique key.
        explain_plan: Some(ExplainPrefix::Explain),
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        switches_database: true,
        chunk_refusal: Some("ClickHouse has no transaction, so each chunk is kept as it lands."),
        ..Abilities::NONE
    },
    hooks: Hooks {
        match_value: Some(clickhouse::match_value),
        ..Hooks::NONE
    },
};

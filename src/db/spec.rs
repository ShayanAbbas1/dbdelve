//! What sets one engine apart from another, as data: one [`EngineSpec`] per
//! [`Engine`](super::Engine), read through [`Engine::spec`](super::Engine::spec).
//!
//! A field is a fact or a choice among strategies the code already has, never
//! SQL text: the statements themselves are written where they are used, and a
//! spec only says which spelling that code picks. A new engine that fits the
//! existing strategies is one more value here and its driver.

use super::{Fields, Syntax, mongo, mssql, mysql, postgres, redis};

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
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub system_databases: &'static [&'static str],
    /// Exact type names read as bytes beyond the families every engine shares.
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub binary_types: &'static [&'static str],
    pub hints: Hints,
    /// Meaningful only where `syntax` is [`Syntax::Sql`].
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub sql: SqlSpec,
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub abilities: Abilities,
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub hooks: Hooks,
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
#[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
pub struct SqlSpec {
    pub identifier_quote: IdentifierQuote,
    pub backslash_escapes: bool,
    /// String literals are written `N'…'`.
    pub national_strings: bool,
    /// Whether a date literal may carry a UTC offset.
    pub literal_offsets: bool,
    pub boolean_literal: BooleanLiteral,
    pub params: Params,
    pub paging: Paging,
    pub select_top: SelectTop,
    pub truncate: Truncate,
    pub foreign_table: ForeignTable,
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
    /// Nothing outlives one request, so no statement leaves state behind.
    pub stateless_requests: bool,
}

impl SqlSpec {
    pub const ANSI: Self = Self {
        identifier_quote: IdentifierQuote::Double,
        backslash_escapes: false,
        national_strings: false,
        literal_offsets: true,
        boolean_literal: BooleanLiteral::TrueFalse,
        params: Params::Question,
        paging: Paging::LimitOffset,
        select_top: SelectTop::Limit,
        truncate: Truncate::Truncate,
        foreign_table: ForeignTable::Foreign,
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
        stateless_requests: false,
    };

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` where one submission is already atomic.
    #[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
    pub fn transaction_opening(&self) -> Option<&'static str> {
        match (self.implicit_transaction, self.transaction) {
            (true, _) => None,
            (false, Transaction::Begin) => Some("BEGIN"),
            (false, Transaction::BeginTransaction) => Some("BEGIN TRANSACTION"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentifierQuote {
    Double,
    Backtick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BooleanLiteral {
    TrueFalse,
    OneZero,
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
    OffsetFetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectTop {
    Limit,
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncate {
    Truncate,
    DeleteFrom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignTable {
    Foreign,
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountAll {
    Count,
    CountBig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transaction {
    Begin,
    BeginTransaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipDuplicates {
    None,
    OnConflictDoNothing { needs_unique_key: bool },
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
    Analyze,
    ExplainQueryPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedExplain {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatDialect {
    Generic,
    SqlServer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexMatch {
    None,
    Tilde,
    RegexpLike,
    RegexpInstr,
    RegexpMatches,
    RegexpCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstringMatch {
    Like,
    LikeBracketEscape,
    Instr,
    InstrCast,
    Contains,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnlySession {
    None,
    DefaultTransaction,
    SessionTransaction,
}

#[derive(Clone, Copy, Debug)]
#[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
pub struct Abilities {
    pub edits: bool,
    pub inserts: bool,
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
#[expect(dead_code, reason = "phase 3: read as each area moves onto the spec")]
pub struct Hooks {
    /// A literal for a value bound for a column of `data_type`, or `None` to
    /// fall back to the plain literal.
    pub quote_value: Option<fn(value: &str, data_type: &str) -> Option<String>>,
}

impl Hooks {
    pub const NONE: Self = Self { quote_value: None };
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
    system_databases: &[],
    binary_types: &[],
    hints: Hints {
        database: "postgres",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        params: Params::Dollar,
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
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        backslash_escapes: true,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
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
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        backslash_escapes: true,
        literal_offsets: false,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
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
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        explain_plan: Some(ExplainPrefix::ExplainQueryPlan),
        typed_explain: TypedExplain::Sqlite,
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
    system_databases: &["SNOWFLAKE"],
    binary_types: &[],
    hints: Hints {
        host: "Implied by the account",
        database: "Required",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        backslash_escapes: true,
        foreign_table: ForeignTable::External,
        parser_dialect: ParserDialect::Snowflake,
        regex: RegexMatch::RegexpCount,
        substring: SubstringMatch::Contains,
        stateless_requests: true,
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
    binary_types: &["image", "rowversion", "timestamp"],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        national_strings: true,
        boolean_literal: BooleanLiteral::OneZero,
        params: Params::AtP,
        paging: Paging::OffsetFetch,
        select_top: SelectTop::Top,
        count_all: CountAll::CountBig,
        transaction: Transaction::BeginTransaction,
        generated_defaults: &[
            DefaultMarker::Prefix("IDENTITY("),
            DefaultMarker::Prefix("AS "),
        ],
        statement_split: StatementSplit::Batches,
        parser_dialect: ParserDialect::MsSql,
        format_dialect: FormatDialect::SqlServer,
        substring: SubstringMatch::LikeBracketEscape,
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
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

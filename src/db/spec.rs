//! What sets one engine apart from another, as data: one [`EngineSpec`] per
//! [`Engine`](super::Engine), read through [`Engine::spec`](super::Engine::spec).
//!
//! A field is a fact or a choice among strategies the code already has, never
//! SQL text: the statements themselves are written where they are used, and a
//! spec only says which spelling that code picks. A new engine that fits the
//! existing strategies is one more value here and its driver.

use super::{
    Builtin, ColumnDefinition, Fields, RelationKind, Syntax, builtins, clickhouse, duckdb, mongo,
    mssql, mysql, postgres, redis,
};

#[derive(Clone, Copy, Debug)]
pub struct EngineSpec {
    /// The spelling stored in `profiles.toml`. Changing one orphans every
    /// profile already written with it.
    pub id: &'static str,
    pub label: &'static str,
    /// Whether DBDelve connects to this engine itself. Without one, a profile
    /// reaches it through a plugin and nothing here can open a connection.
    pub built_in_driver: bool,
    /// What a driver reports as the DBMS name for this engine, compared
    /// case-insensitively as a prefix.
    pub dbms_names: &'static [&'static str],
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
    pub takes_service: bool,
    pub resolves_srv: bool,
    pub system_databases: &'static [&'static str],
    /// Exact type names read as bytes beyond the families every engine shares.
    pub binary_types: &'static [&'static str],
    /// How the driver writes a value of one of those types as text, `None`
    /// where no column is ever typed as one.
    pub binary_text: Option<BinaryText>,
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

    /// `value` pieced together around its NULs, or `None` with none to spell.
    pub fn text_around_nul(&self, value: &str, data_type: Option<&str>) -> Option<String> {
        let joiner = match self.sql.nul_text {
            NulText::Unspelled => return None,
            NulText::PipedChar => " || char(0) || ",
            NulText::PipedChr => " || chr(0) || ",
            NulText::AddedNchar => " + NCHAR(0) + ",
        };
        value.contains('\0').then(|| {
            let text = value
                .split('\0')
                .map(|part| self.quote_value(part, data_type))
                .collect::<Vec<_>>();
            match self.sql.nul_text {
                // Without a `max` operand, `+` stops at 4,000 characters.
                NulText::AddedNchar => {
                    format!(
                        "CAST({} AS nvarchar(max)){joiner}{}",
                        text[0],
                        text[1..].join(joiner)
                    )
                }
                _ => text.join(joiner),
            }
        })
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

    /// [`Engine::bytes_of`](super::Engine::bytes_of).
    pub fn bytes_of(&self, value: &str, most: usize) -> Option<Vec<u8>> {
        // A plugin writes bytes this way whatever dialect it speaks
        // (`external::rows`), and what asks knows only the dialect. Tried
        // first, since bare hex takes anything; no engine's own spelling
        // starts with `0x`, `x` being no hex digit.
        BinaryText::ZeroX
            .payload(value)
            .or_else(|| self.binary_text?.payload(value))?
            .decode(most)
    }

    /// [`Engine::bytes_text`](super::Engine::bytes_text).
    pub fn bytes_text(&self, bytes: &[u8]) -> Option<String> {
        Some(self.binary_text?.spell(bytes))
    }

    /// [`Engine::binary_literal`](super::Engine::binary_literal).
    pub fn binary_literal(&self, bytes: &[u8]) -> Option<String> {
        self.sql.binary_literal?.spell(bytes)
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

    /// [`Engine::writes_back`](super::Engine::writes_back).
    pub fn writes_back<'a>(&self, types: impl IntoIterator<Item = &'a str>) -> bool {
        types.into_iter().all(|data_type| match self.syntax {
            Syntax::Redis => crate::redis_cmd::edit::writes_back(data_type),
            Syntax::Sql | Syntax::Mongo => !self.is_binary_type(data_type),
        })
    }

    /// [`Engine::generates`](super::Engine::generates).
    pub fn generates(&self, column: &ColumnDefinition) -> bool {
        marked(column, self.sql.generated_defaults)
    }

    /// Whether the server numbers `column` itself, where a row written back
    /// keeps the number it had.
    pub fn numbers(&self, column: &ColumnDefinition) -> bool {
        marked(column, self.sql.identity_defaults)
    }

    /// Whether the server works `column` out from the rest of its row, so
    /// that a value for it has nowhere to go.
    pub fn computes(&self, column: &ColumnDefinition) -> bool {
        let data_type = column.data_type.to_ascii_lowercase();
        self.sql.computed_types.contains(&data_type.as_str())
            || (self.generates(column) && !self.numbers(column))
    }
}

fn marked(column: &ColumnDefinition, markers: &[DefaultMarker]) -> bool {
    column.default.as_deref().is_some_and(|default| {
        markers.iter().any(|marker| match *marker {
            DefaultMarker::Exact(exact) => default == exact,
            DefaultMarker::Prefix(prefix) => default.starts_with(prefix),
        })
    })
}

fn unwrap_type_wrapper<'a>(name: &'a str, wrapper: &str) -> Option<&'a str> {
    name.strip_prefix(wrapper)?
        .strip_prefix('(')?
        .strip_suffix(')')
        .map(str::trim)
}

/// How a driver writes bytes as text, which is all the grid holds of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryText {
    /// `0xDEADBEEF`, the T-SQL and MySQL literal.
    ZeroX,
    /// `\xdeadbeef`, Postgres' `bytea_output = hex`, the server's default.
    BackslashX,
    /// `x'DEADBEEF'`, SQLite's literal.
    QuotedX,
    /// `deadbeef`, as Arrow prints a blob and Snowflake sends one.
    Bare,
    /// `BinData(0, '3q2+7w==')`, the shell's spelling.
    BinData,
}

/// The digits of bytes written as text, before they are decoded.
enum Payload<'a> {
    Hex(&'a str),
    Base64(&'a str),
}

impl BinaryText {
    fn payload(self, value: &str) -> Option<Payload<'_>> {
        match self {
            Self::ZeroX => value.strip_prefix("0x").map(Payload::Hex),
            Self::BackslashX => value.strip_prefix("\\x").map(Payload::Hex),
            Self::QuotedX => value
                .strip_prefix("x'")?
                .strip_suffix('\'')
                .map(Payload::Hex),
            Self::Bare => Some(Payload::Hex(value)),
            Self::BinData => {
                let (subtype, rest) = value.strip_prefix("BinData(")?.split_once(", '")?;
                subtype.parse::<u8>().ok()?;
                rest.strip_suffix("')").map(Payload::Base64)
            }
        }
    }

    /// `bytes` as the driver writes them, which `payload` reads back.
    fn spell(self, bytes: &[u8]) -> String {
        match self {
            Self::ZeroX => format!("0x{}", hex::encode_upper(bytes)),
            Self::BackslashX => format!("\\x{}", hex::encode(bytes)),
            Self::QuotedX => format!("x'{}'", hex::encode_upper(bytes)),
            Self::Bare => hex::encode_upper(bytes),
            Self::BinData => {
                use base64::Engine as _;
                let digits = base64::engine::general_purpose::STANDARD.encode(bytes);
                format!("BinData(0, '{digits}')")
            }
        }
    }
}

impl BinaryLiteral {
    /// `None` for what has no spelling the write gate reads.
    fn spell(self, bytes: &[u8]) -> Option<String> {
        let digits = hex::encode_upper(bytes);
        match self {
            Self::EscapeString => Some(format!("E'\\\\x{digits}'")),
            Self::XQuoted => Some(format!("X'{digits}'")),
            Self::ZeroX => (!bytes.is_empty()).then(|| format!("0x{digits}")),
        }
    }
}

impl Payload<'_> {
    /// Only the front of the digits is read: a blob can run to megabytes,
    /// and its cell is drawn every frame. Whatever lies past `most` bytes is
    /// checked for its length alone, as `payload` checks its closing quote.
    ///
    /// ponytail: a character that is no digit past the part read goes
    /// unseen, so a cell can show bytes that the row panel, reading further,
    /// refuses. Only text imitating the engine's own spelling of bytes (a
    /// SQLite BLOB-affinity column holding text) can do that: a driver
    /// renders real bytes whole. If it matters, validate each value once per
    /// result, off the draw path, and keep the answer beside the rows.
    fn decode(self, most: usize) -> Option<Vec<u8>> {
        match self {
            Self::Hex(digits) => {
                let digits = digits.as_bytes();
                if digits.len() % 2 != 0 {
                    return None;
                }
                let read = digits.len().min(most.saturating_mul(2));
                hex::decode(&digits[..read]).ok()
            }
            Self::Base64(digits) => {
                use base64::Engine as _;
                let digits = digits.as_bytes();
                if digits.len() % 4 != 0 {
                    return None;
                }
                let read = digits.len().min(most.div_ceil(3).saturating_mul(4));
                let mut bytes = base64::engine::general_purpose::STANDARD
                    .decode(&digits[..read])
                    .ok()?;
                bytes.truncate(most);
                Some(bytes)
            }
        }
    }
}

/// What the connection form says a blank field means.
#[derive(Clone, Copy, Debug)]
pub struct Hints {
    pub name: &'static str,
    pub host: &'static str,
    /// What a blank database connects to (`ConnectionConfig::database_name`)
    /// where the engine has one; elsewhere it leaves the server to pick. The
    /// form says the service answers instead where one is typed.
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

/// How a text is pieced together around a NUL, on an engine whose text can
/// hold one and whose literal cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NulText {
    /// Nothing to spell: the text cannot hold one, or a literal already can.
    Unspelled,
    /// `'a' || char(0) || 'b'`
    PipedChar,
    /// `'a' || chr(0) || 'b'`
    PipedChr,
    /// `CAST(N'a' AS nvarchar(max)) + NCHAR(0) + N'b'`
    AddedNchar,
}

/// How a value of bytes is written into a statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryLiteral {
    /// `E'\\x6869'`. An escape string reads the same whatever
    /// `standard_conforming_strings` says, where a plain `'\x6869'` with it
    /// off reads `\x68` as `h` and stores the bytes of `h69`.
    EscapeString,
    /// `X'6869'`, empty as `X''`.
    XQuoted,
    /// `0x6869`. Empty, it is `0x`, which the gate's grammar cannot read.
    ZeroX,
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
    /// Whether a bare name is read as upper case, as the standard has it,
    /// rather than lower. A server that folds neither way is treated as
    /// folding down: quoting a name it would have matched bare costs nothing.
    pub identifier_folds_upper: bool,
    /// How a NUL in text is written, where a literal has no escape for it.
    pub nul_text: NulText,
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
    /// Which of `generated_defaults` are an identity's rather than a computed
    /// column's.
    pub identity_defaults: &'static [DefaultMarker],
    /// What an `INSERT` has to say for a value to reach an identity column.
    pub identity_insert: IdentityInsert,
    /// Types the server writes itself, with no default to say so.
    pub computed_types: &'static [&'static str],
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
    /// Functions whose effect the server-side Read-only hold does not stop, so
    /// the verdict has to: a call to one needs more than Read-only wherever it
    /// sits in the statement. A deny-list of named functions, not an allow-list
    /// of safe ones -- a function that is not here, a user-defined one included,
    /// stays a read on the human path. The agent path asks for a call
    /// [`SqlSpec::builtin_functions`] does not name at Read-write and Full.
    /// Lower-case, matched against the name as written.
    pub denied_functions: &'static [&'static str],
    /// Functions that read a file, a URL, a program or another server, or run
    /// text that may, so an agent's read of one asks the human first: the rows
    /// leave the machine, and the agent chose where from. Lower-case, matched
    /// against the last word before the parenthesis, as `denied_functions` is.
    /// Read only by the agent path (`sql::agent_gate`). It is not on
    /// `denied_functions` because that list raises the human path's verdict
    /// (Phase 1 D-05), and DBDelve's own CSV, Excel, Parquet and JSON opens
    /// and a user's own read of a file are reads and stay so.
    pub egress_functions: &'static [&'static str],
    /// The engine's built-in functions that change nothing: one name per line
    /// under `#` comment lines, from a file in `src/db/functions/`, matched
    /// against a bare, unquoted call name. On the agent path a call to a name
    /// not here asks at Read-write and Full. On Generic it is also the human
    /// path's list of reads. A missing name costs one approval; a wrongly
    /// listed one is a breach, so each list is the engine's own catalog
    /// filtered by its purity data where one exists.
    pub builtin_functions: &'static str,
    /// Whether the server tells function names apart by case, so
    /// [`SqlSpec::builtin_functions`] is matched as spelled. ClickHouse is the
    /// example: a user `todate` can sit beside the built-in `toDate`.
    pub case_sensitive_functions: bool,
    /// Whether a quoted string in a table position is read as a file or a URL
    /// (DuckDB's replacement scan: `FROM 'data.csv'`). The agent path asks for
    /// approval on any statement of such an engine that holds a path-shaped
    /// string, since no function name marks the read. Read only by the agent
    /// path (`sql::agent_gate`).
    pub bare_path_reads: bool,
    /// Session settings that change nothing a Read-only profile protects, so
    /// setting one stays a read: navigation and comfort. Every other setting a
    /// user's own `SET` names is stopped, because the hold's whole value is
    /// being hard to remove and each dialect has more settings than anyone has
    /// listed. Lower-case, matched case-insensitively against the name once its
    /// `@@` prefix and scope part are gone.
    pub plain_settings: &'static [&'static str],
    /// Whether a transaction can be opened at all. Without one a generated
    /// multi-row batch is not bracketed, whatever `transaction` says.
    pub transactions: bool,
    /// Whether the write gate reads the terms a row named by every value
    /// carries: `toString(c) = '…'` and `c IS NULL`.
    pub row_terms: bool,
    /// How a grid edit writes bytes, `None` where it writes none.
    pub binary_literal: Option<BinaryLiteral>,
    /// Whether the write gate reads `c IS NULL` beside equalities in a one-row
    /// predicate, for a row named by every value on an engine whose drafts are
    /// verified: the apply re-reads the row and counts what the write changed,
    /// so a predicate matching two rows is caught inside the transaction.
    /// ClickHouse reads the same term under [`SqlSpec::row_terms`] and needs no
    /// second switch. A plugin that speaks one of these dialects reads it too.
    pub null_terms: bool,
    /// Whether the write gate admits a reviewed draft: several statements
    /// holding a one-row `INSERT` or `DELETE`. Only where the grid writes one,
    /// since admitting a shape nothing writes widens the gate for nobody.
    pub drafts: bool,
    /// Whether `"…"` is a string literal rather than a name, as under MySQL's
    /// and MariaDB's default `sql_mode`. Not read off `identifier_quote`:
    /// ClickHouse quotes with backticks and still reads `"x"` as a name.
    pub double_quoted_strings: bool,
    /// What completion offers beside the catalog: the functions and value
    /// expressions the dialect has built in.
    pub builtins: &'static [Builtin],
}

impl SqlSpec {
    pub const ANSI: Self = Self {
        identifier_quote: IdentifierQuote::Double,
        backslash_escapes: false,
        identifier_backslash_escapes: false,
        identifier_folds_upper: false,
        nul_text: NulText::Unspelled,
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
        identity_defaults: &[],
        identity_insert: IdentityInsert::Plain,
        computed_types: &[],
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
        denied_functions: &[],
        egress_functions: &[],
        // Empty is the fail-safe default: an engine that sets nothing asks for
        // every call.
        builtin_functions: "",
        case_sensitive_functions: false,
        bare_path_reads: false,
        plain_settings: &[],
        transactions: true,
        row_terms: false,
        binary_literal: None,
        null_terms: false,
        drafts: false,
        double_quoted_strings: false,
        builtins: &[],
    };

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` where one submission is already atomic.
    pub fn transaction_opening(&self) -> Option<&'static str> {
        (self.transactions && !self.implicit_transaction).then(|| self.transaction.opening())
    }
}

/// Names sqlparser reads as a call that a catalog may not list: the standard's
/// grammar-defined functions and the standard window functions. The generated
/// built-in lists and the live pins add these to what the catalog answers.
#[cfg(test)]
pub(crate) const GRAMMAR_FUNCTIONS: &[&str] = &[
    "coalesce",
    "nullif",
    "greatest",
    "least",
    "cast",
    "try_cast",
    "extract",
    "position",
    "substring",
    "trim",
    "overlay",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "current_user",
    "session_user",
    "user",
    "current_catalog",
    "current_schema",
    "system_user",
    "grouping",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
];

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
pub enum IdentityInsert {
    Plain,
    /// `OVERRIDING SYSTEM VALUE`, in the statement.
    Overriding,
    /// `SET IDENTITY_INSERT`, on the session, around the statements.
    SessionSwitch,
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
    /// `LIKE` with no escaping, for an engine with no default escape character
    /// (Db2, the SQL standard, Oracle, SQL Server) where `Like`'s backslash
    /// would match a literal backslash. The `ESCAPE` clause is not an option:
    /// the grammar the filter gate parses with does not read it. A value
    /// holding a wildcard or a backslash is refused rather than matched wrongly.
    LikePlain,
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
    /// The catalog lists every database on the server, each as a schema,
    /// rather than the one the connection is on.
    pub catalog_lists_databases: bool,
    /// The catalog of one database is one schema named after it, so the tree
    /// shows no schema level of its own and draws that one row as the database.
    pub one_schema_per_database: bool,
    pub picks_schema: bool,
    /// The catalog holds sequences, listed as their own kind of object beside
    /// the routines, in the catalog's second half.
    pub lists_sequences: bool,
    pub exact_row_estimates: bool,
    pub schema_switch_waits_for_transaction: bool,
    /// The catalog declares foreign keys that
    /// [`Connection::schema_graph`](super::Connection::schema_graph) reads.
    pub draws_diagrams: bool,
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
        catalog_lists_databases: false,
        one_schema_per_database: false,
        picks_schema: false,
        lists_sequences: false,
        exact_row_estimates: false,
        schema_switch_waits_for_transaction: false,
        draws_diagrams: false,
        chunk_refusal: None,
    };

    /// [`Target::takes_inserts`](super::Target::takes_inserts).
    pub fn takes_inserts(&self, kind: RelationKind) -> bool {
        self.inserts && (kind != RelationKind::View || self.inserts_into_views)
    }
}

/// The answers no strategy expresses, as code.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// A literal for a value bound for a column of `data_type`, or `None` to
    /// fall back to the plain literal.
    pub quote_value: Option<fn(value: &str, data_type: &str) -> Option<String>>,
    /// A literal for a value the driver itself rendered, written back by an
    /// `INSERT`, or `None` for what [`Self::quote_value`] and the shared shapes
    /// make of it.
    pub fetched_literal: Option<fn(value: &str, data_type: &str) -> Option<String>>,
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
        fetched_literal: None,
        match_value: None,
    };
}

pub(crate) const POSTGRES: EngineSpec = EngineSpec {
    id: "postgres",
    built_in_driver: true,
    dbms_names: &["PostgreSQL"],
    label: "Postgres",
    schemes: &["postgres", "postgresql"],
    file_extensions: &[],
    logo: "icons/engine-postgres.svg",
    brand: 0x4169E1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(postgres::DEFAULT_PORT),
    takes_options: false,
    takes_service: true,
    resolves_srv: false,
    // `postgres` is an ordinary database people work in.
    system_databases: &[],
    binary_types: &[],
    binary_text: Some(BinaryText::BackslashX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        database: "postgres",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        params: Params::Dollar,
        // One simple-query submission is already one implicit transaction.
        implicit_transaction: true,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        generated_defaults: &[DefaultMarker::Prefix("GENERATED ALWAYS AS ")],
        identity_defaults: &[DefaultMarker::Exact("GENERATED ALWAYS AS IDENTITY")],
        identity_insert: IdentityInsert::Overriding,
        explain_plan: Some(ExplainPrefix::Explain),
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::Postgres,
        parser_dialect: ParserDialect::PostgreSql,
        regex: RegexMatch::Tilde,
        read_only_session: ReadOnlySession::DefaultTransaction,
        // The armed `default_transaction_read_only` stops none of these, and
        // `set_config` turns it off. Reading a file the role may read is not
        // here: that is a read, and how DBDelve opens its own files.
        //
        // The large-object writers persist and rewrite stored data under the
        // armed hold (reproduced live: `lo_from_bytea`, `lo_put` and
        // `lo_truncate` each took effect). `lo_get` and `loread` are absent
        // because reading an object is a read. The recovery and replication
        // controls (`pg_promote`, `pg_wal_replay_pause`, the replication slot
        // calls) are absent too: they need a superuser or a replication role,
        // none was reproduced against the dev server, and they control
        // recovery rather than stored data. The live differential asks the
        // server's catalog for the large-object family, so a writer a later
        // version adds fails there by name, provided the server it probes is
        // that later version: the family was checked against Postgres 15.13.
        //
        // A function that executes its text argument as a query reaches any
        // listed call through a name the token scan cannot read, because the
        // call sits inside a string: `query_to_xml('select set_config(...)')`
        // released the hold on the dev server, and `query_to_xml('select
        // lo_from_bytea(...)')` wrote a large object with it armed. SPI's
        // read-only mode refuses DML inside that text and refuses none of the
        // listed functions, which is why the carrier works where
        // `query_to_xml('delete ...')` does not. `query_to_xml`,
        // `query_to_xml_and_xmlschema`, `query_to_xmlschema` and `ts_stat`
        // are carriers found so far, because the server's catalog names their
        // text argument `query` or `sqlquery`; the carrier is listed, never
        // the text it carries. The live differential asks the catalog for
        // that shape, so a function a later version adds with such an
        // argument fails there by name.
        //
        // `ts_rewrite(tsquery, text)` is another, and the catalog does not
        // name its text argument: both overloads carry no argument names.
        // Its second argument is run as a query through SPI, so a listed call
        // carried there reaches the server the same way (reproduced live,
        // once releasing the hold and once creating a large object with it
        // armed). The shape probe cannot see it, so an explicit pin in the
        // differential holds it by name. The three-argument
        // `ts_rewrite(tsquery, tsquery, tsquery)` overload runs no text, and
        // the name stops it as well: an over-classification whose direction
        // is an upgrade prompt, one of the accepted false positives listed
        // in the advisory's `What users will see`.
        //
        // These are the carriers found so far, not a closed set, and the
        // probe sees only those whose text argument the catalog names. A
        // type-shape probe (every `pg_catalog` function taking a `text`
        // argument the catalog leaves unnamed) was measured against Postgres
        // 15.13 and returned 222 distinct names where this list then held 47;
        // dispositioning that many by hand would be a second deny-list with
        // no evidence behind its entries, so the probe keeps its
        // named-argument query.
        //
        // `tablefunc`'s `crosstab` runs its source query through SPI, so a
        // listed call carried there reaches the server (reproduced on the
        // compose Postgres 15.13 in a scratch database, once releasing the
        // hold and once writing a large object with it armed). It lives in an
        // extension outside `pg_catalog`, so neither catalog probe in
        // `conformance.rs` can see it; `live_extension_carriers_agree_with_the_server`
        // in `src/db/postgres.rs` pins it against the extension's own catalog
        // instead. Listing it means a `crosstab` pivot asks for Read-write on
        // a Read-only profile, an accepted cost recorded in the advisory.
        // `crosstab2` to `crosstab4` are `crosstab` with fixed result types and
        // the same implementation. `tablefunc`'s `connectby` splices its
        // relation and column arguments into the query it runs, so a
        // parenthesised `select` given as the relation runs. `xml2`'s
        // `xpath_table` splices its relation and criteria the same way
        // (reproduced writing a large object under the armed hold). The rest
        // of each extension's members are carved out by name in the live test
        // with what each does instead. The `dblink` names are pinned the same
        // way, and have no live case because what they run, they run in
        // another session the local hold never covered. `dblink_open` is
        // listed with them: it declares a cursor over the SQL text it is
        // handed.
        //
        // PostGIS is asked by shape, because it is the extension the dev
        // server has installed: its SQL and PL/pgSQL members whose source runs
        // `EXECUTE`. `st_findextent` splices its column argument inside double
        // quotes unescaped, so a quote closes the identifier and what follows
        // runs, and it wrote a large object with the hold armed (reproduced
        // live). The other six members that match escape every argument. The
        // survey stops there: PostGIS's C members cannot be seen by a catalog
        // shape, and `postgis_raster`, `postgis_topology` and
        // `postgis_tiger_geocoder` were not surveyed. The catalog lists were
        // checked against Postgres 15.13 (Debian 15.13-1.pgdg110+1).
        //
        // `adminpack` is contrib, shipped through Postgres 16 and removed in
        // 17. It installs into `pg_catalog` four functions that write,
        // rename, flush and delete files on the server's file system. They
        // are listed so that a Read-only profile asks first.
        // `pg_file_write`, `pg_file_rename` and `pg_file_unlink` also need
        // Full, through `HOLD_IGNORING_CALLS` in src/sql.rs, because a file
        // written outside the database is not rolled back; that leaves
        // `pg_file_sync` at Read-write. Its `pg_logdir_ls` lists the log
        // directory and is on `egress_functions` below. The extension's
        // members are pinned against its own catalog by
        // `live_every_function_adminpack_owns_is_on_the_deny_or_egress_list`
        // in src/db/postgres.rs. The names come from the PostgreSQL 16
        // documentation, not from a run against a server.
        //
        // The other contrib extensions with side effects were not surveyed
        // and stay reads until they are listed: `pg_surgery`,
        // `pg_stat_statements_reset`, `pg_visibility` and `pg_prewarm`.
        denied_functions: &[
            "set_config",
            "nextval",
            "setval",
            "pg_advisory_lock",
            "pg_advisory_lock_shared",
            "pg_advisory_xact_lock",
            "pg_advisory_xact_lock_shared",
            "pg_try_advisory_lock",
            "pg_try_advisory_lock_shared",
            "pg_try_advisory_xact_lock",
            "pg_try_advisory_xact_lock_shared",
            "pg_advisory_unlock",
            "pg_advisory_unlock_shared",
            "pg_advisory_unlock_all",
            "pg_notify",
            "pg_terminate_backend",
            "pg_cancel_backend",
            "pg_reload_conf",
            "pg_rotate_logfile",
            "pg_switch_wal",
            "pg_create_restore_point",
            "pg_stat_reset",
            "pg_stat_reset_shared",
            "pg_stat_reset_single_table_counters",
            "pg_stat_reset_single_function_counters",
            "pg_stat_reset_slru",
            "pg_stat_reset_replication_slot",
            "lo_create",
            "lo_import",
            "lo_export",
            "lo_unlink",
            "lo_from_bytea",
            "lo_put",
            "lo_creat",
            "lowrite",
            "lo_truncate",
            "lo_truncate64",
            "query_to_xml",
            "query_to_xml_and_xmlschema",
            "query_to_xmlschema",
            "ts_stat",
            "ts_rewrite",
            "crosstab",
            "crosstab2",
            "crosstab3",
            "crosstab4",
            "connectby",
            "xpath_table",
            "dblink",
            "dblink_exec",
            "dblink_connect",
            "dblink_connect_u",
            "dblink_send_query",
            "dblink_open",
            "st_findextent",
            "pg_file_write",
            "pg_file_rename",
            "pg_file_unlink",
            "pg_file_sync",
        ],
        // The documentation's Generic File Access Functions, plus
        // `pg_read_file_old`, which the server carries. Pinned against
        // `pg_catalog` by `live_every_file_access_function_is_on_the_egress_list`.
        // `adminpack`'s `pg_logdir_ls` lists the log directory as
        // `pg_ls_logdir` does; the adminpack live test pins it rather than
        // that regex test, because an extension is installed per database and
        // the shared dev database does not install it.
        egress_functions: &[
            "pg_read_file",
            "pg_read_binary_file",
            "pg_read_file_old",
            "pg_stat_file",
            "pg_ls_dir",
            "pg_ls_logdir",
            "pg_ls_waldir",
            "pg_ls_logicalmapdir",
            "pg_ls_logicalsnapdir",
            "pg_ls_replslotdir",
            "pg_ls_archive_statusdir",
            "pg_ls_tmpdir",
            "pg_ls_summariesdir",
            "pg_logdir_ls",
        ],
        // What a user's own `SET` may change on a Read-only profile without
        // asking. Left off on purpose: the hold's own two names, `role` and
        // `session_authorization`, the switches that turn the server's checks
        // off, and everything that changes how a quote or a backslash is read.
        plain_settings: &[
            "search_path",
            "timezone",
            "statement_timeout",
            "lock_timeout",
            "idle_in_transaction_session_timeout",
            "idle_session_timeout",
            "application_name",
            "extra_float_digits",
            "datestyle",
            "intervalstyle",
            "client_min_messages",
            "bytea_output",
            "lc_monetary",
            "lc_numeric",
            "lc_time",
            "default_text_search_config",
            "work_mem",
            "jit",
            "random_page_cost",
            "max_parallel_workers_per_gather",
            "enable_seqscan",
            "default_transaction_isolation",
            "default_transaction_deferrable",
            "transaction_isolation",
            "xmloption",
        ],
        // From pg_catalog.pg_proc, filtered by volatility; pinned by a live test.
        builtin_functions: include_str!("functions/postgres.txt"),
        builtins: builtins::postgres::BUILTINS,
        binary_literal: Some(BinaryLiteral::EscapeString),
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
        lists_sequences: true,
        schema_switch_waits_for_transaction: true,
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const MYSQL: EngineSpec = EngineSpec {
    id: "mysql",
    built_in_driver: true,
    dbms_names: &["MySQL"],
    label: "MySQL",
    schemes: &["mysql"],
    file_extensions: &[],
    logo: "icons/engine-mysql.svg",
    brand: 0x4479A1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mysql::DEFAULT_PORT),
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    binary_text: Some(BinaryText::ZeroX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        double_quoted_strings: true,
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
        identity_defaults: &[DefaultMarker::Exact("AUTO_INCREMENT")],
        explain_plan: Some(ExplainPrefix::Explain),
        // Since 8.0.18. Server versions are not detected: an older server
        // refuses the statement and says so, which is the error the user needs
        // and is the server's to word rather than a version check's.
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::MySql,
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpLike,
        read_only_session: ReadOnlySession::SessionTransaction,
        // The armed read-only transaction stops none of the named-lock calls.
        // No sequences, so no `nextval`.
        denied_functions: &["get_lock", "release_lock", "release_all_locks"],
        // The one function that reads a file; `LOAD DATA` and `INTO OUTFILE`
        // are statements and the gate reads them as such.
        egress_functions: &["load_file"],
        // Left off on purpose: both spellings of the hold, the server-wide
        // switches, and `sql_mode`, whose `NO_BACKSLASH_ESCAPES` and
        // `ANSI_QUOTES` change how every later statement is lexed and are not
        // stopped by the armed hold.
        plain_settings: &[
            "time_zone",
            "autocommit",
            "max_execution_time",
            "sql_select_limit",
            "wait_timeout",
            "interactive_timeout",
            "lock_wait_timeout",
            "innodb_lock_wait_timeout",
            "net_read_timeout",
            "net_write_timeout",
            "group_concat_max_len",
            "lc_time_names",
            "div_precision_increment",
            "default_week_format",
            "transaction_isolation",
            "sql_big_selects",
            "max_join_size",
        ],
        // From the help tables, filtered by hand; no test pins it.
        builtin_functions: include_str!("functions/mysql.txt"),
        builtins: builtins::mysql::BUILTINS,
        binary_literal: Some(BinaryLiteral::XQuoted),
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
        catalog_lists_databases: true,
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks {
        fetched_literal: Some(mysql::fetched_literal),
        ..Hooks::NONE
    },
};

pub(crate) const MARIADB: EngineSpec = EngineSpec {
    id: "mariadb",
    built_in_driver: true,
    dbms_names: &["MariaDB"],
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
    takes_service: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    binary_text: Some(BinaryText::ZeroX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        double_quoted_strings: true,
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
        identity_defaults: &[DefaultMarker::Exact("AUTO_INCREMENT")],
        explain_plan: Some(ExplainPrefix::Explain),
        // MariaDB has no `EXPLAIN ANALYZE` and spells the same thing `ANALYZE`
        // with no `EXPLAIN`.
        explain_analyze: Some(ExplainPrefix::Analyze),
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpInstr,
        read_only_session: ReadOnlySession::SessionTransaction,
        // The armed read-only transaction stops none of the named-lock calls;
        // it does refuse a sequence advance, but a held session is not the
        // boundary.
        denied_functions: &[
            "get_lock",
            "release_lock",
            "release_all_locks",
            "nextval",
            "setval",
        ],
        // As MySQL's.
        egress_functions: &["load_file"],
        // The same list as MySQL's with the names MariaDB spells differently:
        // `max_statement_time` for the statement timeout, and `tx_isolation`
        // beside `transaction_isolation`. Its `tx_read_only` is a hold name and
        // stays off, as `transaction_read_only` does.
        plain_settings: &[
            "time_zone",
            "autocommit",
            "max_statement_time",
            "sql_select_limit",
            "wait_timeout",
            "interactive_timeout",
            "lock_wait_timeout",
            "innodb_lock_wait_timeout",
            "net_read_timeout",
            "net_write_timeout",
            "group_concat_max_len",
            "lc_time_names",
            "div_precision_increment",
            "default_week_format",
            "transaction_isolation",
            "tx_isolation",
            "sql_big_selects",
            "max_join_size",
        ],
        // From information_schema.sql_functions, filtered by hand; no test pins it.
        builtin_functions: include_str!("functions/mariadb.txt"),
        builtins: builtins::mysql::BUILTINS,
        binary_literal: Some(BinaryLiteral::XQuoted),
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
        catalog_lists_databases: true,
        draws_diagrams: true,
        lists_sequences: true,
        ..Abilities::NONE
    },
    hooks: Hooks {
        fetched_literal: Some(mysql::fetched_literal),
        ..Hooks::NONE
    },
};

/// What `sqlite::Schema::structure` writes into a generated column's default:
/// the expression where the table's `CREATE` parses, the bare mode where not.
const SQLITE_GENERATED: &[DefaultMarker] = &[
    DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
    DefaultMarker::Exact("GENERATED ALWAYS VIRTUAL"),
    DefaultMarker::Exact("GENERATED ALWAYS STORED"),
];

pub(crate) const SQLITE: EngineSpec = EngineSpec {
    id: "sqlite",
    built_in_driver: true,
    dbms_names: &["SQLite"],
    label: "SQLite",
    schemes: &["sqlite", "sqlite3", "file"],
    file_extensions: &["db", "sqlite", "sqlite3", "db3"],
    logo: "icons/engine-sqlite.svg",
    brand: 0x003B57,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    binary_text: Some(BinaryText::QuotedX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        nul_text: NulText::PipedChar,
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
        generated_defaults: SQLITE_GENERATED,
        // Inert while `rusqlite` is built without its `load_extension`
        // feature, and listed so enabling it cannot quietly open a hole.
        denied_functions: &["load_extension"],
        // Generated from the bundled build by
        // `the_built_in_list_is_written_from_the_bundled_catalog` and pinned by
        // `the_built_in_list_is_the_bundled_catalogs_answer`.
        builtin_functions: include_str!("functions/sqlite.txt"),
        // No `plain_settings`, deliberately: SQLite has no `SET` statement. It
        // configures through `PRAGMA`, a different statement type that already
        // needs Full, so a `SET` here is one the server rejects and stopping it
        // costs nobody a working statement.
        builtins: builtins::sqlite::BUILTINS,
        binary_literal: Some(BinaryLiteral::XQuoted),
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
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const TURSO: EngineSpec = EngineSpec {
    id: "turso",
    built_in_driver: true,
    dbms_names: &[],
    label: "Turso",
    schemes: &["turso"],
    file_extensions: &[],
    logo: "icons/engine-turso.svg",
    brand: 0x4FF8D2,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    binary_text: Some(BinaryText::QuotedX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        nul_text: NulText::PipedChar,
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        parser_dialect: ParserDialect::Sqlite,
        substring: SubstringMatch::Instr,
        // Inert for the same reason as SQLite's. The rest of Turso's function
        // set has not been surveyed.
        denied_functions: &["load_extension"],
        // SQLite's list: Turso's own function set has not been surveyed, so a
        // name SQLite lacks asks, and one Turso lacks costs nothing.
        builtin_functions: include_str!("functions/sqlite.txt"),
        // No `plain_settings`, for the reason SQLite has none: configuration is
        // `PRAGMA`, which is already Full, and there is no `SET` to allow.
        generated_defaults: SQLITE_GENERATED,
        builtins: builtins::sqlite::BUILTINS,
        binary_literal: Some(BinaryLiteral::XQuoted),
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const DUCKDB: EngineSpec = EngineSpec {
    id: "duckdb",
    built_in_driver: true,
    dbms_names: &["DuckDB"],
    label: "DuckDB",
    schemes: &["duckdb"],
    file_extensions: &["duckdb", "ddb"],
    logo: "icons/engine-duckdb.svg",
    brand: 0xFFF000,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    binary_text: Some(BinaryText::Bare),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        path: "Blank for a database in memory",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        nul_text: NulText::PipedChr,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: true,
        },
        generated_defaults: &[DefaultMarker::Prefix("GENERATED ALWAYS AS (")],
        parser_dialect: ParserDialect::DuckDb,
        regex: RegexMatch::RegexpMatches,
        substring: SubstringMatch::InstrCast,
        // No Explain: a plan comes as box-drawn text in one column, a shape
        // `explain.rs` does not read yet. No `read_only_session`: an open
        // database has no session switch; read-only is a way to open the
        // file, which a profile does not ask for yet.
        // Nothing holds a session read-only, so an advance on a sequence runs,
        // and the classifier is the only thing between a statement and its
        // effect. The text executors were observed, not assumed, on the
        // embedded build `SELECT version()` reports as `v2.0.0-dev88614`
        // (crate `duckdb` 1.20000.0), with the sequence counter as the canary:
        // `SELECT * FROM query('select nextval(...)')` moved it from 3 to 4,
        // and `SELECT * FROM json_execute_serialized_sql(json_serialize_sql(
        // 'select nextval(...)'))` moved it from 4 to 5, so both are listed.
        // `query_table` is not: it resolves its argument as a table name and
        // refuses the rest (`query_table('select 1')` answers "Table with name
        // select 1 does not exist"), so it runs no text and listing it would
        // only stop a read. `json_serialize_sql` and `json_deserialize_sql`
        // turn text into JSON and back and run nothing. `duckdb_functions()`
        // shows no other function with an argument named like a query. A
        // listed name followed by a parenthesis is a call and stopped; one
        // after a period is a column reference and a read, except
        // `nextval`, which `MEMBER_FORMS` keeps for Snowflake's spelling and
        // which therefore also stops a column called `t.nextval` here. What
        // is left of the cost of listing `query` is the name used as a CTE
        // name or a derived-table alias with a column list, which is followed
        // by a parenthesis and stopped until the profile is upgraded; both
        // are recorded in the corpus as accepted false positives.
        denied_functions: &["nextval", "setval", "query", "json_execute_serialized_sql"],
        // Generated from the bundled build by
        // `the_built_in_list_is_written_from_the_bundled_catalog` and pinned by
        // `the_built_in_list_is_the_bundled_catalogs_answer`.
        // ponytail: scalar, aggregate and macro functions only. A table function
        // reads files or the database's own state and `has_side_effects` says
        // nothing about it, so `FROM duckdb_tables()` asks. Upgrade path: a reviewed
        // list of local table functions, as ClickHouse's file has.
        builtin_functions: include_str!("functions/duckdb.txt"),
        // Every table function of the embedded build's `duckdb_functions()`
        // that reads a file (`read_*`, `parquet_*`, `*_scan`, `glob`,
        // `sniff_csv`), DBDelve's own `read_xlsx` among them, then the
        // extension readers that build does not load. Pinned by
        // `every_file_reading_table_function_is_on_the_egress_list`. The human
        // path keeps them reads: DBDelve's CSV, Excel, Parquet and JSON opens
        // are these calls.
        egress_functions: &[
            "arrow_scan",
            "glob",
            "parquet_bloom_probe",
            "parquet_file_metadata",
            "parquet_full_metadata",
            "parquet_kv_metadata",
            "parquet_metadata",
            "parquet_scan",
            "parquet_schema",
            "read_blob",
            "read_csv",
            "read_csv_auto",
            "read_duckdb",
            "read_json",
            "read_json_auto",
            "read_json_objects",
            "read_json_objects_auto",
            "read_ndjson",
            "read_ndjson_auto",
            "read_ndjson_objects",
            "read_parquet",
            "read_single_csv_file",
            "read_single_json_file",
            "read_single_parquet_file",
            "read_text",
            "read_xlsx",
            "seq_scan",
            "sniff_csv",
            "iceberg_scan",
            "delta_scan",
            "st_read",
            "sqlite_scan",
            "postgres_scan",
            "postgres_query",
            "mysql_scan",
            "mysql_query",
        ],
        // A quoted string in a table position is a file or a URL to DuckDB, so
        // the agent path asks for any path-shaped string (D-03).
        bare_path_reads: true,
        // Names verified in `duckdb_settings()`. The configuration ratchets
        // (`enable_external_access`, `lock_configuration`) are left off on
        // purpose: once set they cannot be undone, and one in a user's buffer
        // would end DBDelve's own CSV, Parquet and Excel opening for the rest
        // of the session. The extension, directory and secret settings are off
        // for the same reason.
        plain_settings: &[
            "threads",
            "memory_limit",
            "max_memory",
            "timezone",
            "calendar",
            "search_path",
            "schema",
            "default_order",
            "default_null_order",
            "preserve_insertion_order",
            "enable_progress_bar",
        ],
        builtins: builtins::duckdb::BUILTINS,
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
        draws_diagrams: true,
        lists_sequences: true,
        ..Abilities::NONE
    },
    hooks: Hooks {
        fetched_literal: Some(duckdb::fetched_literal),
        ..Hooks::NONE
    },
};

pub(crate) const SNOWFLAKE: EngineSpec = EngineSpec {
    id: "snowflake",
    built_in_driver: true,
    dbms_names: &["Snowflake"],
    label: "Snowflake",
    schemes: &["snowflake"],
    file_extensions: &[],
    logo: "icons/engine-snowflake.svg",
    brand: 0x29B5E8,
    syntax: Syntax::Sql,
    layout: Fields::Account,
    default_port: None,
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    // The account usage share every account has. `SNOWFLAKE_SAMPLE_DATA` is a
    // share too, but one people query.
    system_databases: &["SNOWFLAKE"],
    binary_types: &[],
    binary_text: Some(BinaryText::Bare),
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
        identifier_folds_upper: true,
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
        // `seq.nextval` is no call at all, which the scan reads off the period
        // before the name. Procedures are `CALL` statements, already Full.
        // Unlike the Postgres and DuckDB lists, this one was built by documentation:
        // the `SYSTEM$` control entries come from Snowflake's published function
        // reference, and none was run, because no live Snowflake account is
        // reachable from this repository's test environment and the mock in
        // `snowflake/mock.rs` carries no function catalog. There is no read-only
        // session to hold the rest, so on this engine the list is the whole
        // boundary, and it is not an enumerated family: `SYSTEM$` has further
        // members this list does not name. Listing them from documentation would
        // be a long list with no evidence behind any entry, so it stays best-effort
        // in the sense `calls_denied_function` states. A prefix rule, stopping
        // any `system$` word followed by a parenthesis, was weighed and not
        // taken: it would be a second matching mechanism for one engine where
        // the list names functions, and it would stop the read-only `SYSTEM$`
        // helpers on every Read-only profile with no evidence about which
        // members write. The residual is stated for users in the advisory's
        // `What users will see`.
        denied_functions: &[
            "system$cancel_all_queries",
            "system$cancel_query",
            "system$abort_session",
            "system$abort_transaction",
            "nextval",
            "getnextval",
        ],
        // No `plain_settings`, deliberately: nothing a `SET` sets outlives one
        // request on the SQL API, which answers even `USE` with a refusal, so
        // an allow-list would protect nothing. A `SET` here prompts and
        // changes nothing; the one extra arm is cheaper than a special case.
        // From the vendor documentation, not a server: no catalog lists the built-ins, so no test pins it.
        builtin_functions: include_str!("functions/snowflake.txt"),
        builtins: builtins::snowflake::BUILTINS,
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
        lists_sequences: true,
        // An exact `ROW_COUNT` per table, where the others' numbers are as of
        // the last time statistics were gathered, and SQLite keeps none.
        exact_row_estimates: true,
        chunk_refusal: Some(
            "Snowflake runs each request on its own, so no transaction can span the chunks.",
        ),
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SQL_SERVER: EngineSpec = EngineSpec {
    id: "mssql",
    built_in_driver: true,
    dbms_names: &["Microsoft SQL Server"],
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
    takes_service: false,
    resolves_srv: false,
    system_databases: &["master", "tempdb", "model", "msdb"],
    // `image` is the legacy binary type here; elsewhere it is a name anyone
    // may give a column or a domain, and a SQLite `image` column holds text.
    binary_types: &["image", "rowversion", "timestamp"],
    binary_text: Some(BinaryText::ZeroX),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        null_terms: true,
        drafts: true,
        nul_text: NulText::AddedNchar,
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
        identity_defaults: &[DefaultMarker::Prefix("IDENTITY(")],
        identity_insert: IdentityInsert::SessionSwitch,
        computed_types: &["rowversion", "timestamp"],
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
        // The pass-through table functions run a statement on another server,
        // and nothing holds a session read-only. A scalar function cannot have
        // a side effect in T-SQL, so none is listed.
        denied_functions: &["openquery", "openrowset", "opendatasource"],
        // From the documentation, not probed against a server: the functions
        // that read an audit, trace or extended-events file and the two
        // dynamic management functions that list the server's file system. The
        // pass-through functions above are on the stricter list.
        egress_functions: &[
            "fn_xe_file_target_read_file",
            "fn_get_audit_file",
            "fn_trace_gettable",
            "dm_os_file_exists",
            "dm_os_enumerate_filesystem",
        ],
        // Names from the engine's own category table. Left off on purpose:
        // `quoted_identifier`, which is read at parse time and changes how a
        // double quote is read, `implicit_transactions`, which DBDelve's
        // auto-commit switch relies on, and `identity_insert`. `set statistics`
        // and `set transaction isolation level` are read from the parse
        // instead, not from here.
        plain_settings: &[
            "nocount",
            "rowcount",
            "textsize",
            "language",
            "dateformat",
            "datefirst",
            "deadlock_priority",
            "lock_timeout",
            "arithabort",
            "arithignore",
            "numeric_roundabort",
            "ansi_nulls",
            "ansi_padding",
            "ansi_warnings",
            "concat_null_yields_null",
            "xact_abort",
        ],
        // From the vendor documentation, not a server: no catalog lists the built-ins, so no test pins it.
        builtin_functions: include_str!("functions/mssql.txt"),
        builtins: builtins::sqlserver::BUILTINS,
        binary_literal: Some(BinaryLiteral::ZeroX),
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
        lists_sequences: true,
        // No `picks_schema`: the default schema is the login's.
        draws_diagrams: true,
        ..Abilities::NONE
    },
    hooks: Hooks {
        quote_value: Some(mssql::typed_literal),
        fetched_literal: None,
        match_value: None,
    },
};

pub(crate) const MONGODB: EngineSpec = EngineSpec {
    id: "mongodb",
    built_in_driver: true,
    dbms_names: &[],
    label: "MongoDB",
    schemes: &["mongodb", "mongodb+srv"],
    file_extensions: &[],
    logo: "icons/engine-mongodb.svg",
    brand: 0x47A248,
    syntax: Syntax::Mongo,
    layout: Fields::Server,
    default_port: Some(mongo::DEFAULT_PORT),
    takes_options: true,
    takes_service: false,
    resolves_srv: true,
    system_databases: &["admin", "local", "config"],
    binary_types: &["bindata"],
    binary_text: Some(BinaryText::BinData),
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        user: "Optional",
        ..Hints::DEFAULT
    },
    // No `denied_functions` or `plain_settings`: this is not SQL, and its own
    // closed tables decide what a Read-only profile may run.
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        imports: true,
        copies_ddl: true,
        switches_database: true,
        one_schema_per_database: true,
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
    built_in_driver: true,
    dbms_names: &[],
    label: "Redis",
    schemes: &["redis", "rediss"],
    file_extensions: &[],
    logo: "icons/engine-redis.svg",
    brand: 0xFF4438,
    syntax: Syntax::Redis,
    layout: Fields::Server,
    default_port: Some(redis::DEFAULT_PORT),
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    binary_text: None,
    numeric_types: &[],
    type_wrappers: &[],
    hints: Hints {
        name: "Named after the host",
        database: "0",
        user: "Optional",
        ..Hints::DEFAULT
    },
    // No `denied_functions` or `plain_settings`: this is not SQL, and its own
    // closed tables decide what a Read-only profile may run.
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        switches_database: true,
        one_schema_per_database: true,
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
    built_in_driver: true,
    dbms_names: &["ClickHouse"],
    label: "ClickHouse",
    schemes: &["clickhouse"],
    file_extensions: &[],
    logo: "icons/engine-clickhouse.svg",
    brand: 0xFFCC01,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(clickhouse::DEFAULT_PORT),
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &["system", "INFORMATION_SCHEMA", "information_schema"],
    binary_types: &[],
    binary_text: None,
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
        // Empty is a position here, not the default: no ClickHouse function
        // writes from inside a `SELECT`. The table functions that reach outside
        // the database (`url`, `s3`, `remote`, `executable`) are plain reads by
        // the owner's decision (`docs/agents/engines.md`), and `INSERT INTO
        // FUNCTION` is Full by statement in `spec_verdict`, so neither belongs
        // on a list of calls to raise. The agent path asks for approval on
        // those table functions through `egress_functions` below (D-22).
        denied_functions: &[],
        // Every table function of the dev server (26.8.36.3,
        // `system.table_functions`) that reaches a file, a URL, a program or
        // another server, or runs text it is handed: `eval` builds a SELECT
        // from a string, so it could carry `url(..)` past a scan of names.
        // `cluster`, `clusterAllReplicas` and `dictionary` are off it: they read
        // servers and sources the administrator configured as part of the
        // deployment, not ones a statement names. Pinned by
        // `live_every_table_function_is_egress_or_local`.
        egress_functions: &[
            "url",
            "urlcluster",
            "s3",
            "s3cluster",
            "gcs",
            "cosn",
            "oss",
            "azureblobstorage",
            "azureblobstoragecluster",
            "hdfs",
            "hdfscluster",
            "hive",
            "deltalake",
            "deltalakeazure",
            "deltalakeazurecluster",
            "deltalakecluster",
            "deltalakelocal",
            "deltalakes3",
            "deltalakes3cluster",
            "iceberg",
            "icebergazure",
            "icebergazurecluster",
            "icebergcluster",
            "iceberghdfs",
            "iceberghdfscluster",
            "iceberglocal",
            "iceberglocalcluster",
            "icebergs3",
            "icebergs3cluster",
            "hudi",
            "hudicluster",
            "paimon",
            "paimonazure",
            "paimonazurecluster",
            "paimoncluster",
            "paimonhdfs",
            "paimonhdfscluster",
            "paimonlocal",
            "paimons3",
            "paimons3cluster",
            "arrowflight",
            "bigquery",
            "ytsaurus",
            "jdbc",
            "odbc",
            "mysql",
            "postgresql",
            "mongodb",
            "redis",
            "sqlite",
            "remote",
            "remotesecure",
            "file",
            "filecluster",
            "filesystem",
            "executable",
            "eval",
        ],
        // What a user's own `SET` may change on a Read-only profile without
        // asking, chosen as Postgres's and DuckDB's lists are: navigation and
        // comfort. `readonly=2` already refuses a change to `readonly`, but it
        // is the driver's per-request hold, so the verdict does not lean on it
        // and the name stays off. Left off on purpose, each for its own reason:
        // `readonly` and `max_execution_time` (the driver sends both on a
        // request, and the second is the bound a profile's timeout sets),
        // `format_tsv_null_representation` and `enable_http_compression` (the
        // driver decodes its answers by them), every `allow_*` switch (each
        // turns a server check off), and anything that changes who the session
        // is or how text is lexed. A name not listed is stopped rather than
        // assumed harmless.
        plain_settings: &[
            // Parallelism and block size change how fast a read runs, not what
            // it reads.
            "max_threads",
            "max_block_size",
            // A cap on one query's memory, the counterpart of DuckDB's
            // `memory_limit`.
            "max_memory_usage",
            // How a time is shown, the counterpart of Postgres's `timezone`.
            "session_timezone",
            // A label in the server's query log, the counterpart of Postgres's
            // `application_name`.
            "log_comment",
        ],
        // From system.functions, filtered by category; pinned by a live test.
        builtin_functions: include_str!("functions/clickhouse.txt"),
        case_sensitive_functions: true,
        builtins: builtins::clickhouse::BUILTINS,
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        switches_database: true,
        catalog_lists_databases: true,
        chunk_refusal: Some("ClickHouse has no transaction, so each chunk is kept as it lands."),
        ..Abilities::NONE
    },
    hooks: Hooks {
        match_value: Some(clickhouse::match_value),
        ..Hooks::NONE
    },
};

pub(crate) const GENERIC: EngineSpec = EngineSpec {
    id: "generic",
    built_in_driver: false,
    dbms_names: &[],
    label: "Generic SQL",
    schemes: &["generic"],
    file_extensions: &[],
    logo: "icons/engine-generic.svg",
    brand: 0x7A8594,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: None,
    takes_options: false,
    takes_service: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    binary_text: Some(BinaryText::ZeroX),
    // Db2's decimal float, which right-aligns like any number.
    numeric_types: &["decfloat"],
    type_wrappers: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        // Db2's `TRUNCATE TABLE` alone is a syntax error; `DELETE FROM` is the
        // one spelling every SQL database accepts.
        truncate: Truncate::DeleteFrom,
        // Db2, like the standard, has no default `LIKE` escape.
        substring: SubstringMatch::LikePlain,
        // `literal_offsets` only feeds Import Rows' date values, which no
        // plugin target admits yet; false is the answer that cannot send an
        // offset to a server whose timestamp literal takes none (Db2).
        literal_offsets: false,
        // Generic reads the list the human path always held: calls outside it need Read-write there. Hand-kept, no pin.
        builtin_functions: include_str!("functions/generic.txt"),
        builtins: builtins::ansi::BUILTINS,
        ..SqlSpec::ANSI
    },
    abilities: Abilities::NONE,
    hooks: Hooks::NONE,
};

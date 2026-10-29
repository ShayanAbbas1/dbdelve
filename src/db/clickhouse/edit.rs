//! Editing a ClickHouse table's rows: how a value is spelled as a predicate
//! term, and what one describe of a table decides about writing to it.
//!
//! ClickHouse has no unique key, so a row is named by every value it was read
//! with, and each spelling here has to be one the write gate admits.

use std::collections::{HashMap, HashSet};

use super::tsv::peel;
use super::{opens_with, queued_statements, write_of};
use crate::db::{
    Column, DbError, EditTarget, Engine, RowMatch, UpdateSpelling, cancel_cost, is_cancel,
    plain_error,
};
use crate::sql::{Projected, TableRead, Unread, bare_words, table_read_or_why};

const NOT_MERGE_TREE: &str = "ClickHouse edits rows of MergeTree tables only.";

const NOT_MATCHABLE: &str = "Rows with an AggregateFunction, Variant, Dynamic or JSON value, or \
                             bytes that are not text, cannot be matched by value.";

const PROJECTIONS: &str = "This table has projections and its lightweight_mutation_projection_mode \
                           is throw, so rows cannot be deleted.";

const GONE: &str = "No row has these values any more; it was changed or deleted since it was read.";

pub(super) const UNCOUNTED: &str =
    "DBDelve could not count the rows this edit would change, so it ran nothing.";

const RESPELLED: &str =
    "This table's settings changed since its rows were read, so the edit was not run.";

const BLOCK_NUMBER: &str = "enable_block_number_column";
const BLOCK_OFFSET: &str = "enable_block_offset_column";
const PROJECTION_MODE: &str = "lightweight_mutation_projection_mode";

/// The server's own setting, which the describe reads beside the table's.
pub(super) const SERVER_LIGHTWEIGHT_UPDATE: &str = "server.enable_lightweight_update";

pub(super) const DESCRIBE_TABLE_SQL: &str = "
SELECT engine, engine_full, create_table_query
FROM system.tables
WHERE database = {schema} AND name = {relation}";

pub(super) const DESCRIBE_COLUMNS_SQL: &str = "
SELECT name, type, default_kind,
    is_in_partition_key OR is_in_sorting_key OR is_in_primary_key AS in_key
FROM system.columns
WHERE database = {schema} AND table = {relation}";

pub(super) const DESCRIBE_SETTINGS_SQL: &str = "
SELECT name, value FROM system.merge_tree_settings
WHERE name IN ('enable_block_number_column', 'enable_block_offset_column',
               'lightweight_mutation_projection_mode')
UNION ALL
SELECT 'server.enable_lightweight_update', value FROM system.settings
WHERE name = 'enable_lightweight_update'";

enum Spelling {
    /// `` `c` = '<text>' ``: the server converts the literal to the column's
    /// type exactly, no two values of the type are shown as the same text, and
    /// the primary key's index still applies.
    Equal,
    /// ``toString(`c`) = '<text>'``: the server's own text against the text it
    /// sent, for a type no literal converts to exactly or whose text can name
    /// two values. Every row shown as this text matches, so the guard counts
    /// them all.
    Text,
    Unmatchable,
}

/// Fails closed: a type this does not know is not matched.
fn spelling(data_type: &str) -> Spelling {
    let (base, _) = peel(data_type);
    match base.split('(').next().unwrap_or(base) {
        "String" | "FixedString" | "Enum8" | "Enum16" | "Bool" | "UUID" | "IPv4" | "IPv6"
        | "Date" | "Date32" | "UInt8" | "UInt16" | "UInt32" | "UInt64" | "UInt128" | "UInt256"
        | "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256" => Spelling::Equal,
        // A `DateTime` is shown in its time zone, the server's when it names
        // none, where the hour a clock goes back is shown twice: the literal
        // names only one of the two instants.
        "DateTime" | "Float32" | "Float64" | "BFloat16" | "Decimal" | "Decimal32" | "Decimal64"
        | "Decimal128" | "Decimal256" | "DateTime64" | "Time" | "Time64" | "Array" | "Tuple"
        | "Map" | "Nested" | "Point" | "Ring" | "LineString" | "MultiLineString" | "Polygon"
        | "MultiPolygon" => Spelling::Text,
        _ => Spelling::Unmatchable,
    }
}

/// How the grid shows a string that is not UTF-8: `0x` and upper-case hex.
#[cfg_attr(not(test), allow(dead_code))]
fn is_hex(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty()
            && digits.len() % 2 == 0
            && digits
                .bytes()
                .all(|digit| digit.is_ascii_digit() || (b'A'..=b'F').contains(&digit))
    })
}

/// The whole predicate term for `column`'s value as read (`None` for NULL), or
/// `None` where the value cannot be matched: a type with no exact spelling, a
/// `\0`, or bytes that were shown as hex.
#[cfg_attr(not(test), allow(dead_code))] // spelled by the hook the abilities switch on
pub(in crate::db) fn match_value(
    column: &str,
    value: Option<&str>,
    data_type: &str,
) -> Option<String> {
    let spec = Engine::ClickHouse.spec();
    let column = spec.quote_identifier(column);
    let Some(value) = value else {
        return Some(format!("{column} IS NULL"));
    };
    if value.contains('\0') {
        return None;
    }
    let literal = spec.quote_literal(value);
    match spelling(data_type) {
        Spelling::Equal if is_hex(value) && is_string(data_type) => None,
        Spelling::Equal => Some(format!("{column} = {literal}")),
        Spelling::Text => Some(format!("toString({column}) = {literal}")),
        Spelling::Unmatchable => None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn is_string(data_type: &str) -> bool {
    let (base, _) = peel(data_type);
    base.starts_with("String") || base.starts_with("FixedString")
}

/// One column of the table, as `system.columns` has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Described {
    pub name: String,
    pub data_type: String,
    pub default_kind: String,
    pub in_key: bool,
}

/// What `system.tables`, `system.columns` and the two settings sources say
/// about one table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TableFacts {
    pub engine: String,
    pub engine_full: String,
    pub create_table_query: String,
    pub columns: Vec<Described>,
    /// The server's value for each setting the describe reads, and
    /// [`SERVER_LIGHTWEIGHT_UPDATE`].
    pub server: HashMap<String, String>,
}

/// What a table allows, decided once from [`TableFacts`] and kept for the
/// connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Table {
    /// Why no row can be written, when none can.
    pub refusal: Option<String>,
    pub update: UpdateSpelling,
    /// Why a row cannot be deleted though it can be updated.
    pub delete_refusal: Option<&'static str>,
    columns: Vec<Described>,
}

impl Table {
    pub fn of(facts: TableFacts) -> Self {
        let family = family(&facts.engine);
        let settings = table_settings(&facts.engine_full);
        let setting = |name: &str| -> Option<&str> {
            settings
                .get(name)
                .or_else(|| facts.server.get(name))
                .map(String::as_str)
        };
        let on = |name: &str| matches!(setting(name), Some("1" | "true"));
        let lightweight = family.is_some_and(|family| {
            !matches!(
                family,
                "Summing" | "Aggregating" | "Graphite" | "Coalescing"
            ) && facts
                .server
                .get(SERVER_LIGHTWEIGHT_UPDATE)
                .map(String::as_str)
                == Some("1")
                && on(BLOCK_NUMBER)
                && on(BLOCK_OFFSET)
        });
        let delete_refusal = (has_projection(&facts.create_table_query)
            && setting(PROJECTION_MODE) == Some("throw"))
        .then_some(PROJECTIONS);
        Self {
            refusal: family.is_none().then(|| NOT_MERGE_TREE.to_string()),
            update: match lightweight {
                true => UpdateSpelling::Update,
                false => UpdateSpelling::AlterUpdate,
            },
            delete_refusal,
            columns: facts.columns,
        }
    }

    /// The edit target of a `SELECT *` of this table whose result is
    /// `columns`, or the reason its rows cannot be written. `(None, None)`
    /// where the result is not the whole row of this table.
    pub fn target(
        &self,
        schema: &str,
        table: &str,
        columns: &[Column],
    ) -> (Option<EditTarget>, Option<String>) {
        let origins: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
        self.target_of(schema, table, columns, &origins)
    }

    /// The edit target of a result of this table whose columns are `columns`,
    /// each read from the table's column `origins` names at its place, or the
    /// reason its rows cannot be written. `(None, None)` where an origin is no
    /// column of the table, or there are none. A result of some of the
    /// table's columns names its rows by those alone, which the guard keeps
    /// safe: a write still runs only where exactly one row has those values.
    pub fn target_of(
        &self,
        schema: &str,
        table: &str,
        columns: &[Column],
        origins: &[String],
    ) -> (Option<EditTarget>, Option<String>) {
        if let Some(reason) = &self.refusal {
            return (None, Some(reason.clone()));
        }
        let described: Option<Vec<&Described>> =
            origins.iter().map(|origin| self.column(origin)).collect();
        let Some(described) =
            described.filter(|described| !described.is_empty() && described.len() == columns.len())
        else {
            return (None, None);
        };
        let types = columns
            .iter()
            .map(|column| column.data_type.as_deref().unwrap_or_default());
        if types
            .clone()
            .any(|data_type| matches!(spelling(data_type), Spelling::Unmatchable))
        {
            return (None, Some(NOT_MATCHABLE.to_string()));
        }
        let fixed = described
            .iter()
            .zip(types)
            .enumerate()
            .filter(|(_, (known, data_type))| {
                known.in_key
                    || matches!(known.default_kind.as_str(), "MATERIALIZED" | "ALIAS")
                    || peel(data_type).0.starts_with("AggregateFunction")
                    // The server casts a string to a Map nowhere an assignment
                    // takes one, though an INSERT parses the same text.
                    || peel(data_type).0.starts_with("Map(")
            })
            .map(|(position, _)| position)
            .collect();
        let target = EditTarget {
            schema: schema.to_string(),
            table: table.to_string(),
            columns: origins.iter().cloned().map(Some).collect(),
            keys: (0..columns.len()).collect(),
            row_match: RowMatch::EveryColumn { fixed },
            update: self.update,
        };
        (Some(target), None)
    }

    fn column(&self, name: &str) -> Option<&Described> {
        self.columns.iter().find(|known| known.name == name)
    }

    /// Why `write` may not run on this table, when it may not.
    pub fn refuses(&self, write: Write) -> Option<String> {
        if let Some(reason) = &self.refusal {
            return Some(reason.clone());
        }
        match write {
            Write::Delete => self.delete_refusal.map(str::to_string),
            Write::Update(spelling) => (spelling != self.update).then(|| RESPELLED.to_string()),
        }
    }
}

const SELECT_LIST: &str = "Only rows selected as the table's own columns, or *, can be edited.";

const FINAL: &str = "FINAL merges rows as they are read, so these rows cannot be edited.";

pub(super) const SESSION_SET: &str = "This session ran a SET, which can change how values are \
                                      shown, so DBDelve cannot match these rows by value. Open \
                                      the table from the explorer to edit its rows.";

pub(super) const TEMPORARY: &str = "These rows are from a temporary table, which only this session \
                                    can see, so they cannot be edited.";

const TWICE: &str = "These rows hold a column twice, so they cannot be edited.";

const UNMAPPED: &str = "DBDelve could not tell which column of the table each result column is, \
                        so these rows cannot be edited.";

/// What a user's query reads when it is a select of one table's own rows
/// ([`table_read_or_why`]), with the names its select list gives by `AS`, or
/// why its rows cannot be written back.
pub(super) fn query_shape(sql: &str) -> Result<(TableRead, Vec<String>), &'static str> {
    // The grammar takes `t FINAL` for `t` aliased as `FINAL`, and cannot read
    // `t AS x FINAL` at all.
    if bare_words(Engine::ClickHouse, sql)
        .is_some_and(|words| words.iter().any(|word| word.eq_ignore_ascii_case("FINAL")))
    {
        return Err(FINAL);
    }
    table_read_or_why(Engine::ClickHouse, sql).map_err(unread)
}

/// Why a user's query has no edit target, by what [`table_read_or_why`]
/// found in it.
pub(super) fn unread(why: Unread) -> &'static str {
    match why {
        Unread::Unparsed => {
            "DBDelve cannot read this statement whole, so it cannot tell which table its rows are \
             from."
        }
        Unread::NotAQuery => "Only the rows of a SELECT can be edited.",
        Unread::With => "These rows are read through WITH, so they cannot be edited.",
        Unread::NotOneSelect => {
            "These rows are not from one SELECT of a table, so they cannot be edited."
        }
        Unread::Settings => {
            "A SETTINGS clause can change how values are shown, so rows read with one cannot be \
             edited."
        }
        Unread::Format => "Rows read in a FORMAT of their own cannot be edited.",
        Unread::Distinct => "DISTINCT merges identical rows, so these rows cannot be edited.",
        Unread::Grouped => "Grouped rows are not rows of the table, so they cannot be edited.",
        Unread::Filled => {
            "WITH FILL adds rows the table does not hold, so these rows cannot be edited."
        }
        Unread::Join => "These rows come from a join, so they cannot be edited.",
        Unread::ArrayJoin => {
            "ARRAY JOIN repeats a row once for each element, so these rows cannot be edited."
        }
        Unread::NotATable => "These rows are not read from a table, so they cannot be edited.",
        Unread::Projection => SELECT_LIST,
        Unread::Other => "DBDelve edits the rows of a plain SELECT of one table only.",
    }
}

/// The column of `table` (named `name`) behind each of `columns`, the result
/// of `read`, or why the rows cannot be written back. A `*` stands for as
/// many result columns as the rest of the list leaves, which the server names
/// as the table does; a name in the list is read as written.
///
/// `shadows` are the names the list reads as something else (an alias, a
/// table's qualifier); one that is also a column's name refuses the lot, since
/// the server may read it the other way: an alias wins over a column wherever
/// its name appears, so `SELECT a AS b, b` reads `a` twice, and `t.x` can be a
/// field of a column `t`. So does a result column whose type is not its
/// column's: the name was something else to the server, `inf` the literal
/// beside a column `inf`. An `ALIAS` column is computed as it is read, so a
/// value of one need not name the same row a moment later.
pub(super) fn origins(
    read: &TableRead,
    shadows: &[String],
    columns: &[Column],
    table: &Table,
    name: &str,
) -> Result<Vec<String>, String> {
    if let Some(shadow) = shadows.iter().find(|shadow| table.column(shadow).is_some()) {
        return Err(format!(
            "{shadow} is a column's name and is read here as something else too, so DBDelve \
             cannot tell which one each value is."
        ));
    }
    let stars = read
        .columns
        .iter()
        .filter(|projected| matches!(projected, Projected::Every))
        .count();
    if stars > 1 {
        return Err(TWICE.into());
    }
    let spread = columns
        .len()
        .checked_sub(read.columns.len() - stars)
        .ok_or_else(|| UNMAPPED.to_string())?;
    let mut origins = Vec::with_capacity(columns.len());
    for projected in &read.columns {
        match projected {
            Projected::Every => {
                let at = origins.len();
                origins.extend(columns[at..at + spread].iter().map(|c| c.name.clone()));
            }
            Projected::Column(written) => origins.push(written.clone()),
            Projected::Computed => return Err(SELECT_LIST.into()),
        }
    }
    if origins.len() != columns.len() {
        return Err(UNMAPPED.into());
    }
    if let Some(missing) = origins.iter().find(|origin| table.column(origin).is_none()) {
        return Err(format!(
            "{missing} is not a column of {name}, so these rows cannot be edited."
        ));
    }
    for (origin, column) in origins.iter().zip(columns) {
        let Some(described) = table.column(origin) else {
            return Err(UNMAPPED.into());
        };
        if column.data_type.as_deref() != Some(described.data_type.as_str()) {
            return Err(format!(
                "{} is not read as the column {origin} is typed, so these rows cannot be edited.",
                column.name
            ));
        }
        if described.default_kind == "ALIAS" {
            return Err(format!(
                "{origin} is an ALIAS column, computed as it is read, so these rows cannot be \
                 edited."
            ));
        }
    }
    let mut seen = HashSet::new();
    if !origins.iter().all(|origin| seen.insert(origin)) {
        return Err(TWICE.into());
    }
    Ok(origins)
}

const NO_TRANSACTION: &str = "ClickHouse has no transaction. The statements run one at a time and \
                              stop at the first one refused or failed; the ones before it stay applied.";

const ONE_ROW: &str = "Each row is matched by all of its values. Just before each statement runs, \
                       DBDelve counts the rows that match and runs it only if exactly one does.";

const MUTATION: &str = "This table cannot take a lightweight UPDATE, so the change is an ALTER TABLE \
                        ... UPDATE mutation. It rewrites the parts holding the row, which can be \
                        slow on a large table, and it returns once every replica has applied it.";

const PROJECTION_DELETE: &str = "A table with projections deletes rows only when \
                                 lightweight_mutation_projection_mode is not throw; DBDelve refuses \
                                 the delete before it runs otherwise.";

const WRITTEN_AS_TEXT: &str = "Values go to the server as quoted text and it converts each to its \
                               column's type. A Variant or Dynamic value is stored as a String. A \
                               FixedString holding a NUL byte, a Nested column's dotted parts and \
                               an AggregateFunction state cannot be written, and a Map cannot be \
                               edited, only inserted.";

/// The server's refusal of a write, with `readonly` said in words a user can
/// act on: the server names the setting the write needed (`mutations_sync`),
/// not the user's mode. A missing grant is already plain.
pub(super) fn explained(error: DbError) -> DbError {
    if !error.message.contains("(READONLY)") {
        return error;
    }
    DbError {
        message: format!(
            "This ClickHouse user is read-only, so it cannot change rows. The server said: {}",
            error.message
        ),
        ..error
    }
}

pub(super) const LIGHTWEIGHT_UPDATE_HINT: &str = "ALTER TABLE ... UPDATE ... WHERE ... SETTINGS \
     mutations_sync = 2 changes the rows of a table without the block columns.";

/// The server's refusal of a typed lightweight `UPDATE` on a table that lacks
/// the block columns, with its words kept and one line added. The error's code
/// (48) is gone from the message by now, so it is read by its name beside the
/// wording that says which refusal it is: the same prefix also opens the
/// refusals for a unique key and for the Summing, Aggregating and Graphite
/// engines, which block columns would not mend. The statement is never
/// rewritten.
pub(super) fn hinted(error: DbError) -> DbError {
    let message = &error.message;
    let for_want_of_block_columns = ["_block_number", "_block_offset"]
        .iter()
        .any(|column| message.contains(&format!("materialized {column} column")));
    if !message.contains("(NOT_IMPLEMENTED)")
        || !message.contains("Lightweight updates are not supported")
        || !for_want_of_block_columns
    {
        return error;
    }
    DbError {
        message: format!("{message}\n{LIGHTWEIGHT_UPDATE_HINT}"),
        ..error
    }
}

/// What the review of a batch of generated writes tells the user beyond the
/// statements themselves.
pub(in crate::db) fn review_notes(sql: &str) -> Vec<String> {
    let statements = queued_statements(Engine::ClickHouse, sql, None);
    let writes: Vec<&str> = statements
        .iter()
        .map(|range| &sql[range.clone()])
        .filter(|statement| !opens_with(statement, &["INSERT"]))
        .collect();
    let any = |write: fn(Write) -> bool| writes.iter().any(|statement| write(write_of(statement)));
    let mut notes = Vec::new();
    if statements.len() > writes.len() {
        notes.push(WRITTEN_AS_TEXT.to_string());
    }
    if !writes.is_empty() {
        notes.push(ONE_ROW.to_string());
    }
    if any(|write| matches!(write, Write::Update(UpdateSpelling::AlterUpdate))) {
        notes.push(MUTATION.to_string());
    }
    if any(|write| matches!(write, Write::Delete)) {
        notes.push(PROJECTION_DELETE.to_string());
    }
    if statements.len() > 1 {
        notes.push(NO_TRANSACTION.to_string());
    }
    notes
}

/// What a generated write does to the rows its predicate names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Write {
    Update(UpdateSpelling),
    Delete,
}

/// Why a write may not change the rows its guard counted, `None` for the one
/// row it may change.
pub(super) fn count_refusal(count: u64) -> Option<String> {
    match count {
        1 => None,
        0 => Some(GONE.into()),
        many => Some(format!(
            "{many} rows have exactly these values, so DBDelve will not change one of them."
        )),
    }
}

/// Why a write submission stopped where it did.
#[derive(Debug)]
pub(super) enum Halt {
    /// Not sent: its guard or the table's describe would not have it.
    Refused(String),
    Failed(Stage, DbError),
}

/// How far a statement of a submission got before it failed. Only at
/// [`Stage::Write`] did the server have it, so only there can it have
/// written anything or started a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// Describing the table it writes to.
    Describe,
    /// Its guard, counting the rows it names.
    Count,
    /// About to be sent, when a Cancel came first.
    Unsent,
    Write,
}

/// A lone statement cancelled before it went out. Not [`CANCELLED`]'s words,
/// so nothing adds what a cancel of the statement itself would cost.
///
/// [`CANCELLED`]: super::CANCELLED
const CANCELLED_UNSENT: &str = "Cancelled before it was sent.";

/// The error of a submission of `total` statements that stopped at the one
/// at index `at`, `offset` bytes into it. The ones before it ran and, with no
/// transaction to undo them, stay; the ones after it were never sent.
pub(super) fn halted(
    at: usize,
    total: usize,
    halt: Halt,
    statement: &str,
    offset: usize,
) -> DbError {
    let (verb, reason, position) = match halt {
        // A lone statement's refusal or failure is said as it is, and a
        // cancel of the statement itself stays one.
        Halt::Refused(reason) if total == 1 => return plain_error(reason),
        Halt::Failed(Stage::Write, error) if total == 1 => {
            return DbError {
                position: error.position.map(|position| position + offset),
                ..error
            };
        }
        // A position here would point into the guard or the describe, not
        // into the statement the user was shown.
        Halt::Failed(_, error) if total == 1 => {
            return match is_cancel(Engine::ClickHouse, &error) {
                true => plain_error(CANCELLED_UNSENT.into()),
                false => plain_error(error.message),
            };
        }
        Halt::Refused(reason) => ("was refused", reason, None),
        Halt::Failed(Stage::Write, error) => {
            let mut reason = error.message.clone();
            if is_cancel(Engine::ClickHouse, &error)
                && let Some(cost) = cancel_cost(Engine::ClickHouse, &error, false, statement)
            {
                reason = format!("{reason} {cost}");
            }
            let position = error.position.map(|position| position + offset);
            ("failed", reason, position)
        }
        Halt::Failed(Stage::Describe, error) => (
            "failed before it was sent, while DBDelve read its table",
            error.message,
            None,
        ),
        Halt::Failed(Stage::Count, error) => (
            "failed before it was sent, while DBDelve counted its rows",
            error.message,
            None,
        ),
        Halt::Failed(Stage::Unsent, error) => ("failed before it was sent", error.message, None),
    };
    let after = match total - at - 1 {
        0 => String::new(),
        1 => ", and the one after it was not run".into(),
        rest => format!(", and the {rest} after it were not run"),
    };
    let which = ordinal(at + 1);
    let message = match at {
        0 => format!("The first of the {total} edits {verb}{after}: {reason}"),
        1 => format!(
            "1 of {total} edits was applied and stays; ClickHouse has no transaction. The {which} \
             {verb}{after}: {reason}"
        ),
        applied => format!(
            "{applied} of {total} edits were applied and stay; ClickHouse has no transaction. The \
             {which} {verb}{after}: {reason}"
        ),
    };
    DbError { message, position }
}

/// `first` to `tenth` in words, then `11th`, `22nd` and on.
fn ordinal(n: usize) -> String {
    const WORDS: [&str; 10] = [
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        "tenth",
    ];
    if let Some(word) = n.checked_sub(1).and_then(|at| WORDS.get(at)) {
        return (*word).to_string();
    }
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The MergeTree family a table's engine is in, without its `Replicated` or
/// `Shared` form: `Replacing` for `ReplicatedReplacingMergeTree`, empty for a
/// plain `MergeTree`. `None` for any other engine.
fn family(engine: &str) -> Option<&str> {
    let engine = engine
        .strip_prefix("Replicated")
        .or_else(|| engine.strip_prefix("Shared"))
        .unwrap_or(engine);
    let family = engine.strip_suffix("MergeTree")?;
    matches!(
        family,
        "" | "Replacing"
            | "Collapsing"
            | "VersionedCollapsing"
            | "Summing"
            | "Aggregating"
            | "Graphite"
            | "Coalescing"
    )
    .then_some(family)
}

/// The settings a table was created with, or altered to: the `SETTINGS`
/// clause the server prints at the end of the engine.
fn table_settings(engine_full: &str) -> HashMap<String, String> {
    let Some((_, clause)) = engine_full.rsplit_once(" SETTINGS ") else {
        return HashMap::new();
    };
    clause
        .split(',')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            let value = value.trim().trim_matches('\'');
            Some((name.trim().to_string(), value.to_string()))
        })
        .collect()
}

/// Whether the statement has a `PROJECTION` clause, outside every quoted name
/// and string.
fn has_projection(create: &str) -> bool {
    let mut outside = String::with_capacity(create.len());
    let mut quote = None;
    let mut escaped = false;
    for c in create.chars() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(_) if c == '\\' => escaped = true,
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if matches!(c, '`' | '\'' | '"') => {
                quote = Some(c);
                outside.push(' ');
            }
            None => outside.push(c),
        }
    }
    outside
        .split_whitespace()
        .any(|word| word.eq_ignore_ascii_case("PROJECTION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql;

    fn gated(term: &str) -> bool {
        let spec = Engine::ClickHouse.spec();
        let update = format!("UPDATE `d`.`t` SET `a` = 'x' WHERE {term}");
        let delete = format!("DELETE FROM `d`.`t` WHERE {term}");
        let alter = format!(
            "ALTER TABLE `d`.`t` UPDATE `a` = 'x' WHERE {term} SETTINGS mutations_sync = 2"
        );
        sql::is_generated_write_on(Engine::ClickHouse, &update)
            && sql::is_generated_write_on(Engine::ClickHouse, &alter)
            && sql::is_generated_write_on(Engine::ClickHouse, &delete)
            && sql::delete_matches_key_on(Engine::ClickHouse, &delete, &["c"])
            && spec.sql.row_terms
    }

    #[test]
    fn each_type_is_matched_as_the_spec_says_and_the_gate_admits_it() {
        let equal = [
            "String",
            "Nullable(String)",
            "LowCardinality(String)",
            "LowCardinality(Nullable(String))",
            "FixedString(4)",
            "Enum8('a' = 1)",
            "Enum16('a' = 1)",
            "Bool",
            "UUID",
            "IPv4",
            "IPv6",
            "Date",
            "Date32",
            "UInt8",
            "UInt256",
            "Int64",
            "Int128",
            "SimpleAggregateFunction(max, UInt64)",
        ];
        for data_type in equal {
            let term = match_value("c", Some("v"), data_type);
            assert_eq!(term.as_deref(), Some("`c` = 'v'"), "{data_type}");
            assert!(gated(&term.unwrap()), "{data_type}");
        }
        let text = [
            "Float32",
            "Float64",
            "BFloat16",
            "Decimal(10, 2)",
            "Decimal64(3)",
            "DateTime",
            "DateTime('UTC')",
            "DateTime('America/New_York')",
            "Nullable(DateTime('Europe/London'))",
            "LowCardinality(Nullable(DateTime))",
            "SimpleAggregateFunction(max, DateTime('America/New_York'))",
            "DateTime64(6)",
            "DateTime64(3, 'UTC')",
            "Array(String)",
            "Nullable(Float64)",
            "Tuple(String, UInt8)",
            "Map(String, UInt8)",
            "Nested(a UInt8)",
            "Point",
            "Polygon",
        ];
        for data_type in text {
            let term = match_value("c", Some("v"), data_type);
            assert_eq!(term.as_deref(), Some("toString(`c`) = 'v'"), "{data_type}");
            assert!(gated(&term.unwrap()), "{data_type}");
        }
    }

    #[test]
    fn a_null_is_matched_as_is_null_for_any_type() {
        for data_type in ["Nullable(String)", "Float64", "Variant(UInt8, String)", ""] {
            let term = match_value("c", None, data_type).unwrap();
            assert_eq!(term, "`c` IS NULL");
            assert!(gated(&term));
        }
    }

    #[test]
    fn a_value_is_quoted_so_the_gate_still_reads_one_literal() {
        let tricky = [
            "it's",
            "back\\slash",
            "ends with \\",
            "two\nlines\tand a tab",
            "'; DROP TABLE t; --",
            "naïve ✓",
            "",
            "toString(c) = 'x' OR 1 = 1",
        ];
        for value in tricky {
            for data_type in ["String", "Array(String)"] {
                let term = match_value("c", Some(value), data_type).unwrap();
                assert!(gated(&term), "{term}");
            }
        }
        let term = match_value("a`b", Some("v"), "String").unwrap();
        assert_eq!(term, "`a``b` = 'v'");
    }

    #[test]
    fn a_wall_clock_time_shown_twice_is_matched_as_text_so_both_rows_match() {
        let term = match_value(
            "ts",
            Some("2024-11-03 01:30:00"),
            "DateTime('America/New_York')",
        );
        assert_eq!(
            term.as_deref(),
            Some("toString(`ts`) = '2024-11-03 01:30:00'")
        );
    }

    #[test]
    fn the_float_spellings_are_the_servers_text() {
        for value in ["nan", "inf", "-inf", "-0", "1.5e-7"] {
            let term = match_value("c", Some(value), "Float64").unwrap();
            assert_eq!(term, format!("toString(`c`) = '{value}'"));
            assert!(gated(&term));
        }
    }

    #[test]
    fn a_value_with_no_exact_spelling_is_not_matched() {
        for data_type in [
            "Variant(UInt8, String)",
            "Dynamic",
            "JSON",
            "Nothing",
            "AggregateFunction(sum, UInt64)",
            "Nullable(Nothing)",
            "SomeFutureType",
            "",
        ] {
            assert_eq!(match_value("c", Some("v"), data_type), None, "{data_type}");
        }
    }

    #[test]
    fn a_nul_byte_is_not_matched_in_any_type() {
        for data_type in ["String", "FixedString(3)", "Array(String)", "Float64"] {
            assert_eq!(match_value("c", Some("a\0b"), data_type), None);
        }
    }

    #[test]
    fn a_string_shown_as_hex_is_not_matched_but_a_number_or_look_alike_is() {
        for data_type in ["String", "Nullable(String)", "FixedString(2)"] {
            assert_eq!(match_value("c", Some("0xFFFE"), data_type), None);
        }
        assert!(match_value("c", Some("0xff"), "String").is_some());
        assert!(match_value("c", Some("0x"), "String").is_some());
        assert!(match_value("c", Some("0xABC"), "String").is_some());
        assert!(match_value("c", Some("0xAB"), "Array(String)").is_some());
    }

    fn column(name: &str, data_type: &str, default_kind: &str, in_key: bool) -> Described {
        Described {
            name: name.into(),
            data_type: data_type.into(),
            default_kind: default_kind.into(),
            in_key,
        }
    }

    fn facts(engine: &str, engine_full: &str, create: &str) -> TableFacts {
        TableFacts {
            engine: engine.into(),
            engine_full: engine_full.into(),
            create_table_query: create.into(),
            columns: vec![
                column("id", "UInt32", "", true),
                column("label", "String", "", false),
            ],
            server: HashMap::from([
                (SERVER_LIGHTWEIGHT_UPDATE.into(), "1".into()),
                (BLOCK_NUMBER.into(), "0".into()),
                (BLOCK_OFFSET.into(), "0".into()),
                (PROJECTION_MODE.into(), "throw".into()),
            ]),
        }
    }

    const BLOCKS: &str = " SETTINGS enable_block_number_column = 1, \
                          enable_block_offset_column = 1, index_granularity = 8192";

    fn update_of(engine: &str, settings: &str) -> UpdateSpelling {
        let engine_full = format!("{engine} ORDER BY id{settings}");
        Table::of(facts(engine, &engine_full, "CREATE TABLE t (id UInt32)")).update
    }

    #[test]
    fn a_merge_tree_with_both_block_settings_updates_in_place() {
        for engine in [
            "MergeTree",
            "ReplacingMergeTree",
            "CollapsingMergeTree",
            "VersionedCollapsingMergeTree",
            "ReplicatedMergeTree",
            "SharedReplacingMergeTree",
        ] {
            assert_eq!(
                update_of(engine, BLOCKS),
                UpdateSpelling::Update,
                "{engine}"
            );
        }
    }

    #[test]
    fn a_table_missing_either_block_setting_is_updated_by_a_mutation() {
        assert_eq!(update_of("MergeTree", ""), UpdateSpelling::AlterUpdate);
        assert_eq!(
            update_of(
                "MergeTree",
                " SETTINGS enable_block_number_column = 1, index_granularity = 8192"
            ),
            UpdateSpelling::AlterUpdate
        );
        assert_eq!(
            update_of(
                "MergeTree",
                " SETTINGS enable_block_number_column = 1, enable_block_offset_column = 0"
            ),
            UpdateSpelling::AlterUpdate
        );
    }

    #[test]
    fn the_servers_default_stands_in_for_a_setting_the_table_does_not_set() {
        let mut facts = facts(
            "MergeTree",
            "MergeTree ORDER BY id",
            "CREATE TABLE t (id UInt32)",
        );
        facts.server.insert(BLOCK_NUMBER.into(), "1".into());
        facts.server.insert(BLOCK_OFFSET.into(), "1".into());
        assert_eq!(Table::of(facts.clone()).update, UpdateSpelling::Update);

        facts.engine_full = "MergeTree ORDER BY id SETTINGS enable_block_offset_column = 0".into();
        assert_eq!(Table::of(facts).update, UpdateSpelling::AlterUpdate);
    }

    #[test]
    fn summing_aggregating_and_graphite_are_always_updated_by_a_mutation() {
        for engine in [
            "SummingMergeTree",
            "AggregatingMergeTree",
            "GraphiteMergeTree",
            "CoalescingMergeTree",
            "ReplicatedSummingMergeTree",
        ] {
            let table = Table::of(facts(engine, &format!("{engine} ORDER BY id{BLOCKS}"), ""));
            assert_eq!(table.update, UpdateSpelling::AlterUpdate, "{engine}");
            assert_eq!(table.refusal, None, "{engine}");
        }
    }

    #[test]
    fn a_server_with_lightweight_updates_off_is_updated_by_a_mutation() {
        let mut facts = facts("MergeTree", &format!("MergeTree ORDER BY id{BLOCKS}"), "");
        facts
            .server
            .insert(SERVER_LIGHTWEIGHT_UPDATE.into(), "0".into());
        assert_eq!(Table::of(facts.clone()).update, UpdateSpelling::AlterUpdate);
        facts.server.remove(SERVER_LIGHTWEIGHT_UPDATE);
        assert_eq!(Table::of(facts).update, UpdateSpelling::AlterUpdate);
    }

    #[test]
    fn only_the_merge_tree_family_has_editable_rows() {
        for engine in [
            "Memory",
            "Log",
            "TinyLog",
            "StripeLog",
            "View",
            "MaterializedView",
            "Distributed",
            "Dictionary",
            "Merge",
            "Buffer",
            "Null",
            "Set",
            "Join",
            "File",
            "URL",
            "MySQL",
            "ReplicatedFoo",
            "MergeTreeish",
        ] {
            let table = Table::of(facts(engine, engine, ""));
            assert_eq!(table.refusal.as_deref(), Some(NOT_MERGE_TREE), "{engine}");
            assert_eq!(
                table.target("d", "t", &[]),
                (None, Some(NOT_MERGE_TREE.into()))
            );
        }
    }

    #[test]
    fn a_projection_withholds_delete_only_while_the_mode_is_throw() {
        let create = "CREATE TABLE t (`id` UInt32, PROJECTION p (SELECT id GROUP BY id)) \
                      ENGINE = MergeTree ORDER BY id";
        let mut with = facts("MergeTree", "MergeTree ORDER BY id", create);
        let table = Table::of(with.clone());
        assert_eq!(table.delete_refusal, Some(PROJECTIONS));
        assert_eq!(table.refusal, None);

        with.engine_full = "MergeTree ORDER BY id SETTINGS \
                            lightweight_mutation_projection_mode = 'rebuild'"
            .into();
        assert_eq!(Table::of(with.clone()).delete_refusal, None);

        with.engine_full = "MergeTree ORDER BY id".into();
        with.server.insert(PROJECTION_MODE.into(), "drop".into());
        assert_eq!(Table::of(with).delete_refusal, None);

        let plain = facts(
            "MergeTree",
            "MergeTree ORDER BY id",
            "CREATE TABLE t (id UInt32)",
        );
        assert_eq!(Table::of(plain).delete_refusal, None);
    }

    #[test]
    fn a_column_or_comment_that_says_projection_is_not_a_projection() {
        let create = "CREATE TABLE t (`projection` UInt8, `x` String COMMENT 'PROJECTION p') \
                      ENGINE = MergeTree ORDER BY x COMMENT 'a PROJECTION'";
        let table = Table::of(facts("MergeTree", "MergeTree ORDER BY x", create));
        assert_eq!(table.delete_refusal, None);
    }

    fn result(columns: &[(&str, &str)]) -> Vec<Column> {
        columns
            .iter()
            .map(|(name, data_type)| Column {
                name: (*name).into(),
                data_type: Some((*data_type).into()),
            })
            .collect()
    }

    #[test]
    fn the_target_matches_every_column_and_fixes_the_key_ones() {
        let mut facts = facts("MergeTree", &format!("MergeTree ORDER BY id{BLOCKS}"), "");
        facts
            .columns
            .push(column("agg", "AggregateFunction(sum, UInt64)", "", false));
        facts
            .columns
            .push(column("day", "Date", "MATERIALIZED", false));
        let table = Table::of(facts);

        let columns = result(&[("id", "UInt32"), ("label", "String"), ("day", "Date")]);
        let (target, reason) = table.target("d", "t", &columns);
        assert_eq!(reason, None);
        let target = target.unwrap();
        assert_eq!((target.schema.as_str(), target.table.as_str()), ("d", "t"));
        assert_eq!(
            target.columns,
            vec![Some("id".into()), Some("label".into()), Some("day".into())]
        );
        assert_eq!(target.keys, vec![0, 1, 2]);
        assert_eq!(
            target.row_match,
            RowMatch::EveryColumn { fixed: vec![0, 2] }
        );
        assert_eq!(target.update, UpdateSpelling::Update);
    }

    #[test]
    fn a_map_column_is_fixed_because_no_assignment_casts_text_to_one() {
        let mut facts = facts("MergeTree", "MergeTree ORDER BY id", "");
        facts
            .columns
            .push(column("m", "Map(String, UInt8)", "", false));
        facts
            .columns
            .push(column("a", "Array(Map(String, UInt8))", "", false));
        let columns = result(&[
            ("id", "UInt32"),
            ("m", "Map(String, UInt8)"),
            ("a", "Array(Map(String, UInt8))"),
        ]);
        let (target, _) = Table::of(facts).target("d", "t", &columns);
        assert_eq!(
            target.unwrap().row_match,
            RowMatch::EveryColumn { fixed: vec![0, 1] }
        );
    }

    #[test]
    fn a_column_the_table_does_not_have_leaves_the_result_unwritable() {
        let table = Table::of(facts("MergeTree", "MergeTree ORDER BY id", ""));
        let columns = result(&[("id", "UInt32"), ("nope", "String")]);
        assert_eq!(table.target("d", "t", &columns), (None, None));
        assert_eq!(table.target("d", "t", &[]), (None, None));
    }

    #[test]
    fn a_column_that_cannot_be_matched_makes_the_rows_read_only() {
        let mut facts = facts("MergeTree", "MergeTree ORDER BY id", "");
        for data_type in [
            "Variant(UInt8, String)",
            "Dynamic",
            "JSON",
            "AggregateFunction(sum, UInt64)",
        ] {
            facts.columns.push(column("v", data_type, "", false));
            let table = Table::of(facts.clone());
            let columns = result(&[("id", "UInt32"), ("v", data_type)]);
            assert_eq!(
                table.target("d", "t", &columns),
                (None, Some(NOT_MATCHABLE.into())),
                "{data_type}"
            );
            facts.columns.pop();
        }
    }

    #[test]
    fn a_query_of_one_tables_own_rows_is_read_off_its_statement() {
        let every = || vec![Projected::Every];
        let none: Vec<&str> = Vec::new();
        for (sql, table, columns, shadows) in [
            (
                "SELECT * FROM edit_lw",
                vec!["edit_lw"],
                every(),
                none.clone(),
            ),
            (
                "SELECT * FROM `d`.`t` WHERE id > 1 ORDER BY id LIMIT 10 OFFSET 5",
                vec!["d", "t"],
                every(),
                none.clone(),
            ),
            (
                "/* rows */ select * from d.t",
                vec!["d", "t"],
                every(),
                none.clone(),
            ),
            (
                "SELECT * FROM t LIMIT 1 BY id",
                vec!["t"],
                every(),
                none.clone(),
            ),
            (
                "SELECT * FROM t PREWHERE id = 1 WHERE label != ''",
                vec!["t"],
                every(),
                none.clone(),
            ),
            (
                "SELECT * FROM t SAMPLE 0.5",
                vec!["t"],
                every(),
                none.clone(),
            ),
            (
                "SELECT label, x.id, x.* FROM d.t AS x",
                vec!["d", "t"],
                vec![
                    Projected::Column("label".into()),
                    Projected::Column("id".into()),
                    Projected::Every,
                ],
                vec!["x", "x"],
            ),
            (
                "SELECT id AS ident, label AS label, d.t.qty FROM d.t",
                vec!["d", "t"],
                vec![
                    Projected::Column("id".into()),
                    Projected::Column("label".into()),
                    Projected::Column("qty".into()),
                ],
                vec!["ident", "d"],
            ),
            // Names are case-sensitive: `T` is a column, and `T.x` a field of it.
            (
                "SELECT T.x FROM d.t AS t",
                vec!["d", "t"],
                vec![Projected::Computed],
                none.clone(),
            ),
        ] {
            let (read, found) = query_shape(sql).unwrap_or_else(|why| panic!("{sql}: {why}"));
            assert_eq!(read.table, table, "{sql}");
            assert_eq!(read.columns, columns, "{sql}");
            assert_eq!(found, shadows, "{sql}");
        }
    }

    #[test]
    fn every_other_shape_of_query_is_refused_saying_why() {
        for (sql, reason) in [
            ("SELECT * FROM t FINAL", FINAL),
            ("SELECT * FROM d.t final WHERE id = 1", FINAL),
            ("SELECT * FROM t AS x FINAL", FINAL),
            (
                "SELECT * FROM t JOIN u ON t.id = u.id",
                unread(Unread::Join),
            ),
            ("SELECT * FROM t, u", unread(Unread::Join)),
            (
                "SELECT id, count() FROM t GROUP BY id",
                unread(Unread::Grouped),
            ),
            (
                "SELECT id FROM t ARRAY JOIN tags",
                unread(Unread::ArrayJoin),
            ),
            (
                "SELECT id FROM t LEFT ARRAY JOIN tags AS tag",
                unread(Unread::ArrayJoin),
            ),
            (
                "WITH x AS (SELECT * FROM t) SELECT * FROM x",
                unread(Unread::With),
            ),
            ("SELECT * FROM (SELECT * FROM t)", unread(Unread::NotATable)),
            ("SELECT * FROM numbers(10)", unread(Unread::NotATable)),
            ("SELECT 1", unread(Unread::NotATable)),
            (
                "SELECT * FROM t SETTINGS session_timezone = 'Asia/Tokyo'",
                unread(Unread::Settings),
            ),
            (
                "SELECT * FROM t SETTINGS max_threads = 1",
                unread(Unread::Settings),
            ),
            ("SELECT * FROM t FORMAT JSON", unread(Unread::Format)),
            ("SELECT DISTINCT * FROM t", unread(Unread::Distinct)),
            (
                "SELECT * FROM t UNION ALL SELECT * FROM t",
                unread(Unread::NotOneSelect),
            ),
            (
                "SELECT * FROM t ORDER BY id WITH FILL",
                unread(Unread::Filled),
            ),
            ("SHOW TABLES", unread(Unread::NotAQuery)),
            ("SELECT * FROM t WHERE", unread(Unread::Unparsed)),
            ("SELECT * EXCEPT (id) FROM t", unread(Unread::Projection)),
        ] {
            assert_eq!(query_shape(sql).map(drop), Err(reason), "{sql}");
        }
        // A select item that is not a column is the caller's to refuse.
        let (read, _) = query_shape("SELECT id + 1 FROM t").unwrap();
        assert_eq!(read.columns, [Projected::Computed]);
    }

    /// `sql` read off `(id UInt32, label String, qty Int32, inf String, total
    /// Int64 ALIAS …, t String)`, its header `header`, each column typed as
    /// the table types it unless `typed` says otherwise.
    fn mapped_as(
        sql: &str,
        header: &[&str],
        typed: &[(&str, &str)],
    ) -> Result<Vec<String>, String> {
        let mut facts = facts("MergeTree", "MergeTree ORDER BY id", "");
        facts.columns.push(column("qty", "Int32", "", false));
        facts.columns.push(column("inf", "String", "", false));
        facts.columns.push(column("total", "Int64", "ALIAS", false));
        facts.columns.push(column("t", "String", "", false));
        let types: HashMap<String, String> = facts
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.data_type.clone()))
            .chain(
                typed
                    .iter()
                    .map(|(name, data_type)| (name.to_string(), data_type.to_string())),
            )
            .collect();
        let table = Table::of(facts);
        let (read, shadows) = query_shape(sql).unwrap();
        let columns: Vec<(&str, &str)> = header
            .iter()
            .map(|name| (*name, types.get(*name).map_or("String", String::as_str)))
            .collect();
        origins(&read, &shadows, &result(&columns), &table, "t")
    }

    fn mapped(sql: &str, header: &[&str]) -> Result<Vec<String>, String> {
        mapped_as(sql, header, &[])
    }

    #[test]
    fn each_result_column_is_mapped_to_the_table_column_it_was_read_from() {
        assert_eq!(
            mapped("SELECT * FROM t", &["id", "label", "qty"]).unwrap(),
            ["id", "label", "qty"]
        );
        assert_eq!(
            mapped("SELECT qty, id FROM t", &["qty", "id"]).unwrap(),
            ["qty", "id"]
        );
        assert_eq!(
            mapped("SELECT label AS name, x.id FROM t AS x", &["name", "id"]).unwrap(),
            ["label", "id"]
        );
    }

    #[test]
    fn a_result_column_that_cannot_be_mapped_exactly_refuses_the_rows() {
        let shadowed = |name: &str| {
            format!(
                "{name} is a column's name and is read here as something else too, so DBDelve \
                 cannot tell which one each value is."
            )
        };
        for (sql, header, reason) in [
            // The server reads the second `qty` as the alias: `id` twice.
            (
                "SELECT id AS qty, qty FROM t",
                vec!["qty", "qty"],
                shadowed("qty"),
            ),
            // `t.label` could be a field of the column `t`.
            ("SELECT t.label FROM d.t AS t", vec!["label"], shadowed("t")),
            (
                "SELECT total, id FROM t",
                vec!["total", "id"],
                "total is an ALIAS column, computed as it is read, so these rows cannot be \
                 edited."
                    .to_string(),
            ),
            ("SELECT id, id FROM t", vec!["id", "id"], TWICE.to_string()),
            (
                "SELECT id, * FROM t",
                vec!["id", "id", "label", "qty"],
                TWICE.to_string(),
            ),
            ("SELECT *, * FROM t", vec!["id", "id"], TWICE.to_string()),
            (
                "SELECT _part, * FROM t",
                vec!["_part", "id", "label", "qty"],
                "_part is not a column of t, so these rows cannot be edited.".to_string(),
            ),
            // An alias naming no column is read as the alias it is.
            (
                "SELECT id AS ident, ident FROM t",
                vec!["ident", "ident"],
                "ident is not a column of t, so these rows cannot be edited.".to_string(),
            ),
            (
                "SELECT id + 1 FROM t",
                vec!["plus(id, 1)"],
                SELECT_LIST.to_string(),
            ),
            ("SELECT id, label FROM t", vec!["id"], UNMAPPED.to_string()),
        ] {
            assert_eq!(mapped(sql, &header), Err(reason), "{sql}");
        }
        // The server reads a bare `inf` as the number, not the column.
        assert_eq!(
            mapped_as(
                "SELECT inf, id FROM t",
                &["inf", "id"],
                &[("inf", "Float64")]
            ),
            Err(
                "inf is not read as the column inf is typed, so these rows cannot be edited."
                    .to_string()
            )
        );
    }

    #[test]
    fn some_of_a_tables_columns_name_their_rows_by_those_alone_and_the_guard_keeps_it_one_row() {
        let mut facts = facts("MergeTree", &format!("MergeTree ORDER BY id{BLOCKS}"), "");
        facts
            .columns
            .push(column("day", "Date", "MATERIALIZED", false));
        facts.columns.push(column("qty", "Int64", "", false));
        let table = Table::of(facts);
        let columns = result(&[("day", "Date"), ("shown", "String")]);
        let origins = ["day".to_string(), "label".to_string()];
        let (target, reason) = table.target_of("d", "t", &columns, &origins);
        assert_eq!(reason, None);
        let target = target.unwrap();
        assert_eq!(
            target.columns,
            vec![Some("day".into()), Some("label".into())]
        );
        // Only the two columns read name the row, so other rows can share
        // them; what keeps the write to one row is the guard's count of
        // exactly this predicate, which refuses any count but one.
        assert_eq!(target.keys, vec![0, 1]);
        assert_eq!(target.row_match, RowMatch::EveryColumn { fixed: vec![0] });
        assert_eq!(count_refusal(1), None);
        assert!(count_refusal(2).is_some());
    }

    #[test]
    fn a_table_refuses_a_delete_it_withholds_and_an_update_spelled_for_other_settings() {
        let create = "CREATE TABLE t (`id` UInt32, PROJECTION p (SELECT id GROUP BY id))";
        let table = Table::of(facts("MergeTree", "MergeTree ORDER BY id", create));
        assert_eq!(table.refuses(Write::Delete), Some(PROJECTIONS.into()));
        assert_eq!(
            table.refuses(Write::Update(UpdateSpelling::AlterUpdate)),
            None
        );
        assert_eq!(
            table.refuses(Write::Update(UpdateSpelling::Update)),
            Some(RESPELLED.into())
        );

        let memory = Table::of(facts("Memory", "Memory", "CREATE TABLE t (id UInt32)"));
        for write in [Write::Delete, Write::Update(UpdateSpelling::AlterUpdate)] {
            assert_eq!(memory.refuses(write), Some(NOT_MERGE_TREE.into()));
        }
    }

    #[test]
    fn a_count_other_than_one_refuses_in_its_own_words() {
        assert_eq!(count_refusal(1), None);
        assert_eq!(count_refusal(0).as_deref(), Some(GONE));
        assert_eq!(
            count_refusal(3).as_deref(),
            Some("3 rows have exactly these values, so DBDelve will not change one of them.")
        );
    }

    fn refused() -> Halt {
        Halt::Refused("No.".into())
    }

    #[test]
    fn a_halt_says_which_edits_stay_and_which_never_ran() {
        let at = |at, total| halted(at, total, refused(), "", 0).message;
        assert_eq!(at(0, 1), "No.");
        assert_eq!(
            at(0, 3),
            "The first of the 3 edits was refused, and the 2 after it were not run: No."
        );
        assert_eq!(
            at(1, 2),
            "1 of 2 edits was applied and stays; ClickHouse has no transaction. The second was \
             refused: No."
        );
        assert_eq!(
            at(2, 3),
            "2 of 3 edits were applied and stay; ClickHouse has no transaction. The third was \
             refused: No."
        );
        assert_eq!(
            at(11, 14),
            "11 of 14 edits were applied and stay; ClickHouse has no transaction. The 12th was \
             refused, and the 2 after it were not run: No."
        );
    }

    #[test]
    fn a_lone_failure_is_the_servers_and_a_batchs_points_into_the_submission() {
        let failed = || {
            Halt::Failed(
                Stage::Write,
                DbError {
                    message: "Syntax error (SYNTAX_ERROR)".into(),
                    position: Some(4),
                },
            )
        };
        let lone = halted(0, 1, failed(), "", 10);
        assert_eq!(lone.message, "Syntax error (SYNTAX_ERROR)");
        assert_eq!(lone.position, Some(14));
        let batch = halted(1, 2, failed(), "", 10);
        assert!(
            batch
                .message
                .ends_with("The second failed: Syntax error (SYNTAX_ERROR)")
        );
        assert_eq!(batch.position, Some(14));
    }

    const MUTATION_SQL: &str = "ALTER TABLE `d`.`t` UPDATE `a` = 'x' WHERE `a` = 'y' \
                                SETTINGS mutations_sync = 2";

    fn cancelled(stage: Stage) -> Halt {
        Halt::Failed(stage, plain_error(super::super::CANCELLED.into()))
    }

    #[test]
    fn a_cancel_stays_one_alone_and_says_what_a_mutation_keeps_doing_in_a_batch() {
        let lone = halted(0, 1, cancelled(Stage::Write), MUTATION_SQL, 0);
        assert!(is_cancel(Engine::ClickHouse, &lone));
        let batch = halted(1, 2, cancelled(Stage::Write), MUTATION_SQL, 0);
        assert!(
            batch.message.ends_with(
                "The second failed: Cancelled. The mutation keeps running in the background; \
                 KILL MUTATION stops it."
            ),
            "{}",
            batch.message
        );
    }

    #[test]
    fn a_cancel_before_the_mutation_was_sent_says_it_was_not_sent_and_nothing_of_its_cost() {
        for (stage, verb) in [
            (
                Stage::Describe,
                "failed before it was sent, while DBDelve read its table",
            ),
            (
                Stage::Count,
                "failed before it was sent, while DBDelve counted its rows",
            ),
            (Stage::Unsent, "failed before it was sent"),
        ] {
            let batch = halted(1, 3, cancelled(stage), MUTATION_SQL, 10);
            assert_eq!(
                batch.message,
                format!(
                    "1 of 3 edits was applied and stays; ClickHouse has no transaction. The \
                     second {verb}, and the one after it was not run: Cancelled."
                ),
                "{stage:?}"
            );
            assert_eq!(batch.position, None);

            let lone = halted(0, 1, cancelled(stage), MUTATION_SQL, 0);
            assert_eq!(lone.message, CANCELLED_UNSENT, "{stage:?}");
            // The run adds what a cancel costs only to a cancel.
            assert!(!is_cancel(Engine::ClickHouse, &lone));
        }
    }

    #[test]
    fn a_guard_or_describe_failure_keeps_its_words_but_not_its_position() {
        let failed = |stage| {
            Halt::Failed(
                stage,
                DbError {
                    message: "Memory limit exceeded (MEMORY_LIMIT_EXCEEDED)".into(),
                    position: Some(4),
                },
            )
        };
        for stage in [Stage::Describe, Stage::Count] {
            let lone = halted(0, 1, failed(stage), MUTATION_SQL, 10);
            assert_eq!(
                (lone.message.as_str(), lone.position),
                ("Memory limit exceeded (MEMORY_LIMIT_EXCEEDED)", None)
            );
            let batch = halted(1, 2, failed(stage), MUTATION_SQL, 10);
            assert!(!batch.message.contains("KILL MUTATION"));
            assert_eq!(batch.position, None);
        }
    }

    fn noted(sql: &str) -> Vec<String> {
        review_notes(sql)
    }

    const UPDATE: &str = "UPDATE `d`.`t` SET `a` = 'x' WHERE `id` = '1'";
    const ALTER: &str =
        "ALTER TABLE `d`.`t` UPDATE `a` = 'x' WHERE `id` = '1' SETTINGS mutations_sync = 2";
    const DELETE: &str = "DELETE FROM `d`.`t` WHERE `id` = '1'";
    const INSERT: &str = "INSERT INTO `d`.`t` (`a`) VALUES ('x')";

    #[test]
    fn an_update_says_it_is_matched_by_every_value_and_counted_first() {
        assert_eq!(noted(UPDATE), [ONE_ROW]);
    }

    #[test]
    fn a_mutation_says_it_rewrites_parts() {
        assert_eq!(noted(ALTER), [ONE_ROW, MUTATION]);
        assert!(MUTATION.contains("rewrites the parts"));
    }

    #[test]
    fn a_delete_says_what_a_projection_does() {
        assert_eq!(noted(DELETE), [ONE_ROW, PROJECTION_DELETE]);
    }

    #[test]
    fn an_insert_says_what_cannot_be_written_faithfully() {
        assert_eq!(noted(INSERT), [WRITTEN_AS_TEXT]);
        for words in [
            "Variant",
            "Dynamic",
            "NUL",
            "Nested",
            "AggregateFunction",
            "Map",
        ] {
            assert!(WRITTEN_AS_TEXT.contains(words), "{words}");
        }
    }

    #[test]
    fn a_batch_says_nothing_stays_undone_and_each_note_once() {
        let batch = format!("{UPDATE};\n{ALTER};\n{UPDATE};");
        assert_eq!(noted(&batch), [ONE_ROW, MUTATION, NO_TRANSACTION]);
        assert!(NO_TRANSACTION.contains("stay applied"));
    }

    #[test]
    fn the_ten_engines_add_nothing_to_a_review() {
        for engine in Engine::ALL {
            if engine != Engine::ClickHouse {
                assert!(engine.review_notes(UPDATE).is_empty(), "{engine:?}");
                assert!(engine.review_notes(INSERT).is_empty(), "{engine:?}");
            }
        }
        assert!(!Engine::ClickHouse.review_notes(UPDATE).is_empty());
    }

    #[test]
    fn a_refused_lightweight_update_keeps_the_servers_words_and_gains_one_line() {
        let refused = plain_error(
            "Lightweight updates are not supported. Lightweight updates are supported only for \
             tables with materialized _block_number column. Run 'MODIFY SETTING \
             enable_block_number_column = 1' command to enable it. (NOT_IMPLEMENTED)"
                .into(),
        );
        let hinted = hinted(refused.clone());
        assert_eq!(
            hinted.message,
            format!("{}\n{LIGHTWEIGHT_UPDATE_HINT}", refused.message)
        );
        assert!(hinted.message.starts_with(&refused.message));

        let offset = plain_error(
            "Lightweight updates are not supported. Lightweight updates are supported only for \
             tables with materialized _block_offset column. Run 'MODIFY SETTING \
             enable_block_offset_column = 1' command to enable it. (NOT_IMPLEMENTED)"
                .into(),
        );
        assert!(
            super::hinted(offset)
                .message
                .ends_with(LIGHTWEIGHT_UPDATE_HINT)
        );

        for other in [
            "Cannot modify 'mutations_sync' setting in readonly mode. (READONLY)",
            "Method read is not supported by storage Memory. (NOT_IMPLEMENTED)",
            "Lightweight updates are not supported. (BAD_ARGUMENTS)",
            "Lightweight updates are not supported. (NOT_IMPLEMENTED)",
            "Lightweight updates are not supported. Lightweight updates are not supported on \
             tables with UNIQUE KEY. (NOT_IMPLEMENTED)",
            "Lightweight updates are not supported. Lightweight updates are not supported for \
             tables with SummingMergeTree engine. (NOT_IMPLEMENTED)",
            "Lightweight updates are not supported. Lightweight updates are supported only for \
             tables with custom partitioning. (NOT_IMPLEMENTED)",
        ] {
            let error = plain_error(other.into());
            assert_eq!(super::hinted(error.clone()), error, "{other}");
        }
    }

    #[test]
    fn a_readonly_user_is_told_so_and_a_missing_grant_is_left_as_the_server_said() {
        let readonly = explained(plain_error(
            "Code: 164. DB::Exception: Cannot modify 'mutations_sync' setting in readonly mode. \
             (READONLY)"
                .into(),
        ));
        assert!(
            readonly
                .message
                .starts_with("This ClickHouse user is read-only")
        );
        assert!(readonly.message.contains("mutations_sync"));
        let denied = plain_error(
            "Code: 497. DB::Exception: u: Not enough privileges. To execute this query, it's \
             necessary to have the grant ALTER UPDATE ON d.t. (ACCESS_DENIED)"
                .into(),
        );
        assert_eq!(explained(denied.clone()).message, denied.message);
    }

    #[test]
    fn ordinals_are_words_to_ten_then_figures() {
        let spelled: Vec<String> = [1, 2, 3, 10, 11, 12, 13, 21, 22, 23, 101, 111]
            .map(ordinal)
            .into();
        assert_eq!(
            spelled,
            [
                "first", "second", "third", "tenth", "11th", "12th", "13th", "21st", "22nd",
                "23rd", "101st", "111th"
            ]
        );
    }

    #[test]
    fn the_settings_clause_is_read_from_the_end_of_the_engine() {
        let settings = table_settings(
            "MergeTree ORDER BY id SETTINGS index_granularity = 8192, \
             lightweight_mutation_projection_mode = 'rebuild'",
        );
        assert_eq!(settings["index_granularity"], "8192");
        assert_eq!(settings["lightweight_mutation_projection_mode"], "rebuild");
        assert!(table_settings("MergeTree ORDER BY id").is_empty());
    }
}

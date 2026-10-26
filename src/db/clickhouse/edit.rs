//! Editing a ClickHouse table's rows: how a value is spelled as a predicate
//! term, and what one describe of a table decides about writing to it.
//!
//! ClickHouse has no unique key, so a row is named by every value it was read
//! with, and each spelling here has to be one the write gate admits.

use std::collections::HashMap;

use super::tsv::peel;
use crate::db::{
    Column, DbError, EditTarget, Engine, RowMatch, UpdateSpelling, cancel_cost, is_cancel,
    plain_error,
};

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
    /// type exactly, and the primary key's index still applies.
    Equal,
    /// ``toString(`c`) = '<text>'``: the server's own text against the text it
    /// sent, for a type no literal converts to exactly.
    Text,
    Unmatchable,
}

/// Fails closed: a type this does not know is not matched.
fn spelling(data_type: &str) -> Spelling {
    let (base, _) = peel(data_type);
    match base.split('(').next().unwrap_or(base) {
        "String" | "FixedString" | "Enum8" | "Enum16" | "Bool" | "UUID" | "IPv4" | "IPv6"
        | "Date" | "Date32" | "DateTime" | "UInt8" | "UInt16" | "UInt32" | "UInt64" | "UInt128"
        | "UInt256" | "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256" => Spelling::Equal,
        "Float32" | "Float64" | "BFloat16" | "Decimal" | "Decimal32" | "Decimal64"
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
        if let Some(reason) = &self.refusal {
            return (None, Some(reason.clone()));
        }
        let described: Option<Vec<&Described>> = columns
            .iter()
            .map(|column| self.columns.iter().find(|known| known.name == column.name))
            .collect();
        let Some(described) = described.filter(|described| !described.is_empty()) else {
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
                    || known.default_kind == "MATERIALIZED"
                    || peel(data_type).0.starts_with("AggregateFunction")
            })
            .map(|(position, _)| position)
            .collect();
        let target = EditTarget {
            schema: schema.to_string(),
            table: table.to_string(),
            columns: columns
                .iter()
                .map(|column| Some(column.name.clone()))
                .collect(),
            keys: (0..columns.len()).collect(),
            row_match: RowMatch::EveryColumn { fixed },
            update: self.update,
        };
        (Some(target), None)
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
    Failed(DbError),
}

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
        // cancel stays one.
        Halt::Refused(reason) if total == 1 => return plain_error(reason),
        Halt::Failed(error) if total == 1 => {
            return DbError {
                position: error.position.map(|position| position + offset),
                ..error
            };
        }
        Halt::Refused(reason) => ("was refused", reason, None),
        Halt::Failed(error) => {
            let mut reason = error.message.clone();
            if is_cancel(Engine::ClickHouse, &error)
                && let Some(cost) = cancel_cost(Engine::ClickHouse, &error, false, statement)
            {
                reason = format!("{reason} {cost}");
            }
            let position = error.position.map(|position| position + offset);
            ("failed", reason, position)
        }
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
            "DateTime",
            "DateTime('UTC')",
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
            Halt::Failed(DbError {
                message: "Syntax error (SYNTAX_ERROR)".into(),
                position: Some(4),
            })
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

    #[test]
    fn a_cancel_stays_one_alone_and_says_what_a_mutation_keeps_doing_in_a_batch() {
        let cancel = || Halt::Failed(plain_error(super::super::CANCELLED.into()));
        let lone = halted(0, 1, cancel(), "", 0);
        assert!(is_cancel(Engine::ClickHouse, &lone));
        let mutation = "ALTER TABLE `d`.`t` UPDATE `a` = 'x' WHERE `a` = 'y' \
                        SETTINGS mutations_sync = 2";
        let batch = halted(1, 2, cancel(), mutation, 0);
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

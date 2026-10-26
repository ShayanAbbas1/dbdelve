//! Finding statement boundaries in a query buffer.
//!
//! `cmd+enter` runs the statement under the cursor, so we need to know where
//! each statement starts and ends. Splitting on `;` is wrong — a semicolon can
//! sit inside a string literal, a line comment, or a `$$`-quoted function body,
//! and each of those would be cut in the wrong place. We run a real parser.
//!
//! gpui-component highlights with tree-sitter internally but keeps the tree
//! private, so this is a second, independent parse of the same text. For a
//! query buffer that cost is irrelevant.
//!
//! Statement ranges **exclude the terminating semicolon** — that is where the
//! grammar puts the node boundary, and it is what we want, since Postgres does
//! not need a trailing semicolon on a statement sent over the wire.

use std::ops::Range;

use serde::Deserialize;
// Aliased: `tree_sitter::Parser` already owns the name `Parser` in this file,
// and the two parsers are never interchangeable -- see `classify`'s doc.
use sqlparser::ast::{
    AlterTableOperation, ConditionalStatementBlock, CopySource, CopyTarget, Delete, Expr, Insert,
    Merge, Query, SetExpr, Statement, Update, UtilityOption,
};
use sqlparser::dialect::{
    Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect,
    SQLiteDialect, SnowflakeDialect,
};
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser as SqlParser;
use sqlparser::tokenizer::{Location, Token, Tokenizer, Whitespace};
use tree_sitter::{Node, Parser, Tree};

use crate::db::{
    ColumnDefinition, Engine, EngineSpec, ExplainMode, ExplainPrefix, FormatDialect, Paging,
    Params, ParserDialect, SkipDuplicates, StatementSplit, Syntax, TypedExplain,
};
use crate::import_rows::Value;
use crate::result_grid::{NewValue, PendingRow};

/// The runnable statements of a query buffer, as byte ranges into it.
pub struct Buffer {
    statements: Vec<Range<usize>>,
}

impl Buffer {
    /// Every dialect's buffer but SQL Server's, which [`Buffer::for_engine`]
    /// reads.
    pub fn parse(sql: &str) -> Self {
        Self {
            statements: statements_in(sql),
        }
    }

    /// T-SQL is read a batch at a time, the way the server reads it: `GO`
    /// lines are hard boundaries that are never part of a statement, `[names]`
    /// are quoted, a routine's body runs to the end of its batch, and a
    /// `BEGIN … END` block is one statement however many `;`s it holds. Sending
    /// the second `DELETE` of an `IF … BEGIN … END` alone runs it outside its
    /// `IF`.
    ///
    /// A MongoDB buffer is mongosh statements, not SQL, and `mql` reads its
    /// boundaries: a newline ends a statement there unless a `.` continues it.
    pub fn for_engine(engine: Engine, sql: &str) -> Self {
        let statements = match engine.syntax() {
            Syntax::Sql if batches_on(engine) => batches(sql)
                .into_iter()
                .flat_map(|batch| tsql_statements(sql, batch))
                .collect(),
            Syntax::Sql => statements_in(sql),
            Syntax::Mongo => crate::mql::statements(sql),
            Syntax::Redis => crate::redis_cmd::statements(sql),
        };

        Self { statements }
    }

    /// Byte ranges of each statement, in source order, trimmed of surrounding
    /// whitespace. Empty if the buffer holds no statements.
    pub fn statements(&self) -> &[Range<usize>] {
        &self.statements
    }

    /// Where `statement` stands in `sql`, the buffer this was parsed from: the
    /// statement under `cursor` if it is that one, else the first that is.
    /// `None` once it has been edited away.
    ///
    /// A selection is sent verbatim, so `statement` can carry what the
    /// buffer's ranges leave out, like its closing `;`. It is cut the way the
    /// buffer is before the two are compared.
    pub fn find(
        &self,
        engine: Engine,
        sql: &str,
        cursor: usize,
        statement: &str,
    ) -> Option<Range<usize>> {
        let statement = match Buffer::for_engine(engine, statement).statements() {
            [only] => &statement[only.clone()],
            _ => statement,
        };
        let is_it = |range: &Range<usize>| sql[range.clone()].trim() == statement.trim();
        self.statement_at(cursor)
            .filter(is_it)
            .or_else(|| self.statements.iter().find(|range| is_it(range)).cloned())
    }

    /// The statement to run for a cursor at `offset`.
    ///
    /// Inside a statement, that statement. In whitespace or a comment between
    /// two statements, the preceding one — you just finished typing it. Before
    /// the first statement, the first one.
    pub fn statement_at(&self, offset: usize) -> Option<Range<usize>> {
        if self.statements.is_empty() {
            return None;
        }

        if let Some(hit) = self
            .statements
            .iter()
            .find(|range| range.contains(&offset) || range.end == offset)
        {
            return Some(hit.clone());
        }

        self.statements
            .iter()
            .rev()
            .find(|range| range.end < offset)
            .or_else(|| self.statements.first())
            .cloned()
    }
}

/// What a selection of several statements runs as, one submission apiece:
/// every unit in `sql`, or, with a non-empty `selection`, only the ones it
/// touches -- a unit partly selected still counts.
///
/// **The unit is the batch on SQL Server and the statement everywhere else.**
/// A `GO` line is a scope boundary there: a variable, a temp table and a
/// routine's body all end with the batch that declared them, so splitting
/// below one would send `DECLARE @x int` and the `SELECT @x` that reads it as
/// two batches, and the second would not know the first. The other four
/// engines have no such boundary -- a session carries variables, temp tables
/// and an open transaction across every submission -- so there the statement
/// is the unit and a semicolon is where it ends.
///
/// A counted `GO` is not refused here: this answers what the units are, not
/// whether they may run. `batch_counts` is the refusal.
pub(crate) fn queued_statements(
    engine: Engine,
    sql: &str,
    selection: Option<Range<usize>>,
) -> Vec<Range<usize>> {
    let units = match batches_on(engine) {
        true => queued_batches(sql),
        false => Buffer::for_engine(engine, sql).statements().to_vec(),
    };
    let Some(selection) = selection.filter(|sel| !sel.is_empty()) else {
        return units;
    };
    units
        .into_iter()
        .filter(|range| range.start < selection.end && selection.start < range.end)
        .collect()
}

/// How many result sets one submission of `sql` is expected to return: the
/// statements inside it on SQL Server, where the unit submitted is the batch,
/// and one everywhere else, where the unit submitted is the statement.
///
/// An expectation, not a promise. A statement returns at most one set and
/// usually none, so this is an upper bound for the ordinary batch -- but a
/// procedure call, a loop or a trigger can return more sets than the batch has
/// statements, and there is no reading that off the text.
pub(crate) fn expected_sets(engine: Engine, sql: &str) -> usize {
    match batches_on(engine) {
        true => tsql_statements(sql, 0..sql.len()).len().max(1),
        false => 1,
    }
}

/// One range per `GO`-separated batch, trimmed, skipping any that holds no
/// statement -- a batch of nothing but comments is not a batch to send, the
/// same judgement `one_batch` makes.
fn queued_batches(sql: &str) -> Vec<Range<usize>> {
    batches(sql)
        .into_iter()
        .filter(|batch| !tsql_statements(sql, batch.clone()).is_empty())
        .filter_map(|batch| trim_range(sql, batch))
        .collect()
}

/// A SQL Server selection's batches cut into their statements, so a run of
/// reads can send each at once on a connection of its own as every other
/// engine does. Only when every batch is plain reads (`plain_read`) naming no
/// `@variable` and no `#temp` table, which is what a batch scopes: nothing in
/// one needs another statement beside it. Like `queued_statements`, only the
/// statements `selection` touches. Empty for anything else, and on every other
/// engine, whose units are statements already.
pub(crate) fn split_reads(
    engine: Engine,
    sql: &str,
    batches: &[Range<usize>],
    selection: Range<usize>,
) -> Vec<Range<usize>> {
    let reads_alone = |batch: &str| {
        plain_read(engine, batch)
            && !tsql_scan(batch).0.iter().any(|word| {
                let word = &batch[word.clone()];
                word.starts_with('#') || word.starts_with('@') && !word.starts_with("@@")
            })
    };
    if !batches_on(engine) || !batches.iter().all(|batch| reads_alone(&sql[batch.clone()])) {
        return Vec::new();
    }
    batches
        .iter()
        .flat_map(|batch| tsql_statements(sql, batch.clone()))
        .filter_map(|statement| trim_range(sql, statement))
        .filter(|range| range.start < selection.end && selection.start < range.end)
        .collect()
}

/// One key of an `ORDER BY`, as dbdelve reads and writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// The key exactly as it appears in the statement — `"created_at"`, `3`,
    /// `lower(name)`. Kept verbatim, so a key dbdelve did not write survives a
    /// click on some other column.
    pub expression: String,
    pub ascending: bool,
}

impl SortKey {
    pub fn new(expression: impl Into<String>, ascending: bool) -> Self {
        Self {
            expression: expression.into(),
            ascending,
        }
    }

    fn render(&self) -> String {
        let direction = match self.ascending {
            true => "ASC",
            false => "DESC",
        };
        format!("{} {direction}", self.expression)
    }
}

/// The keys of a statement's `ORDER BY`, in order. `Some(empty)` is a statement
/// that could carry one and does not; `None` is a statement dbdelve cannot read
/// well enough to say without guessing.
pub fn order_by(engine: Engine, statement: &str) -> Option<Vec<SortKey>> {
    match engine.syntax() {
        Syntax::Mongo => return crate::mql::browse::order_by(statement),
        // A reply's order is the command's own; nothing sorts it.
        Syntax::Redis => return None,
        Syntax::Sql => {}
    }
    let sql = statement;
    let tree = parse(sql)?;
    let anchor = clause_anchor(&tree, sql)?;
    let Some(clause) = child_of_kind(&anchor, "order_by") else {
        return Some(Vec::new());
    };

    let mut cursor = clause.walk();
    let keys = clause
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "order_target")
        .filter_map(|target| {
            let mut cursor = target.walk();
            let children: Vec<_> = target.named_children(&mut cursor).collect();
            let expression = children
                .iter()
                .find(|node| node.kind() != "direction")
                .and_then(|node| sql.get(node.byte_range()))?;
            let descending = children
                .iter()
                .find(|node| node.kind() == "direction")
                .and_then(|node| sql.get(node.byte_range()))
                .is_some_and(|text| text.trim().eq_ignore_ascii_case("desc"));

            Some(SortKey::new(expression, !descending))
        })
        .collect();

    Some(keys)
}

/// `statement` with `keys` as its `ORDER BY`, replacing the clause it already
/// has and removing it when `keys` is empty.
///
/// The clause is placed where it belongs rather than appended: `ORDER BY` after
/// a `LIMIT` is a syntax error, and a limit that applies *before* the sort
/// would order one arbitrary page of the table instead of the table.
///
/// `None` when dbdelve cannot see where the clause goes — a statement it cannot
/// parse cleanly, one with no `FROM`, or one that is not a query. Nothing is
/// guessed at, because the alternative is handing the server a statement the
/// user did not write and cannot read.
pub fn with_order_by(engine: Engine, statement: &str, keys: &[SortKey]) -> Option<String> {
    match engine.syntax() {
        Syntax::Mongo => return crate::mql::browse::with_order_by(statement, keys),
        Syntax::Redis => return None,
        Syntax::Sql => {}
    }
    let sql = statement;
    let tree = parse(sql)?;
    let anchor = clause_anchor(&tree, sql)?;
    let clause = match keys.is_empty() {
        true => String::new(),
        false => format!(
            "ORDER BY {}",
            keys.iter()
                .map(SortKey::render)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    // Replacing the existing clause, rather than adding a second one, is what
    // makes a repeated click a change of sort instead of an accumulation.
    if let Some(existing) = child_of_kind(&anchor, "order_by") {
        return Some(splice(sql, &tree, existing.byte_range(), &clause));
    }

    if clause.is_empty() {
        return Some(sql.to_string());
    }

    let insert_at = child_of_kind(&anchor, "limit")
        .map(|limit| limit.byte_range().start)
        .unwrap_or(anchor.byte_range().end);

    Some(splice(sql, &tree, insert_at..insert_at, &clause))
}

/// [`sql_paged`] for `engine`. A MongoDB or Redis preview comes back as it
/// is.
pub fn paged(engine: Engine, statement: &str, key: &[String]) -> Option<String> {
    match engine.syntax() {
        Syntax::Sql => sql_paged(engine.spec(), statement, key),
        Syntax::Mongo | Syntax::Redis => Some(statement.to_string()),
    }
}

/// A generated preview's page, spelled the way `spec` reads one.
///
/// Every engine but SQL Server takes the `LIMIT … OFFSET …` a preview is
/// generated with. T-SQL has no `LIMIT`, and the grammar the gates parse with
/// has none of T-SQL's `OFFSET … FETCH`, so the preview is generated, sorted and
/// checked in the one spelling and re-spelled here after the gate has read it.
/// Only the `limit` node the parse tree locates is replaced, with two integers
/// read out of it: nothing the user typed into a filter is touched. `OFFSET`
/// needs an `ORDER BY`, and an unsorted preview is ordered by `key`, the
/// columns that tell its rows apart: without a total order the server may hand
/// the same row to two pages and none to another. The key follows a
/// `(SELECT NULL)` that orders nothing, which is how [`unpaged`] tells it from
/// a sort the user asked for, and is all there is when no key is known. A
/// preview with no limit is one page already, and comes back as it is.
///
/// `None` for a statement this cannot read, or whose limit it cannot.
pub(crate) fn sql_paged(spec: &EngineSpec, statement: &str, key: &[String]) -> Option<String> {
    if spec.sql.paging == Paging::LimitOffset {
        return Some(statement.to_string());
    }
    let tree = parse(statement)?;
    let anchor = clause_anchor(&tree, statement)?;
    let Some(limit) = child_of_kind(&anchor, "limit") else {
        return Some(statement.to_string());
    };
    let number = |node: tree_sitter::Node| -> Option<usize> {
        statement
            .get(child_of_kind(&node, "literal")?.byte_range())?
            .parse()
            .ok()
    };
    let rows = number(limit)?;
    let offset = match child_of_kind(&limit, "offset") {
        Some(offset) => number(offset)?,
        None => 0,
    };
    let order = match child_of_kind(&anchor, "order_by") {
        Some(_) => String::new(),
        None => {
            key.iter()
                .fold("ORDER BY (SELECT NULL)".to_string(), |order, column| {
                    format!("{order}, {}", spec.quote_identifier(column))
                })
                + " "
        }
    };
    Some(splice(
        statement,
        &tree,
        limit.byte_range(),
        &format!("{order}OFFSET {offset} ROWS FETCH NEXT {rows} ROWS ONLY"),
    ))
}

/// [`paged`] undone: the `LIMIT … OFFSET …` spelling back from the
/// `OFFSET … FETCH` one, so the preview that runs can be read by the same
/// parse that wrote it. Anything not in exactly the shape `paged` writes
/// comes back unchanged.
pub fn unpaged(statement: &str) -> String {
    let unpage = || {
        let (head, tail) = statement.rsplit_once(" OFFSET ")?;
        let (offset, tail) = tail.split_once(" ROWS FETCH NEXT ")?;
        let rows = tail.strip_suffix(" ROWS ONLY")?;
        offset.parse::<usize>().ok()?;
        rows.parse::<usize>().ok()?;
        // A sort the user asked for ends in its direction, never in a quoted
        // key column, so one inside a filter's string literal is left alone.
        let head = head
            .rsplit_once(" ORDER BY (SELECT NULL)")
            .filter(|(_, key)| key.is_empty() || (key.starts_with(", ") && key.ends_with('"')))
            .map_or(head, |(head, _)| head);
        Some(format!("{head} LIMIT {rows} OFFSET {offset}"))
    };
    unpage().unwrap_or_else(|| statement.to_string())
}

/// [`sql_update_statement`] for `engine`. MongoDB's and Redis's edits are
/// written by their own modules, and only the tests read the SQL-shaped text
/// spelled with their quoting here.
#[cfg(test)]
pub fn update_row(
    engine: Engine,
    schema: &str,
    table: &str,
    sets: &[(&str, NewValue)],
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Option<String> {
    if engine.syntax() == Syntax::Sql {
        return sql_update_statement(engine.spec(), schema, table, sets, keys, types);
    }
    if sets.is_empty() || keys.is_empty() {
        return None;
    }

    let keys: Vec<(&str, NewValue)> = keys
        .iter()
        .map(|&(column, value)| (column, NewValue::Value(value.into())))
        .collect();
    Some(format!(
        "UPDATE {} SET {} WHERE {}",
        engine.qualified(schema, table),
        assignments(engine, sets, types, ", "),
        assignments(engine, &keys, types, " AND ")
    ))
}

/// An `INSERT` naming exactly the columns it was given, and no others.
///
/// A column the caller does not pass is not mentioned in the statement at all,
/// which is what leaves the server's default to apply to it. That is the whole
/// reason this takes a list of columns rather than a row: a row would have a
/// value for every column, and every default would be unreachable.
///
/// The asymmetry with `update_row` is deliberate and worth stating: this needs
/// a schema and a table but **no primary key**, because an insert has no
/// existing row to name yet, where editing has to name a row that already
/// exists. So a table without a primary key can be inserted into and not
/// edited.
///
/// An error on an empty list. The alternative is `INSERT INTO t DEFAULT
/// VALUES`, a statement nobody has asked dbdelve for.
pub fn insert_row(
    engine: Engine,
    schema: &str,
    table: &str,
    columns: &[(&str, Option<&str>)],
    types: &[(String, String)],
) -> Result<String, String> {
    match engine.syntax() {
        Syntax::Sql => sql_insert_row(engine.spec(), schema, table, columns, types),
        Syntax::Mongo => crate::mql::insert_row(table, columns, types),
        Syntax::Redis => crate::redis_cmd::edit::insert_row(schema, table, columns),
    }
}

/// [`insert_row`] for a SQL engine, written from its spec alone: nothing here
/// asks which engine it is, so an engine with no arm of its own writes through
/// it too. The same holds for every `sql_` writer below.
pub(crate) fn sql_insert_row(
    spec: &EngineSpec,
    schema: &str,
    table: &str,
    columns: &[(&str, Option<&str>)],
    types: &[(String, String)],
) -> Result<String, String> {
    if columns.is_empty() {
        return Err("There is nothing in this row to insert.".into());
    }

    let names: Vec<String> = columns
        .iter()
        .map(|&(column, _)| spec.quote_identifier(column))
        .collect();
    let values: Vec<String> = columns
        .iter()
        // An insert leaves a default to apply by omitting the column outright,
        // so the third state the grid's edits carry has nothing to mean here.
        .map(|&(column, value)| {
            sql_literal(
                spec,
                &value.map_or(NewValue::Null, |value| NewValue::Value(value.into())),
                type_of(types, column),
            )
        })
        .collect();
    Ok(format!(
        "INSERT INTO {} ({}) VALUES ({})",
        spec.qualified(schema, table),
        names.join(", "),
        values.join(", ")
    ))
}

/// One multi-row `INSERT` for an import chunk. `columns` holds `(name,
/// data_type)`, each row its values in that order; a `None` has no SQL
/// spelling, so it is an error. Every value is quoted as the engine quotes it,
/// which is what the write gate's literal check relies on.
pub(crate) fn insert_rows(
    engine: Engine,
    schema: &str,
    table: &str,
    columns: &[(&str, &str)],
    rows: &[Vec<Option<&Value>>],
    skip: bool,
) -> Result<String, String> {
    match engine.syntax() {
        Syntax::Sql => sql_insert_rows(engine.spec(), schema, table, columns, rows, skip),
        Syntax::Mongo => {
            let names: Vec<&str> = columns.iter().map(|&(name, _)| name).collect();
            crate::mql::insert_many(table, &names, rows)
        }
        Syntax::Redis => Err("Redis takes no imported rows.".into()),
    }
}

pub(crate) fn sql_insert_rows(
    spec: &EngineSpec,
    schema: &str,
    table: &str,
    columns: &[(&str, &str)],
    rows: &[Vec<Option<&Value>>],
    skip: bool,
) -> Result<String, String> {
    let Some(&(first, _)) = columns.first() else {
        return Err("There is nothing in this row to insert.".into());
    };
    if rows.is_empty() {
        return Err("There are no rows to insert.".into());
    }
    let names: Vec<String> = columns
        .iter()
        .map(|&(name, _)| spec.quote_identifier(name))
        .collect();
    let mut tuples = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(columns.len());
        for (&(_, data_type), value) in columns.iter().zip(row) {
            let value = value.ok_or("A value is missing from a row to insert.")?;
            let new = match value {
                Value::Null => NewValue::Null,
                Value::Bool(b) => NewValue::Value(spec.sql.boolean_literal.spelled(*b).into()),
                Value::Json(json) => NewValue::Value(json.to_string().into()),
                Value::Text(text) | Value::Number(text) | Value::Date { text, .. } => {
                    NewValue::Value(text.clone().into())
                }
            };
            values.push(sql_literal(spec, &new, Some(data_type)));
        }
        tuples.push(format!("({})", values.join(", ")));
    }
    // ponytail: Stop and Skip only. Update-existing-row is deferred: Postgres cannot spell ON CONFLICT (pk) DO UPDATE under the pinned grammar, MySQL's matches any unique key, SQL Server and Snowflake would need MERGE.
    let tail = match (skip, spec.sql.skip_duplicates) {
        (false, _) => String::new(),
        (true, SkipDuplicates::OnConflictDoNothing { .. }) => " ON CONFLICT DO NOTHING".into(),
        (true, SkipDuplicates::OnDuplicateKey) => {
            let first = spec.quote_identifier(first);
            format!(" ON DUPLICATE KEY UPDATE {first} = {first}")
        }
        (true, SkipDuplicates::None) => {
            return Err("This engine cannot skip rows that already exist.".into());
        }
    };
    Ok(format!(
        "INSERT INTO {} ({}) VALUES {}{tail}",
        spec.qualified(schema, table),
        names.join(", "),
        tuples.join(",\n")
    ))
}

/// An `INSERT` naming every column an insert supplies, each value the engine's
/// own parameter placeholder: a template for the user's code, put on the
/// clipboard and never run. `None` on MongoDB, which has no placeholders, and
/// with no such columns.
pub fn insert_template(
    engine: Engine,
    schema: &str,
    table: &str,
    columns: &[ColumnDefinition],
) -> Option<String> {
    match engine.syntax() {
        Syntax::Sql => sql_insert_template(engine.spec(), schema, table, columns),
        Syntax::Mongo | Syntax::Redis => None,
    }
}

pub(crate) fn sql_insert_template(
    spec: &EngineSpec,
    schema: &str,
    table: &str,
    columns: &[ColumnDefinition],
) -> Option<String> {
    let placeholder: fn(usize) -> String = match spec.sql.params {
        Params::Question => |_| "?".into(),
        Params::Dollar => |n| format!("${n}"),
        Params::AtP => |n| format!("@p{n}"),
    };
    let names: Vec<String> = columns
        .iter()
        .filter(|column| !spec.generates(column))
        .map(|column| spec.quote_identifier(&column.name))
        .collect();
    if names.is_empty() {
        return None;
    }
    let values: Vec<String> = (1..=names.len()).map(placeholder).collect();
    Some(format!(
        "INSERT INTO {} ({}) VALUES ({});",
        spec.qualified(schema, table),
        names.join(", "),
        values.join(", ")
    ))
}

/// One row's `DELETE`: every column in `keys` matched, and nothing else.
///
/// One row per statement. Multi-row deletion is cut, and the upgrade path when
/// it is wanted is the `BEGIN`/`COMMIT` bracketing multi-row edits already use
/// on the engines that commit each statement alone — one `DELETE` per row, each
/// naming its own key, never one statement with a predicate covering several.
///
/// `None` on an empty key list. A `DELETE` with no `WHERE` empties the table, so
/// it must not be possible to produce one: a caller that has lost the row's key
/// gets nothing to run rather than something that runs.
pub fn delete_row(
    engine: Engine,
    schema: &str,
    table: &str,
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Option<String> {
    match engine.syntax() {
        Syntax::Sql => sql_delete_row(engine.spec(), schema, table, keys, types),
        Syntax::Mongo => crate::mql::delete_row(table, keys, types).ok(),
        Syntax::Redis => crate::redis_cmd::edit::delete_row(schema, table, keys, types).ok(),
    }
}

pub(crate) fn sql_delete_row(
    spec: &EngineSpec,
    schema: &str,
    table: &str,
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Option<String> {
    if keys.is_empty() {
        return None;
    }

    let keys: Vec<(&str, NewValue)> = keys
        .iter()
        .map(|&(column, value)| (column, NewValue::Value(value.into())))
        .collect();
    Some(format!(
        "DELETE FROM {} WHERE {}",
        spec.qualified(schema, table),
        sql_assignments(spec, &keys, types, " AND ")
    ))
}

/// Whether `sql` is a statement dbdelve could have written: one or more `UPDATE`s,
/// a single `INSERT` of literals, a single `DELETE` naming one row, or a lone
/// transaction bracket, and nothing else at all.
///
/// The one gate every dbdelve-generated statement passes before anything runs,
/// and the code half of hard rule 1 — dbdelve never writes a `DROP` or a
/// `TRUNCATE`, whatever the user asked for, and writes a `DELETE` only as a
/// conjunction of equalities over distinct, unqualified columns. A whitelist,
/// because a blocklist of keywords is only a list of the spellings someone
/// thought of.
///
/// The delete's shape is read out of the parse tree rather than trusted because
/// `delete_row` produced it. A gate that trusts its caller is a comment, and the
/// day the generator and the check disagree is the day this earns its keep.
/// Whether the columns it names are the row's *key* is `delete_matches_key`'s
/// answer, which this cannot give: no key reaches here to compare against.
///
/// Named for what it admits rather than for one of the shapes, because it
/// admits more than one now: a rule that lets an `INSERT` through under a name
/// promising an `UPDATE` is how a whitelist quietly becomes a list of things
/// nobody refused.
///
/// Reads every literal as a backslash-escaping engine would, the stricter of
/// the two readings. Every caller outside the tests has an engine and goes
/// through [`is_generated_write_on`], which reads it as that engine does.
#[cfg(test)]
pub fn is_generated_write(sql: &str) -> bool {
    generated_write(sql, true)
}

/// `backslash_escapes` is [`Engine::backslash_escapes`] for the engine the
/// statement is bound for.
pub(crate) fn generated_write(sql: &str, backslash_escapes: bool) -> bool {
    // An exact-text whitelist rather than a parse, so nothing can ride along.
    if is_transaction_bracket(sql) {
        return true;
    }
    let Some(tree) = parse_write(sql, backslash_escapes) else {
        return false;
    };
    let root = tree.root_node();
    // Before any shape is considered, because no shape redeems either.
    if forbidden(root) {
        return false;
    }
    let Some(statements) = generated_statements(&root) else {
        return false;
    };

    // Comments are tree-sitter extras and land at the root too, so anything
    // that is not a statement here is something dbdelve did not generate.
    let kinds: Vec<&str> = statements
        .iter()
        .map(|statement| match statement.kind() == "statement" {
            true => statement.named_child(0).map_or("", |node| node.kind()),
            false => "",
        })
        .collect();

    // The one place a `delete` node is tolerated, and only for the shape read
    // back out of the tree rather than trusted because dbdelve wrote it.
    if kinds == ["delete"] {
        return delete_key_columns(sql, backslash_escapes).is_some();
    }

    // One insert alone, or a batch of updates. A batch of inserts is a shape
    // nothing generates, so admitting it would widen the gate for nobody.
    // A comment anywhere in an insert refuses it: MySQL runs `/*! … */` as
    // SQL, and the grammar hides one inside a name or between tokens the shape
    // checks below read only by kind.
    ((kinds == ["insert"] && literal_insert(statements[0], sql) && !commented(root))
        || (!kinds.is_empty() && kinds.iter().all(|kind| *kind == "update")))
        && !deletes_anything(root)
}

/// Whether `sql` is one lone `BEGIN`, `BEGIN TRANSACTION`, `COMMIT` or
/// `ROLLBACK`, spelled as dbdelve writes it: the brackets of an import's
/// transaction, which spans submissions on a connection of the import's own.
/// Compared as text, so a bracket is never admitted beside anything else.
pub(crate) fn is_transaction_bracket(sql: &str) -> bool {
    let sql = sql.trim();
    let sql = sql.strip_suffix(';').unwrap_or(sql).trim();
    matches!(sql, "BEGIN" | "BEGIN TRANSACTION" | "COMMIT" | "ROLLBACK")
}

/// Whether `statement` is one `INSERT` naming its columns, of one or more rows
/// of literals, ending in nothing or in Import Rows' fixed skip clause.
///
/// No subquery, function call, expression or `SELECT` source, because a value
/// that computes something is a value dbdelve did not write. The skip clause is
/// compared as text as well as read from the tree: `ON CONFLICT (a) DO NOTHING`
/// and `ON DUPLICATE KEY UPDATE a = VALUES(a)` are real clauses nothing
/// generates.
fn literal_insert(statement: Node, sql: &str) -> bool {
    // `RETURNING` hangs beside the insert, not under it.
    let Some(insert) = statement
        .named_child(0)
        .filter(|_| statement.named_child_count() == 1)
    else {
        return false;
    };
    let mut cursor = insert.walk();
    let children: Vec<_> = insert.named_children(&mut cursor).collect();
    let [keyword, into, target, columns, values, rest @ ..] = children.as_slice() else {
        return false;
    };
    let kinds = [keyword, into, target, columns, values].map(|node| node.kind());
    if kinds
        != [
            "keyword_insert",
            "keyword_into",
            "object_reference",
            "list",
            "keyword_values",
        ]
        || !plain_columns(*columns, sql)
    {
        return false;
    }

    let rows = rest.iter().take_while(|node| node.kind() == "list").count();
    let (rows, tail) = rest.split_at(rows);
    let null = |node: &Node| {
        node.kind() == "literal"
            && sql
                .get(node.byte_range())
                .is_some_and(|text| text.eq_ignore_ascii_case("NULL"))
    };
    let literal_rows = rows.iter().all(|row| {
        let mut cursor = row.walk();
        let values: Vec<_> = row.named_children(&mut cursor).collect();
        !values.is_empty()
            && values
                .iter()
                .all(|value| literal_value(*value, sql) || null(value))
    });
    let Some(last) = rows.last() else {
        return false;
    };
    let Some(text) = sql.get(last.end_byte()..insert.end_byte()) else {
        return false;
    };
    let text = text.trim();
    let tail_kinds: Vec<_> = tail.iter().map(|node| node.kind()).collect();
    let skip = match tail_kinds.as_slice() {
        [] => text.is_empty(),
        [
            "keyword_on",
            "keyword_conflict",
            "keyword_do",
            "keyword_nothing",
        ] => text == "ON CONFLICT DO NOTHING",
        [
            "keyword_on",
            "keyword_duplicate",
            "keyword_key",
            "keyword_update",
            "assignment_list",
        ] => updates_itself(tail[4], sql)
            .is_some_and(|column| text == format!("ON DUPLICATE KEY UPDATE {column} = {column}")),
        _ => false,
    };
    literal_rows && skip
}

/// A column list whose every entry is a plain or quoted name. A `"…"` name is a
/// `literal` to this grammar, so the quote character is what tells it from a
/// string.
fn plain_columns(list: Node, sql: &str) -> bool {
    let mut cursor = list.walk();
    let columns: Vec<_> = list.named_children(&mut cursor).collect();
    !columns.is_empty()
        && columns.iter().all(|column| {
            let mut cursor = column.walk();
            let named: Vec<_> = column.named_children(&mut cursor).collect();
            column.kind() == "column"
                && matches!(named.as_slice(), [name] if name.kind() == "identifier"
                    || (name.kind() == "literal"
                        && sql.get(name.byte_range()).is_some_and(|text| text.starts_with('"'))))
        })
}

/// The text of `c` when `assignments` is exactly one `c = c`, the same bare or
/// backticked name on both sides, which is MySQL's spelling of doing nothing.
/// Not `"c"`: under MySQL's default `sql_mode` that is a string.
fn updates_itself<'sql>(assignments: Node, sql: &'sql str) -> Option<&'sql str> {
    let assignment = assignments
        .named_child(0)
        .filter(|_| assignments.named_child_count() == 1)?;
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    let text = sql.get(left.byte_range())?;
    (assignment.named_child_count() == 2
        && [left, right]
            .iter()
            .all(|side| side.kind() == "field" && column_name(*side, sql).is_some())
        && sql.get(right.byte_range()) == Some(text))
    .then_some(text)
}

/// Whether `sql` is a `DELETE` whose `WHERE` names exactly `keys` — nothing
/// absent from the key, and nothing in the key absent from the predicate.
///
/// The half of the delete admission `is_generated_write` cannot make alone: it
/// has no key to compare a predicate against. This is not a second gate and
/// admits nothing — it is a readout — and a caller runs both.
///
/// Set equality, order-independent. A composite key matched on half of itself
/// reaches every row sharing that half.
///
/// Reads literals as strictly as [`is_generated_write`] does; callers outside
/// the tests go through [`delete_matches_key_on`].
#[cfg(test)]
pub fn delete_matches_key(sql: &str, keys: &[&str]) -> bool {
    delete_key_matches(sql, keys, true)
}

fn delete_key_matches(sql: &str, keys: &[&str], backslash_escapes: bool) -> bool {
    let Some(columns) = delete_key_columns(sql, backslash_escapes) else {
        return false;
    };
    columns.len() == keys.len() && keys.iter().all(|key| columns.iter().any(|c| c == key))
}

/// [`is_generated_write`] for the engine the statement is bound for. A
/// MongoDB grid writes mongosh statements, so its gate is `mql`'s, which
/// admits the same shapes read out of its own parse, less the transaction
/// bracket and with `insertMany` for the multi-row insert.
pub fn is_generated_write_on(engine: Engine, statement: &str) -> bool {
    match engine.syntax() {
        Syntax::Mongo => crate::mql::is_generated_write(statement),
        Syntax::Redis => crate::redis_cmd::edit::is_generated_write(statement),
        Syntax::Sql => generated_write(statement, engine.backslash_escapes()),
    }
}

/// [`delete_matches_key`] for the engine the statement is bound for.
pub fn delete_matches_key_on(engine: Engine, statement: &str, keys: &[&str]) -> bool {
    match engine.syntax() {
        Syntax::Mongo => crate::mql::delete_matches_key(statement, keys),
        Syntax::Redis => crate::redis_cmd::edit::delete_matches_key(statement, keys),
        Syntax::Sql => delete_key_matches(statement, keys, engine.backslash_escapes()),
    }
}

/// Whether `sql` is a `SELECT` dbdelve could have written: exactly one root
/// statement, a query, with nothing destructive anywhere under it.
///
/// The filter bar is a trust boundary. Everywhere else a statement is either
/// wholly the user's or wholly dbdelve's; a filter is the user's text spliced
/// into dbdelve's statement, so this is what makes `id = 1; DROP TABLE t`
/// structurally impossible rather than merely unlikely. It also guards the
/// filters dbdelve writes for itself.
///
/// Not the second gate `AGENTS.md` rule 2 forbids. That rule governs the one
/// path by which the grid writes, and `is_generated_write` remains its only
/// gate; this guards a path that did not previously admit user text at all,
/// and it admits no write -- a statement reaching it must be a query. Neither
/// is a way around the other, and no generated statement passes through both.
///
/// Exactly one root statement rather than `generated_statements`' view through
/// a transaction: a preview never brackets anything, so seeing through
/// brackets here would only widen what is accepted.
pub fn is_generated_select(engine: Engine, sql: &str) -> bool {
    match engine.syntax() {
        Syntax::Mongo => return crate::mql::browse::is_generated_read(sql),
        Syntax::Redis => return crate::redis_cmd::browse::is_generated_read(sql),
        Syntax::Sql => {}
    }
    let Some(tree) = parse(sql) else {
        return false;
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    // Comments are tree-sitter extras and land at the root too, so anything
    // that is not the one statement is something dbdelve did not generate.
    let children: Vec<_> = root.named_children(&mut cursor).collect();
    let [statement] = children.as_slice() else {
        return false;
    };
    let mut cursor = statement.walk();

    // A `select` among the statement's own children, not its first: `WITH`
    // puts `keyword_with` and the cte ahead of the outer query's select, the
    // same level `select_anchor` reads it back from. A write hides its select
    // inside its own `insert` or `update` node, so none reaches this level.
    statement.kind() == "statement"
        && statement
            .named_children(&mut cursor)
            .any(|node| node.kind() == "select")
        && !forbidden(root)
        && !deletes_anything(root)
}

/// The statements to check, seeing through the transaction that brackets a
/// batch on an engine which does not make one submission atomic by itself.
///
/// The brackets are verified rather than assumed. A `BEGIN` without its
/// `COMMIT` would leave the session in an open transaction, and putting a user
/// in that state without them having written it is exactly what this gate
/// exists to prevent.
fn generated_statements<'tree>(root: &Node<'tree>) -> Option<Vec<Node<'tree>>> {
    let mut cursor = root.walk();
    let children: Vec<_> = root.named_children(&mut cursor).collect();

    let [transaction] = children.as_slice() else {
        return Some(children);
    };
    if transaction.kind() != "transaction" {
        return Some(children);
    }

    let mut cursor = transaction.walk();
    let mut bracketed: Vec<_> = transaction.named_children(&mut cursor).collect();
    // `BEGIN TRANSACTION`, which is how T-SQL has to spell it: the grammar
    // hangs the second word beside the first.
    if bracketed.get(1).map(|node| node.kind()) == Some("keyword_transaction") {
        bracketed.remove(1);
    }
    match bracketed.as_slice() {
        [begin, statements @ .., commit]
            if begin.kind() == "keyword_begin" && commit.kind() == "keyword_commit" =>
        {
            Some(statements.to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
fn assignments(
    engine: Engine,
    columns: &[(&str, NewValue)],
    types: &[(String, String)],
    separator: &str,
) -> String {
    columns
        .iter()
        .map(|(column, value)| {
            format!(
                "{} = {}",
                engine.quote_identifier(column),
                literal(engine, value, type_of(types, column))
            )
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn sql_assignments(
    spec: &EngineSpec,
    columns: &[(&str, NewValue)],
    types: &[(String, String)],
    separator: &str,
) -> String {
    columns
        .iter()
        .map(|(column, value)| {
            format!(
                "{} = {}",
                spec.quote_identifier(column),
                sql_literal(spec, value, type_of(types, column))
            )
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn type_of<'a>(types: &'a [(String, String)], column: &str) -> Option<&'a str> {
    types
        .iter()
        .find(|(name, _)| name == column)
        .map(|(_, data_type)| data_type.as_str())
}

#[cfg(test)]
fn literal(engine: Engine, value: &NewValue, data_type: Option<&str>) -> String {
    match value {
        NewValue::Value(value) => engine.quote_value(value, data_type),
        NewValue::Null => "NULL".to_string(),
        NewValue::Default => "DEFAULT".to_string(),
    }
}

/// A value as it goes into a statement: quoted, or one of the two keywords that
/// stand for there being no value to quote. Unquoted is the only way to write
/// either — `'NULL'` and `'DEFAULT'` are the words, and a user who typed one of
/// them into a cell meant the word.
fn sql_literal(spec: &EngineSpec, value: &NewValue, data_type: Option<&str>) -> String {
    match value {
        NewValue::Value(value) => spec.quote_value(value, data_type),
        NewValue::Null => "NULL".to_string(),
        NewValue::Default => "DEFAULT".to_string(),
    }
}

/// `DROP` and `TRUNCATE`, anywhere in the tree and under every spelling. Never
/// admitted, by any shape, for any reason.
///
/// The grammar offers no `drop` or `truncate` node to look for. `DROP TABLE` is
/// `drop_table`, one of thirteen `drop_*` siblings, and `TRUNCATE t` is a bare
/// `statement` holding a `keyword_truncate` with no wrapper node at all. The
/// keyword is the one part every spelling of either has.
fn forbidden(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    matches!(node.kind(), "keyword_drop" | "keyword_truncate")
        || node.children(&mut cursor).any(forbidden)
}

/// Any `delete` at all, anywhere in the tree, not only at the root.
/// `WITH x AS (DELETE FROM t RETURNING *) UPDATE …` is a real statement shape
/// whose root child is an `update` node, so the whitelist alone would let it
/// through.
fn deletes_anything(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    matches!(node.kind(), "delete" | "keyword_delete")
        || node.children(&mut cursor).any(deletes_anything)
}

/// The columns a single-row `DELETE`'s `WHERE` names, read out of the parse
/// tree, or `None` for anything that is not exactly that shape.
///
/// Exactly one root statement whose named children are `["delete", "from"]` —
/// which is where the grammar puts them, with the `where` under the `from` —
/// and whose `WHERE` is a conjunction of equality predicates over distinct,
/// unqualified columns against single-quoted literals. A CTE beside the delete,
/// a `RETURNING`, a `LIMIT`, an `OR`, a subquery, a function call, a qualified
/// column or a second statement all change that child list or that expression
/// tree, and so all arrive here as `None`.
fn delete_key_columns(sql: &str, backslash_escapes: bool) -> Option<Vec<String>> {
    let tree = parse_write(sql, backslash_escapes)?;
    let root = tree.root_node();
    if forbidden(root) {
        return None;
    }

    let mut cursor = root.walk();
    let children: Vec<_> = root.named_children(&mut cursor).collect();
    let [statement] = children.as_slice() else {
        return None;
    };
    if statement.kind() != "statement" {
        return None;
    }

    let mut cursor = statement.walk();
    let parts: Vec<_> = statement.named_children(&mut cursor).collect();
    let [delete, from] = parts.as_slice() else {
        return None;
    };
    if delete.kind() != "delete" || from.kind() != "from" {
        return None;
    }

    let mut cursor = from.walk();
    let inside: Vec<_> = from.named_children(&mut cursor).collect();
    let [keyword, relation, filter] = inside.as_slice() else {
        return None;
    };
    if keyword.kind() != "keyword_from"
        || relation.kind() != "object_reference"
        || filter.kind() != "where"
    {
        return None;
    }

    let mut cursor = filter.walk();
    let clause: Vec<_> = filter.named_children(&mut cursor).collect();
    let [keyword_where, predicate] = clause.as_slice() else {
        return None;
    };
    if keyword_where.kind() != "keyword_where" {
        return None;
    }

    let mut columns = Vec::new();
    if !equality_columns(*predicate, sql, &mut columns) {
        return None;
    }

    // A column named twice is a predicate dbdelve never writes, and reading it as
    // a one-column key would call a half-matched composite key a whole one.
    let distinct = columns.iter().collect::<std::collections::HashSet<_>>();
    (distinct.len() == columns.len()).then_some(columns)
}

/// Walks a conjunction, pushing the column each `=` predicate names. False the
/// moment anything else appears — an `OR`, another operator, a parenthesized
/// group, a subquery, a function call.
fn equality_columns(node: tree_sitter::Node, sql: &str, columns: &mut Vec<String>) -> bool {
    if node.kind() != "binary_expression" {
        return false;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    let [left, operator, right] = children.as_slice() else {
        return false;
    };

    match operator.kind() {
        "keyword_and" => {
            equality_columns(*left, sql, columns) && equality_columns(*right, sql, columns)
        }
        "=" => {
            if !literal_value(*right, sql) {
                return false;
            }
            match column_name(*left, sql) {
                Some(column) => {
                    columns.push(column);
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// A single-quoted literal and nothing else. `"other"` is a `literal` to this
/// grammar too, and matching a column against a column is not naming a row.
/// `N'…'` is SQL Server's single-quoted literal, and `0x…` its binary one,
/// which is never quoted.
///
/// Exactly one token: the grammar reads `'1' '2'` as one `literal`, and every
/// engine concatenates adjacent strings, so a quote inside must be doubled.
///
/// Not `NULL`: `WHERE id = NULL` matches no row, so only an insert admits it,
/// and beside this rather than through it.
fn literal_value(node: Node, sql: &str) -> bool {
    let Some(value) = sql.get(node.byte_range()) else {
        return false;
    };
    let quoted = value
        .strip_prefix(['N', 'n'])
        .unwrap_or(value)
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .is_some_and(|inner| inner.replace("''", "").find('\'').is_none());
    let hex = value.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    node.kind() == "literal" && (quoted || hex)
}

fn commented(node: Node) -> bool {
    let mut cursor = node.walk();
    matches!(node.kind(), "comment" | "marginalia") || node.children(&mut cursor).any(commented)
}

/// [`parse`] for the write gate, refusing on an engine where a backslash
/// escapes any literal holding a quote after an odd run of backslashes.
///
/// The grammar ends a string at the first lone quote whatever precedes it; the
/// engine reads `\'` as part of the string. Everything between would be string
/// to the gate and SQL to the engine, in any arm, so the check is on every
/// literal in the tree rather than on the values one arm reads. Either quote
/// character, because MySQL reads `"…"` as a string with the same escapes, and
/// `quote_literal` doubles every backslash on these engines, so nothing it
/// writes has an odd run.
fn parse_write(sql: &str, backslash_escapes: bool) -> Option<Tree> {
    let tree = parse(sql)?;
    (!(backslash_escapes && misread_literal(tree.root_node(), sql))).then_some(tree)
}

fn misread_literal(node: Node, sql: &str) -> bool {
    let escaped_quote = |text: &str| {
        let bytes = text.as_bytes();
        bytes.iter().enumerate().any(|(at, byte)| {
            matches!(byte, b'\'' | b'"')
                && bytes[..at]
                    .iter()
                    .rev()
                    .take_while(|&&b| b == b'\\')
                    .count()
                    % 2
                    == 1
        })
    };
    let mut cursor = node.walk();
    (node.kind() == "literal" && sql.get(node.byte_range()).is_some_and(escaped_quote))
        || node
            .children(&mut cursor)
            .any(|child| misread_literal(child, sql))
}

/// The unqualified column a predicate's left side names, unquoted.
///
/// To this grammar a double quote opens a **string**: a bare or backticked name
/// arrives as a `field`, but `"id"` arrives as a `literal` indistinguishable by
/// kind from `'id'`, so the quote character is what tells them apart. That
/// matters because Postgres and SQLite quote identifiers with `"`, which is
/// what `delete_row` writes on both.
fn column_name(node: tree_sitter::Node, sql: &str) -> Option<String> {
    let text = sql.get(node.byte_range())?;
    match node.kind() {
        // `t.id` puts an `object_reference` under the field beside the
        // identifier. A qualified column is not one this reads.
        "field" => {
            let mut cursor = node.walk();
            let named: Vec<_> = node.named_children(&mut cursor).collect();
            let [identifier] = named.as_slice() else {
                return None;
            };
            (identifier.kind() == "identifier").then(|| unquote(text, '`'))
        }
        "literal" if text.starts_with('"') => Some(unquote(text, '"')),
        _ => None,
    }
}

fn unquote(text: &str, quote: char) -> String {
    let doubled = [quote, quote].iter().collect::<String>();
    match text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
        Some(inner) => inner.replace(&doubled, &quote.to_string()),
        None => text.to_string(),
    }
}

fn parse(sql: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(sql, None)?;
    // A statement the grammar could not read whole is a statement whose clause
    // boundaries are unknown, and splicing against a guess would corrupt SQL
    // the user wrote. `NULLS FIRST` and `FOR UPDATE` land here today.
    (!tree.root_node().has_error()).then_some(tree)
}

/// The node whose children carry `ORDER BY` and `LIMIT`: the query's outermost
/// `FROM`. A subquery's own clauses hang under its `subquery` node instead, so
/// looking only at this node's children cannot reach into one by accident.
fn clause_anchor<'tree>(tree: &'tree Tree, sql: &str) -> Option<tree_sitter::Node<'tree>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let statements: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|node| STATEMENT_KINDS.contains(&node.kind()))
        .collect();
    // One statement, or there is no telling which one the rows came from.
    let [statement] = statements[..] else {
        return None;
    };
    // `BEGIN; …; COMMIT` and `DO $$…$$` are runnable but not queries.
    if statement.kind() != "statement" || sql.get(statement.byte_range()).is_none() {
        return None;
    }

    let mut cursor = statement.walk();
    let children: Vec<_> = statement.named_children(&mut cursor).collect();
    // The grammar hangs an `EXPLAIN`'s payload off the same statement node, so
    // the `select` and `from` below belong to the explained query rather than to
    // anything the result describes. Its rows are a plan -- one text column,
    // whose order is the tree's shape -- so there is nothing to sort by, and a
    // spliced `ORDER BY` would silently re-plan a different query than the one
    // the user asked about.
    if children.iter().any(|node| node.kind() == "keyword_explain") {
        return None;
    }
    // A `UNION` puts the whole query's `ORDER BY` after its last branch, so its
    // clauses hang under the set operation rather than the statement.
    if let Some(set_operation) = children.iter().find(|node| node.kind() == "set_operation") {
        let mut cursor = set_operation.walk();
        let branches: Vec<_> = set_operation.named_children(&mut cursor).collect();
        return select_anchor(&branches);
    }

    select_anchor(&children)
}

/// The `from` of a query, and only of a query.
///
/// The grammar gives `DELETE FROM t` the same `from` child a `SELECT` has, so a
/// `from` alone is not evidence that a sort belongs here — and writing one into
/// a `DELETE` is what hard rule 1 forbids outright. A `select` beside it is the
/// evidence. `WITH` leaves the outer query's `select` and `from` at this level
/// too, beside the cte, so a CTE still sorts.
fn select_anchor<'tree>(children: &[tree_sitter::Node<'tree>]) -> Option<tree_sitter::Node<'tree>> {
    if !children.iter().any(|node| node.kind() == "select") {
        return None;
    }

    children.iter().rfind(|node| node.kind() == "from").copied()
}

fn child_of_kind<'tree>(
    node: &tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// `sql` with `range` replaced by `clause`, tidying only the seam.
///
/// Only the whitespace either side of the splice point is touched. Collapsing
/// runs of spaces across the whole statement instead would rewrite string
/// literals, quoted identifiers and the user's indentation — a silent edit to
/// what the statement means, which is the one thing this module must not do.
///
/// The seam is a newline when what comes before it ends in a `--` comment,
/// which would otherwise run on over the clause and everything after it. The
/// tree says whether it is one: a `--` inside a string is not, and a newline
/// there would move the `ORDER BY (SELECT NULL)` that [`unpaged`] reads back
/// after a space.
fn splice(sql: &str, tree: &Tree, range: Range<usize>, clause: &str) -> String {
    let head = sql[..range.start].trim_end();
    let tail = sql[range.end..].trim_start();
    let after_comment = head
        .len()
        .checked_sub(1)
        .and_then(|last| tree.root_node().descendant_for_byte_range(last, head.len()))
        .is_some_and(|node| node.kind() == "comment");

    let mut spliced = String::with_capacity(head.len() + clause.len() + tail.len() + 2);
    spliced.push_str(head);
    for part in [clause, tail] {
        if part.is_empty() {
            continue;
        }
        if !spliced.is_empty() {
            let seam = match after_comment && spliced.len() == head.len() {
                true => '\n',
                false => ' ',
            };
            spliced.push(seam);
        }
        spliced.push_str(part);
    }
    spliced
}

/// The grammar declares exactly these three as the root's statement children.
/// Filtering on them is not optional: comments are tree-sitter *extras*, so
/// `comment` and `marginalia` also land at the root, and sending one of those
/// to the server returns an empty response the user cannot explain.
const STATEMENT_KINDS: [&str; 3] = ["statement", "block", "transaction"];

fn collect_statements(tree: &Tree, sql: &str) -> Vec<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut statements: Vec<Range<usize>> = Vec::new();
    // Whether a `;` has closed the statement before this node. Read off the
    // tree's own `;` tokens rather than off the text between nodes, where one
    // inside a comment would look the same and is not a separator.
    let mut separated = true;
    // Whether the last statement began as text the grammar could not read and
    // has not reached its `;` yet, so what follows is still part of it.
    let mut unread = false;

    for node in root.children(&mut cursor) {
        if node.kind() == ";" {
            separated = true;
            unread = false;
            continue;
        }

        // Whatever the grammar could not read lands in a sibling ERROR node.
        if node.is_error() {
            // It can swallow the `;` that ends it -- and whole statements
            // after that, readable ones included -- so it is cut at its own
            // separators before anything is made of it.
            let (pieces, closed) = unread_pieces(sql, node.byte_range());
            let mut opened = false;
            for (index, piece) in pieces.into_iter().enumerate() {
                // After a statement with no `;` between, the first piece is
                // that statement's tail. Dropping it would send the head alone,
                // and the head of a half-typed `DELETE … WHERE` is an
                // unqualified DELETE. The tail was typed into this statement,
                // so it goes to the server with it and the server is what
                // explains the problem.
                if index == 0
                    && !separated
                    && let Some(last) = statements.last_mut()
                {
                    last.end = piece.end;
                    continue;
                }
                // Otherwise it is a statement of its own. The grammar is one
                // dialect's worth of SQL and the servers speak four: `PRAGMA`,
                // `CALL`, `LISTEN` and `USE` are all statements it has never
                // heard of, and a buffer holding only one of them used to hold
                // "no statement to run". Whether it is valid is the server's to
                // say (hard rule 1 cuts both ways: not rewritten, and not
                // withheld either).
                statements.push(piece);
                opened = true;
            }
            separated = closed;
            unread = (unread || opened) && !closed;
            continue;
        }

        if !STATEMENT_KINDS.contains(&node.kind()) {
            continue;
        }

        // The rest of a statement whose opening the grammar could not read:
        // `GRANT SELECT ON t TO r` parses as an unread `GRANT` and then a
        // `SELECT ON t`, and sending the second without the first runs a
        // statement nobody wrote.
        if unread
            && !separated
            && let Some(last) = statements.last_mut()
        {
            if let Some(merged) = trim_range(sql, last.start..node.byte_range().end) {
                *last = merged;
            }
            continue;
        }

        if let Some(range) = trim_range(sql, node.byte_range()) {
            statements.push(range);
            separated = false;
            unread = false;
        }
    }

    statements
}

/// Text the grammar could not read, cut at the `;`s that separate statements
/// in it, and whether the last piece was closed by one.
///
/// A scan rather than a split, because the text is unparsed by definition and
/// a `;` inside a string, a quoted name, a comment or a dollar-quoted body
/// separates nothing: cutting a function body at its first `;` would send the
/// server half a `CREATE FUNCTION`.
///
/// ponytail: standard SQL only. MySQL's `#` line comments and backslash
/// escapes are not read, so a `;` inside one of those splits a statement the
/// server then rejects. Reading them would break Postgres, where `#` is an
/// operator and `\` is not an escape -- the fix is a dialect here, not a
/// bigger scanner, and it can wait for someone who hits it.
fn unread_pieces(sql: &str, range: Range<usize>) -> (Vec<Range<usize>>, bool) {
    let text = sql.get(range.clone()).unwrap_or_default();
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut closed = false;

    while index < bytes.len() {
        let rest = &text[index..];
        index += match bytes[index] {
            b';' => {
                pieces.extend(trim_range(sql, range.start + start..range.start + index));
                start = index + 1;
                1
            }
            quote @ (b'\'' | b'"' | b'`') => {
                // A doubled quote is an escaped one, and reads here as one
                // string ending and another beginning, which comes to the same.
                1 + rest[1..]
                    .find(quote as char)
                    .map_or(rest.len() - 1, |end| end + 1)
            }
            b'-' if rest.starts_with("--") => rest.find('\n').unwrap_or(rest.len()),
            // From past the `/*`, or `/*/` would read as a whole comment.
            b'/' if rest.starts_with("/*") => {
                rest[2..].find("*/").map_or(rest.len(), |end| end + 4)
            }
            b'$' => {
                // `$$` or `$tag$`, closed by the same marker.
                let tag = rest[1..]
                    .find('$')
                    .filter(|end| {
                        rest[1..1 + end]
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_')
                    })
                    .map(|end| &rest[..end + 2]);
                match tag {
                    Some(tag) => rest[tag.len()..]
                        .find(tag)
                        .map_or(rest.len(), |end| end + 2 * tag.len()),
                    None => 1,
                }
            }
            // Byte-wise scan, char-wise slicing: advancing one byte past a
            // multi-byte character would leave `index` mid-character and the
            // next `&text[index..]` panics.
            _ => rest.chars().next().map_or(1, char::len_utf8),
        };
    }

    match trim_range(sql, range.start + start..range.end) {
        Some(tail) => pieces.push(tail),
        None => closed = !pieces.is_empty() || text.trim_end().ends_with(';'),
    }
    (pieces, closed)
}

fn trim_range(sql: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = sql.get(range.clone())?;
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    let trimmed = (range.start + leading)..(range.end - trailing);
    (!trimmed.is_empty()).then_some(trimmed)
}

fn statements_in(sql: &str) -> Vec<Range<usize>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()
        .and_then(|_| parser.parse(sql, None))
        .map(|tree| collect_statements(&tree, sql))
        .unwrap_or_default()
}

/// The bare words of T-SQL text, and where its `[bracketed]` names are: what is
/// left once strings, quoted names and comments are stepped over, so a `GO`, a
/// `BEGIN` or an `END` inside one of those is not read as a keyword.
///
/// ponytail: block comments are read flat. T-SQL nests them, so an `END`
/// after the inner `*/` of `/* /* */ END */` counts; nobody has written one.
fn tsql_scan(text: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    let (mut words, mut brackets) = (Vec::new(), Vec::new());
    let mut index = 0;
    while let Some(c) = text[index..].chars().next() {
        let rest = &text[index..];
        index += match c {
            // A doubled quote reads as one string ending and another beginning.
            '\'' | '"' => 1 + rest[1..].find(c).map_or(rest.len() - 1, |end| end + 1),
            '[' => {
                // `]]` is an escaped `]`, not the end of the name.
                let mut end = 1;
                loop {
                    match rest[end..].find(']') {
                        Some(found) if rest[end + found + 1..].starts_with(']') => end += found + 2,
                        Some(found) => break end += found + 1,
                        None => break end = rest.len(),
                    }
                }
                brackets.push(index..index + end);
                end
            }
            '-' if rest.starts_with("--") => rest.find('\n').unwrap_or(rest.len()),
            '/' if rest.starts_with("/*") => rest[2..].find("*/").map_or(rest.len(), |end| end + 4),
            c if is_word(c) => {
                let len = rest.find(|c| !is_word(c)).unwrap_or(rest.len());
                words.push(index..index + len);
                len
            }
            c => c.len_utf8(),
        };
    }
    (words, brackets)
}

/// SQL Server's `GO` lines: the word alone on its line, give or take a repeat
/// count and a trailing comment. `GO` is sqlcmd's and SSMS's batch separator,
/// never T-SQL, and the server reads one sent to it as a column alias.
fn go_lines(sql: &str) -> Vec<(Range<usize>, Option<u64>)> {
    let (words, _) = tsql_scan(sql);
    words
        .into_iter()
        .filter(|word| sql[word.clone()].eq_ignore_ascii_case("go"))
        .filter_map(|word| {
            let start = sql[..word.start].rfind('\n').map_or(0, |at| at + 1);
            let end = sql[word.end..]
                .find('\n')
                .map_or(sql.len(), |at| word.end + at);
            if !sql[start..word.start].trim().is_empty() {
                return None;
            }
            let tail = sql[word.end..end].trim_start();
            let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            let rest = tail[digits..].trim();
            if !(rest.is_empty() || rest.starts_with("--")) {
                return None;
            }
            // A count too big to read is still a count, never a plain `GO`.
            let count = (digits > 0).then(|| tail[..digits].parse().unwrap_or(u64::MAX));
            Some((start..end, count))
        })
        .collect()
}

/// Whether `engine` reads a buffer as `GO`-separated batches.
fn batches_on(engine: Engine) -> bool {
    engine.syntax() == Syntax::Sql && engine.spec().sql.statement_split == StatementSplit::Batches
}

/// The text between `GO` lines.
fn batches(sql: &str) -> Vec<Range<usize>> {
    let mut batches = Vec::new();
    let mut start = 0;
    for (line, _) in go_lines(sql) {
        batches.push(start..line.start);
        start = line.end;
    }
    batches.push(start..sql.len());
    batches
}

/// One batch's statements. The grammar finds the boundaries, reading each
/// `[name]` as a `"name"` of the same length so a `;` or a `'` inside one
/// separates nothing and every offset still points into `sql`. Then what T-SQL
/// holds together is put back together: a routine's body to the end of the
/// batch, and a `BEGIN … END` block (`CASE … END` counted too, as it closes on
/// the same word) whole.
fn tsql_statements(sql: &str, batch: Range<usize>) -> Vec<Range<usize>> {
    let text = &sql[batch.clone()];
    let (words, brackets) = tsql_scan(text);
    let mut masked = text.as_bytes().to_vec();
    for bracket in brackets {
        masked[bracket.clone()].fill(b'_');
        masked[bracket.start] = b'"';
        if text[bracket.clone()].ends_with(']') {
            masked[bracket.end - 1] = b'"';
        }
    }
    // Every byte of a bracket is overwritten, continuation bytes included, so
    // what is left is still UTF-8.
    let masked = String::from_utf8(masked).unwrap_or_else(|_| text.to_string());

    let word = |index: usize| {
        words
            .get(index)
            .map(|range| text[range.clone()].to_ascii_uppercase())
    };
    let mut depth = 0usize;
    let mut depths = Vec::with_capacity(words.len());
    for (index, range) in words.iter().enumerate() {
        match (word(index).as_deref(), word(index + 1).as_deref()) {
            (
                Some("BEGIN"),
                Some("TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION"),
            ) => {}
            (Some("END"), Some("CONVERSATION")) => {}
            (Some("BEGIN" | "CASE"), _) => depth += 1,
            (Some("END"), _) => depth = depth.saturating_sub(1),
            _ => {}
        }
        depths.push((range.end, depth));
    }
    let depth_at = |offset: usize| match depths.partition_point(|(end, _)| *end <= offset) {
        0 => 0,
        after => depths[after - 1].1,
    };
    let routine = |start: usize| {
        let first = words.partition_point(|range| range.start < start);
        let mut opening = (first..first + 4).filter_map(word);
        let object = match (opening.next().as_deref(), opening.next().as_deref()) {
            (Some("CREATE"), Some("OR")) => opening.nth(1),
            (Some("CREATE" | "ALTER"), object) => object.map(str::to_string),
            _ => None,
        };
        matches!(
            object.as_deref(),
            Some("PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER")
        )
    };

    let mut statements: Vec<Range<usize>> = Vec::new();
    for statement in statements_in(&masked) {
        match statements.last_mut() {
            Some(last) if routine(last.start) || depth_at(last.end) > 0 => last.end = statement.end,
            _ => statements.push(statement),
        }
    }
    statements
        .into_iter()
        .map(|range| batch.start + range.start..batch.start + range.end)
        .collect()
}

/// What of `sql` to send to SQL Server: the one batch it holds, without the
/// `GO` lines around it. More than one batch, or a batch asked to run more than
/// once, is refused rather than sent as text the server cannot read.
pub(crate) fn one_batch(engine: Engine, sql: &str) -> Result<&str, String> {
    if !batches_on(engine) {
        return Ok(sql);
    }
    batch_counts(engine, sql)?;
    // A batch of nothing but comments is not a second batch.
    let mut batches = batches(sql)
        .into_iter()
        .filter(|batch| !tsql_statements(sql, batch.clone()).is_empty())
        .filter_map(|batch| trim_range(sql, batch));
    match (batches.next(), batches.next()) {
        (_, Some(_)) => Err("The selection holds more than one batch separated by GO, \
             and dbdelve sends one batch at a time."
            .into()),
        (batch, None) => Ok(batch.map_or("", |batch| &sql[batch])),
    }
}

/// Refused when any `GO` in `sql` carries a count. The count repeats its
/// batch, and running it once is not what the buffer says; a selection run a
/// batch at a time has to refuse it for the reason a single one does.
pub(crate) fn batch_counts(engine: Engine, sql: &str) -> Result<(), String> {
    if !batches_on(engine) {
        return Ok(());
    }
    for (_, count) in go_lines(sql) {
        count.map_or(Ok(()), repeats)?;
    }
    Ok(())
}

/// Refused when the `GO` that ends the batch holding `offset` carries a count:
/// running the statement under the cursor once would quietly drop it.
pub(crate) fn batch_repeats(engine: Engine, sql: &str, offset: usize) -> Result<(), String> {
    if !batches_on(engine) {
        return Ok(());
    }
    match go_lines(sql)
        .into_iter()
        .find(|(line, _)| line.start >= offset)
    {
        Some((_, Some(count))) => repeats(count),
        _ => Ok(()),
    }
}

fn repeats(count: u64) -> Result<(), String> {
    match count {
        1 => Ok(()),
        _ => Err(format!(
            "GO {count} repeats its batch, and dbdelve runs a batch once."
        )),
    }
}

/// Every pending row as one `UPDATE`, joined into a single string.
///
/// All-or-nothing, which each engine reaches differently, and
/// `SqlSpec::transaction_opening` is where that per-engine answer lives. Postgres
/// runs one submission as a single implicit transaction and needs nothing;
/// MySQL and SQLite commit every statement on its own, so a batch of more than
/// one is bracketed — in the statement text itself, where the user can read,
/// edit and undo it, because dbdelve does not open a transaction behind anyone's
/// back.
///
/// An error when there is nothing to apply, and an error — rather than a
/// shorter batch — when any one row cannot be written: a partial apply is not
/// the change the user made, and dbdelve would have no way to say which part of
/// it ran.
pub(crate) fn update_batch(engine: Engine, rows: &[PendingRow]) -> Result<String, String> {
    match engine.syntax() {
        Syntax::Sql => sql_update_batch(engine.spec(), rows),
        Syntax::Mongo => crate::mql::update_batch(rows),
        Syntax::Redis => crate::redis_cmd::edit::update_batch(rows),
    }
}

pub(crate) fn sql_update_batch(spec: &EngineSpec, rows: &[PendingRow]) -> Result<String, String> {
    if rows.is_empty() {
        return Err("There are no edits to apply.".into());
    }

    fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
        pairs
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_str()))
            .collect()
    }
    fn borrowed_sets(pairs: &[(String, NewValue)]) -> Vec<(&str, NewValue)> {
        pairs
            .iter()
            .map(|(column, value)| (column.as_str(), value.clone()))
            .collect()
    }
    let statements: Option<Vec<String>> = rows
        .iter()
        .map(|row| {
            sql_update_statement(
                spec,
                &row.schema,
                &row.table,
                &borrowed_sets(&row.sets),
                &borrowed(&row.keys),
                &row.types,
            )
            // Terminated, not separated: the last statement carries its
            // semicolon too, so appending to a buffer cannot fuse it onto
            // whatever the user writes next.
            .map(|statement| format!("{statement};"))
        })
        .collect();

    let batch = statements
        .ok_or("dbdelve cannot name an edited row by its primary key.")?
        .join("\n");
    let bracket = spec.sql.transaction_opening().filter(|_| rows.len() > 1);
    Ok(match bracket {
        Some(start) => format!("{start};\n{batch}\nCOMMIT;"),
        None => batch,
    })
}

/// One row's `UPDATE`: every column in `sets` assigned, every column in `keys`
/// matched.
///
/// Values go in as literals and are never cast. Postgres applies the target
/// column's assignment cast, so `'123'` lands in an `int4` exactly as `123`
/// would, and SQLite applies the column's type affinity to the same effect. A
/// cast dbdelve chose for itself could only ever be the wrong one. A cleared cell
/// is therefore the empty string, and [`NewValue`]'s other two arms are the
/// keywords: three different writes, which is the whole point of spelling two
/// of them as something other than a value.
///
/// `keys` carries plain values, because a row identified by a `NULL` is a row
/// `=` does not find; the caller drops such a row before it gets here.
///
/// `types` names each column's type where it is known, for the engine whose
/// literal depends on it (`EngineSpec::quote_value`). A column missing from it is
/// quoted the way any value is.
///
/// `None` when either list is empty. A statement with no `WHERE` rewrites every
/// row in the table and one with no `SET` is not a statement at all, so a caller
/// that has lost the row's key gets nothing to run rather than something that
/// runs.
fn sql_update_statement(
    spec: &EngineSpec,
    schema: &str,
    table: &str,
    sets: &[(&str, NewValue)],
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Option<String> {
    if sets.is_empty() || keys.is_empty() {
        return None;
    }

    let keys: Vec<(&str, NewValue)> = keys
        .iter()
        .map(|&(column, value)| (column, NewValue::Value(value.into())))
        .collect();
    Some(format!(
        "UPDATE {} SET {} WHERE {}",
        spec.qualified(schema, table),
        sql_assignments(spec, sets, types, ", "),
        sql_assignments(spec, &keys, types, " AND ")
    ))
}

/// A statement at the front of the history, and there only once however many
/// times it has been run: a query run five times is one row to recall, not five
/// rows to read past.
pub(crate) fn remember_statement(history: &mut Vec<String>, sql: &str) {
    history.retain(|past| past != sql);
    history.insert(0, sql.to_string());
    history.truncate(crate::store::HISTORY_DEPTH);
}

/// dbdelve's statement appended to the buffer the user is writing in.
///
/// The terminator is the whole subtlety: an unterminated statement with an
/// `UPDATE` appended to it becomes one statement, and the next `cmd+enter`
/// would send both as one. dbdelve is writing here because the user asked it to,
/// so the boundary of what they wrote has to survive the ask.
pub(crate) fn appended_statement(engine: Engine, buffer: &str, statement: &str) -> String {
    let text = buffer.trim_end();
    if text.is_empty() {
        return statement.to_string();
    }
    // A Redis command ends with its line, and a `;` would join its last word.
    if engine.syntax() == Syntax::Redis {
        return format!("{text}\n\n{statement}");
    }

    format!("{}\n\n{statement}", terminated(text))
}

fn terminated(text: &str) -> String {
    let mut parser = Parser::new();
    let end = parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()
        .and_then(|_| parser.parse(text, None))
        .and_then(|tree| {
            let root = tree.root_node();
            let mut cursor = root.walk();
            root.children(&mut cursor)
                .filter(|node| !matches!(node.kind(), "comment" | "marginalia"))
                .last()
                .map(|node| node.end_byte())
        });
    match end {
        Some(end) if !text[..end].ends_with(';') => format!("{};{}", &text[..end], &text[end..]),
        _ => text.to_string(),
    }
}

/// What a connection is allowed to do, and what a statement needs in order to
/// run. One enum for both, because they are the same three-rung ladder and two
/// types would be the same three values under different names.
///
/// **Variant order is load-bearing**: `Ord` derives from it, and the whole mode
/// check is `required <= allowed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    ReadOnly,
    /// What a profile written before modes existed reads back as -- which is
    /// what it has always been connecting as.
    #[default]
    ReadWrite,
    Full,
}

impl Mode {
    pub(crate) const ALL: [Mode; 3] = [Mode::ReadOnly, Mode::ReadWrite, Mode::Full];

    pub(crate) fn next(self) -> Mode {
        match self {
            Mode::ReadOnly => Mode::ReadWrite,
            Mode::ReadWrite => Mode::Full,
            Mode::Full => Mode::ReadOnly,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Mode::ReadOnly => "Read-only",
            Mode::ReadWrite => "Read-write",
            Mode::Full => "Full",
        }
    }

    /// How a mode is written to `profiles.toml`: a name rather than a number,
    /// so the file stays readable and a build that drops a mode still reads
    /// something it can name in a message.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::ReadWrite => "read-write",
            Mode::Full => "full",
        }
    }

    /// `None` for a slug this build does not have. The caller decides what to do
    /// with that -- `restore_profile` reads it as Read-only rather than refusing
    /// the profile, let alone the file it came in.
    pub(crate) fn from_slug(slug: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.slug() == slug)
    }
}

/// Why a statement needs Full. Carried so the confirmation can name what it is
/// about to do, and so a "don't ask again" tick knows what it is silencing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destructive {
    Drop,
    Truncate,
    UnfilteredDelete,
    /// dbdelve could not parse it, so it cannot say what it does.
    Unreadable,
}

impl Destructive {
    const SUPPRESSIBLE: [Destructive; 3] = [
        Destructive::Drop,
        Destructive::Truncate,
        Destructive::UnfilteredDelete,
    ];

    /// How the confirmation and the picker name a kind.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Destructive::Drop => "DROP",
            Destructive::Truncate => "TRUNCATE",
            Destructive::UnfilteredDelete => "DELETE without WHERE",
            // Unreachable by construction, and the three things that make it so
            // are worth naming because breaking any one of them lands here:
            // `gate` answers RunOnce for an unreadable statement and never
            // Confirm, `suppressible` keeps it out of `confirmed` going in, and
            // `from_slug` keeps it out coming back off disk.
            Destructive::Unreadable => unreachable!("an unreadable statement is never named"),
        }
    }

    /// How a silenced kind is written to `profiles.toml`.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Destructive::Drop => "drop",
            Destructive::Truncate => "truncate",
            Destructive::UnfilteredDelete => "unfiltered-delete",
            Destructive::Unreadable => "unreadable",
        }
    }

    /// `None` for anything that has no business in a silenced list -- a slug this
    /// build does not have, and `unreadable`, which §5.3 says is never
    /// suppressible however it got written there.
    pub(crate) fn from_slug(slug: &str) -> Option<Destructive> {
        Destructive::SUPPRESSIBLE
            .into_iter()
            .find(|kind| kind.slug() == slug)
    }

    /// Whether a "don't ask again" tick may silence this kind. Never for
    /// `Unreadable`: that would silence an open-ended set -- every future typo,
    /// every `DO` block -- on one decision about one of them.
    pub(crate) fn suppressible(self) -> bool {
        !matches!(self, Destructive::Unreadable)
    }
}

// No `Default`: it would inherit `Mode`'s, handing out a Read-write verdict to
// anyone who reached for it. A verdict is something `classify` concludes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) mode: Mode,
    /// **Every** destructive kind the submission carries, in the order it
    /// carries them -- not just the first. Suppression is per kind (spec §6),
    /// so a single slot let one silenced kind mask another:
    /// `DROP TABLE a; TRUNCATE TABLE b` with DROP silenced ran both, and
    /// nobody was ever asked about the TRUNCATE.
    pub(crate) destructive: Vec<Destructive>,
}

impl Verdict {
    pub(crate) const READ: Self = Self {
        mode: Mode::ReadOnly,
        destructive: Vec::new(),
    };
    pub(crate) const WRITE: Self = Self {
        mode: Mode::ReadWrite,
        destructive: Vec::new(),
    };
    pub(crate) const FULL: Self = Self {
        mode: Mode::Full,
        destructive: Vec::new(),
    };

    pub(crate) fn destroys(kind: Destructive) -> Self {
        Self {
            mode: Mode::Full,
            destructive: vec![kind],
        }
    }

    /// The more restrictive of two verdicts: the higher mode, and the union of
    /// what they destroy. A kind only ever arrives with `Mode::Full`, so taking
    /// the union never smuggles a destructive kind under a lower mode.
    /// `Unreadable` is the exception, and `classify` returns it alone.
    pub(crate) fn max(mut self, other: Self) -> Self {
        self.mode = self.mode.max(other.mode);
        for kind in other.destructive {
            if !self.destructive.contains(&kind) {
                self.destructive.push(kind);
            }
        }
        self
    }
}

/// Whether `sql` can be put in an Explain mode, and why not when it cannot.
pub(crate) fn explainable(engine: Engine, sql: &str) -> Result<(), String> {
    match engine.syntax() {
        Syntax::Mongo => crate::mql::explainable(sql),
        // The server refuses what it cannot explain, and says so.
        Syntax::Sql | Syntax::Redis => Ok(()),
    }
}

/// The Explain mode of a statement the user typed as an `EXPLAIN` themselves,
/// so its answer is shown as a plan rather than as rows. Read off the words in
/// front of the statement explained, never off the statement itself.
///
/// `None` wherever the answer is a shape `explain::parse` does not read:
/// MySQL's traditional `EXPLAIN` and both of MariaDB's are tables, SQLite's
/// bare `EXPLAIN` is bytecode, and a `FORMAT` other than text or tree is JSON,
/// XML or YAML. Those stay in the grid, where they are at least legible.
pub(crate) fn typed_explain(engine: Engine, sql: &str) -> Option<ExplainMode> {
    if engine.syntax() == Syntax::Redis {
        return None;
    }
    let Some(dialect) = dialect(engine) else {
        return crate::mql::typed_explain(sql);
    };
    let tokens = Tokenizer::new(dialect.as_ref(), sql).tokenize().ok()?;
    let mut tokens = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .peekable();
    let word = |token: Option<&&Token>, word: &str| matches!(token, Some(Token::Word(found)) if found.value.eq_ignore_ascii_case(word));
    if !word(tokens.next().as_ref(), "EXPLAIN") {
        return None;
    }
    match engine.spec().sql.typed_explain {
        TypedExplain::Postgres if tokens.next_if(|token| **token == Token::LParen).is_some() => {
            let mut mode = ExplainMode::Plan;
            loop {
                let name = tokens.next()?;
                let argument: Vec<String> = std::iter::from_fn(|| {
                    tokens.next_if(|token| !matches!(token, Token::Comma | Token::RParen))
                })
                .map(|token| token.to_string().trim_matches('\'').to_ascii_lowercase())
                .collect();
                if word(Some(&name), "ANALYZE") || word(Some(&name), "ANALYSE") {
                    let off = matches!(
                        argument.as_slice(),
                        [value] if ["false", "off", "0", "no"].contains(&value.as_str())
                    );
                    mode = if off {
                        ExplainMode::Plan
                    } else {
                        ExplainMode::Analyze
                    };
                }
                if word(Some(&name), "FORMAT") && argument != ["text"] {
                    return None;
                }
                if tokens.next()? == &Token::RParen {
                    return Some(mode);
                }
            }
        }
        TypedExplain::Postgres => Some(
            match word(tokens.peek(), "ANALYZE") || word(tokens.peek(), "ANALYSE") {
                true => ExplainMode::Analyze,
                false => ExplainMode::Plan,
            },
        ),
        TypedExplain::MySql => {
            let analyze = tokens
                .next_if(|token| word(Some(token), "ANALYZE"))
                .is_some();
            let tree = match tokens.next_if(|token| word(Some(token), "FORMAT")) {
                Some(_) => tokens.next()? == &Token::Eq && word(tokens.next().as_ref(), "TREE"),
                None => analyze,
            };
            tree.then_some(match analyze {
                true => ExplainMode::Analyze,
                false => ExplainMode::Plan,
            })
        }
        TypedExplain::Sqlite => (word(tokens.next().as_ref(), "QUERY")
            && word(tokens.next().as_ref(), "PLAN"))
        .then_some(ExplainMode::Plan),
        TypedExplain::None => None,
    }
}

/// The lowest mode that may run `sql`, and what makes it dangerous if anything
/// does.
///
/// Parses with `sqlparser` rather than the tree-sitter parse the rest of this
/// module uses. The two do different jobs: tree-sitter is error-tolerant and
/// finds statement boundaries in a buffer someone is still typing into;
/// this needs a typed statement, and would rather refuse than guess.
pub(crate) fn classify(engine: Engine, sql: &str) -> Verdict {
    if engine.syntax() == Syntax::Redis {
        return crate::redis_cmd::classify(sql);
    }
    let Some(dialect) = dialect(engine) else {
        return crate::mql::classify(sql);
    };

    if engine.spec().sql.explain_analyze == Some(ExplainPrefix::Analyze)
        && let Some(explain) = mariadb_analyze_as_explain(dialect.as_ref(), sql)
    {
        return classify(engine, &explain);
    }

    // All or nothing: one statement it cannot read makes the whole submission
    // one it cannot vouch for.
    // An unreadable verdict's mode is the lowest one it may be run once in.
    // Any, where the server holds Read-only to reads; otherwise nothing stops
    // what it turns out to write, so Read-only may not run it at all.
    let Ok(statements) = SqlParser::parse_sql(dialect.as_ref(), sql) else {
        return Verdict {
            mode: if engine.holds_read_only() {
                Mode::ReadOnly
            } else {
                Mode::ReadWrite
            },
            destructive: vec![Destructive::Unreadable],
        };
    };

    // An empty or comment-only string parses to no statements at all. There is
    // nothing there to be dangerous, and a dialog over nothing is noise.
    statements
        .iter()
        .map(statement_verdict)
        .fold(Verdict::READ, Verdict::max)
}

/// The `sqlparser` dialect an engine's SQL is read in; `None` for MongoDB
/// and Redis, which are not SQL.
fn dialect(engine: Engine) -> Option<Box<dyn Dialect>> {
    (engine.syntax() == Syntax::Sql).then(|| parser(engine.spec().sql.parser_dialect))
}

fn parser(dialect: ParserDialect) -> Box<dyn Dialect> {
    match dialect {
        ParserDialect::Generic => Box::new(GenericDialect {}),
        ParserDialect::PostgreSql => Box::new(PostgreSqlDialect {}),
        ParserDialect::MySql => Box::new(MySqlDialect {}),
        ParserDialect::Sqlite => Box::new(SQLiteDialect {}),
        ParserDialect::DuckDb => Box::new(DuckDbDialect {}),
        ParserDialect::Snowflake => Box::new(SnowflakeDialect {}),
        ParserDialect::MsSql => Box::new(MsSqlDialect {}),
    }
}

/// MariaDB's `ANALYZE <statement>` respelled as the `EXPLAIN ANALYZE` sqlparser
/// reads, so it is classified by the statement it runs, as MySQL's is.
///
/// Only a statement that opens with a DML keyword qualifies. `ANALYZE TABLE t`
/// is a write sqlparser already reads, and would turn into an `EXPLAIN` of the
/// query `TABLE t`; anything else this does not recognise stays unreadable.
fn mariadb_analyze_as_explain(dialect: &dyn Dialect, sql: &str) -> Option<String> {
    let tokens = Tokenizer::new(dialect, sql).tokenize().ok()?;
    let mut tokens = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .peekable();
    let keyword = |token: Option<&Token>| match token {
        Some(Token::Word(word)) => Some(word.keyword),
        _ => None,
    };
    if keyword(tokens.next()) != Some(Keyword::ANALYZE) {
        return None;
    }
    if keyword(tokens.peek().copied()) == Some(Keyword::FORMAT) {
        tokens.next();
        if tokens.next() != Some(&Token::Eq) || keyword(tokens.next()) != Some(Keyword::JSON) {
            return None;
        }
    }
    let statement = tokens.next()?;
    let dml = *statement == Token::LParen
        || matches!(
            keyword(Some(statement)),
            Some(
                Keyword::SELECT
                    | Keyword::WITH
                    | Keyword::INSERT
                    | Keyword::REPLACE
                    | Keyword::UPDATE
                    | Keyword::DELETE
            )
        );
    dml.then(|| format!("EXPLAIN {sql}"))
}

/// Whether `sql` can run again just to reload the rows it produced. A grid is
/// reloaded by re-running the statement behind it, and one that writes --
/// `INSERT … RETURNING` traces to its table like any select -- would write
/// again. A statement `classify` cannot read is not vouched for either.
pub(crate) fn rerunnable(engine: Engine, sql: &str) -> bool {
    let verdict = classify(engine, sql);
    verdict.mode == Mode::ReadOnly && verdict.destructive.is_empty()
}

/// Whether the last statement of `sql` answers with rows: a query, or a
/// write with a `RETURNING`. DuckDB answers a command with a result set too,
/// a single `Count` or `Success` column, which only the statement tells apart
/// from a query's own column of that name.
///
/// duckdb-rs keeps the type DuckDB gives a prepared statement to itself, so
/// it is read off the text: the last statement alone, so an earlier one the
/// grammar cannot read does not hide it, and by its first word where the
/// grammar cannot read it either -- DuckDB has queries of its own, `TABLE t`
/// or `PIVOT`, as well as commands. A word not known as a command reads as a
/// query, since showing a command's count as a row costs less than dropping
/// a query's rows.
pub(crate) fn answers_with_rows(engine: Engine, sql: &str) -> bool {
    let Some(dialect) = dialect(engine) else {
        return false;
    };
    let Some(tokens) = located_tokens(dialect.as_ref(), sql) else {
        return true;
    };
    let Some(last) = between_semicolons(&tokens).pop() else {
        return false;
    };
    match SqlParser::parse_sql(dialect.as_ref(), &sql[last.clone()]) {
        Ok(statements) => statements.last().is_some_and(statement_answers_with_rows),
        Err(_) => !tokens
            .iter()
            .filter(|(_, range)| last.contains(&range.start))
            .find_map(|(token, _)| match token {
                Token::Word(word) => Some(word),
                _ => None,
            })
            .is_some_and(|word| {
                COMMAND_WORDS
                    .iter()
                    .any(|command| word.value.eq_ignore_ascii_case(command))
            }),
    }
}

/// The first words of the statements DuckDB answers with a `Count` or a
/// `Success` of its own.
const COMMAND_WORDS: &[&str] = &[
    "ABORT",
    "ALTER",
    "ANALYZE",
    "ATTACH",
    "BEGIN",
    "CHECKPOINT",
    "COMMENT",
    "COMMIT",
    "COPY",
    "CREATE",
    "DEALLOCATE",
    "DELETE",
    "DETACH",
    "DROP",
    "END",
    "EXPORT",
    "FORCE",
    "IMPORT",
    "INSERT",
    "INSTALL",
    "LOAD",
    "MERGE",
    "PREPARE",
    "RESET",
    "ROLLBACK",
    "SET",
    "START",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "VACUUM",
];

fn statement_answers_with_rows(statement: &Statement) -> bool {
    match statement {
        Statement::Query(query) => query_answers_with_rows(query),
        Statement::Insert(Insert { returning, .. })
        | Statement::Update(Update { returning, .. })
        | Statement::Delete(Delete { returning, .. }) => returning.is_some(),
        Statement::Merge(Merge { output, .. }) => output.is_some(),
        _ => false,
    }
}

/// A query answers with rows unless its body is a write a `WITH` leads:
/// `WITH s AS (…) INSERT INTO t SELECT * FROM s` parses as a query.
fn query_answers_with_rows(query: &Query) -> bool {
    match query.body.as_ref() {
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_answers_with_rows(statement),
        SetExpr::Query(query) => query_answers_with_rows(query),
        _ => true,
    }
}

/// Whether a submission may be stopped part way once its rows pass the limit:
/// a read, and the only statement in it. Stopping one statement of several
/// ends the rest with it -- a `COMMIT` behind a `SELECT` never runs, a `SET`
/// before it is rolled back with the implicit transaction -- and the rows on
/// screen are not the last statement's. A SQL Server batch is several
/// statements in one submission wherever the user did not separate them.
///
/// Both parsers have to count one: the grammar reads `BEGIN; …; COMMIT` as a
/// single transaction block. And it must leave the session alone: a Postgres
/// cancel aborts the implicit transaction a `set_config` or advisory lock
/// lives in, so such a read is drained rather than stopped.
pub(crate) fn stoppable(engine: Engine, sql: &str) -> bool {
    let parsed_alone = match dialect(engine) {
        Some(dialect) => SqlParser::parse_sql(dialect.as_ref(), sql)
            .is_ok_and(|statements| statements.len() == 1),
        None => true,
    };
    rerunnable(engine, sql)
        && parsed_alone
        && Buffer::for_engine(engine, sql).statements().len() == 1
        && !touches_session(engine, sql)
}

/// The one table a query's rows are rows of, read off the statement for an
/// engine whose driver does not say which table a result column came from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TableRead {
    /// The name as written, one entry per dotted part.
    pub(crate) table: Vec<String>,
    pub(crate) columns: Vec<Projected>,
}

/// One entry of a [`TableRead`]'s select list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Projected {
    /// A `*`, which stands for every column of the table in its own order.
    Every,
    /// A bare or table-qualified name, as written.
    Column(String),
    /// Anything else, which names no column a write could land on.
    Computed,
}

/// [`TableRead`] for the last statement of `sql`, when it is a plain select of
/// one table: no join, subquery, table function, CTE, set operation,
/// `DISTINCT`, grouping or window clause, and no table alias renaming the
/// columns. Every one of those makes a result row something other than one
/// row of the table, or a column something other than the column it names,
/// so each refuses rather than being read past.
pub(crate) fn table_read(engine: Engine, sql: &str) -> Option<TableRead> {
    use sqlparser::ast::{
        GroupByExpr, ObjectName, SelectFlavor, SelectItem, SelectItemQualifiedWildcardKind,
        TableFactor, WildcardAdditionalOptions,
    };

    let dialect = dialect(engine)?;
    let Statement::Query(query) = SqlParser::parse_sql(dialect.as_ref(), sql).ok()?.pop()? else {
        return None;
    };
    let Query {
        with: None,
        body,
        locks,
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators,
        ..
    } = *query
    else {
        return None;
    };
    let SetExpr::Select(select) = *body else {
        return None;
    };
    let plain = select.distinct.is_none()
        && select.select_modifiers.is_none()
        && select.top.is_none()
        && select.exclude.is_none()
        && select.into.is_none()
        && select.lateral_views.is_empty()
        && select.prewhere.is_none()
        && select.connect_by.is_empty()
        && matches!(&select.group_by, GroupByExpr::Expressions(by, with) if by.is_empty() && with.is_empty())
        && select.cluster_by.is_empty()
        && select.distribute_by.is_empty()
        && select.sort_by.is_empty()
        && select.having.is_none()
        && select.named_window.is_empty()
        && select.qualify.is_none()
        && select.value_table_mode.is_none()
        && locks.is_empty()
        && pipe_operators.is_empty();
    let [from] = select.from.as_slice() else {
        return None;
    };
    let TableFactor::Table {
        name,
        alias,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        index_hints,
        ..
    } = &from.relation
    else {
        return None;
    };
    if !plain
        || !from.joins.is_empty()
        || !with_hints.is_empty()
        || !partitions.is_empty()
        || !index_hints.is_empty()
        || alias
            .as_ref()
            .is_some_and(|alias| !alias.columns.is_empty())
    {
        return None;
    }

    let parts = |name: &ObjectName| -> Option<Vec<String>> {
        name.0
            .iter()
            .map(|part| part.as_ident().map(|ident| ident.value.clone()))
            .collect()
    };
    let table = parts(name)?;
    // Once aliased, the table is named by its alias alone. Unaliased, by any
    // tail of the name it was written with.
    let qualifiers: Vec<&[String]> = match alias {
        Some(alias) => vec![std::slice::from_ref(&alias.name.value)],
        None => (0..table.len()).map(|from| &table[from..]).collect(),
    };
    let qualifies = |written: &[String]| {
        qualifiers.iter().any(|qualifier| {
            qualifier.len() == written.len()
                && qualifier
                    .iter()
                    .zip(written)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
    };
    let bare = |options: &WildcardAdditionalOptions| {
        options.opt_ilike.is_none()
            && options.opt_exclude.is_none()
            && options.opt_except.is_none()
            && options.opt_replace.is_none()
            && options.opt_rename.is_none()
            && options.opt_alias.is_none()
    };
    let column = |expr: &Expr| match expr {
        Expr::Identifier(ident) => Projected::Column(ident.value.clone()),
        Expr::CompoundIdentifier(idents) => match idents.split_last() {
            Some((last, qualifier))
                if qualifies(
                    &qualifier
                        .iter()
                        .map(|ident| ident.value.clone())
                        .collect::<Vec<_>>(),
                ) =>
            {
                Projected::Column(last.value.clone())
            }
            // A struct's field, read off a column of the table.
            _ => Projected::Computed,
        },
        _ => Projected::Computed,
    };

    let mut columns = Vec::with_capacity(select.projection.len().max(1));
    if select.projection.is_empty() && select.flavor == SelectFlavor::FromFirstNoSelect {
        columns.push(Projected::Every);
    }
    for item in &select.projection {
        columns.push(match item {
            SelectItem::Wildcard(options) if bare(options) => Projected::Every,
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(name),
                options,
            ) if bare(options) && qualifies(&parts(name)?) => Projected::Every,
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => column(expr),
            _ => return None,
        });
    }
    Some(TableRead { table, columns })
}

/// Whether `sql` is nothing but queries that only read: no `SET`, `USE`,
/// transaction control, declaration or `SELECT … INTO`, no call that reads or
/// changes what only its session holds, and nothing `classify` cannot read.
/// Such a submission leaves its session as it found it, and reads the same on
/// another session to the same server as on this one, bar what this one
/// already holds that a fresh one would not.
pub(crate) fn plain_read(engine: Engine, sql: &str) -> bool {
    let queries_only = match dialect(engine) {
        Some(dialect) => SqlParser::parse_sql(dialect.as_ref(), sql).is_ok_and(|statements| {
            statements
                .iter()
                .all(|statement| matches!(statement, Statement::Query(_)))
        }),
        None => true,
    };
    queries_only && rerunnable(engine, sql) && !touches_session(engine, sql)
}

/// Calls whose answer or effect belongs to the session they run on: a
/// sequence's value, a session variable, an advisory or named lock, the last
/// identity inserted.
const SESSION_FUNCTIONS: &[&str] = &[
    "nextval",
    "currval",
    "setval",
    "lastval",
    "set_config",
    "last_insert_id",
    "get_lock",
    "release_lock",
    "release_all_locks",
    "scope_identity",
    "sp_getapplock",
    "sp_releaseapplock",
];

/// Whether a query calls one of [`SESSION_FUNCTIONS`], an advisory lock, reads
/// `@@IDENTITY` or assigns a variable with `:=`. Read off the tokens whatever
/// the engine, so a call anywhere in the statement counts, and a column that
/// merely shares a name keeps its statement in turn, the conservative side.
fn touches_session(engine: Engine, sql: &str) -> bool {
    let Some(dialect) = dialect(engine) else {
        return false;
    };
    let Ok(tokens) = Tokenizer::new(dialect.as_ref(), sql).tokenize() else {
        return true;
    };
    let mut tokens = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .peekable();
    while let Some(token) = tokens.next() {
        match token {
            Token::Assignment => return true,
            Token::Word(word) => {
                let name = word.value.to_lowercase();
                let called = tokens.peek() == Some(&&Token::LParen);
                if name == "@@identity"
                    || called
                        && (SESSION_FUNCTIONS.contains(&name.as_str())
                            || name.starts_with("pg_advisory")
                            || name.starts_with("pg_try_advisory"))
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// What running `sql` leaves on its session that a fresh connection to the
/// same profile would not have, which is what keeps a queue of reads from
/// running at once on fresh ones.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Leaves {
    /// `Some(true)` when it leaves a transaction open, `Some(false)` when it
    /// ends one, `None` when it does neither.
    pub(crate) transaction: Option<bool>,
    /// A `SET` (MySQL's `autocommit` among them), `USE`, `PRAGMA … =`, table
    /// lock, temporary table or variable `SELECT … INTO`, which lasts until
    /// the session does, or a call [`touches_session`] finds: `set_config`, a
    /// lock, an `@x :=`. One that only reads what the session holds counts
    /// too, the conservative side.
    pub(crate) settings: bool,
}

/// [`Leaves`] for one submission of the user's. A read, a `SHOW`, an `EXPLAIN`
/// or an autocommitted write leaves nothing a fresh connection would not see,
/// and neither does a statement nothing can read -- bar a `SET` or `USE` the
/// grammar does not know, which still changed the session if it ran.
/// Snowflake and MongoDB keep nothing from one run to the next.
pub(crate) fn leaves(engine: Engine, sql: &str) -> Leaves {
    if engine.syntax() == Syntax::Redis {
        return crate::redis_cmd::leaves(sql);
    }
    let mut leaves = Leaves::default();
    let Some(dialect) = dialect(engine).filter(|_| !engine.spec().sql.stateless_requests) else {
        return leaves;
    };
    match SqlParser::parse_sql(dialect.as_ref(), sql) {
        Ok(statements) => statements
            .iter()
            .for_each(|statement| leaves.add(statement)),
        Err(_) => {
            let tokens = Tokenizer::new(dialect.as_ref(), sql)
                .tokenize()
                .unwrap_or_default();
            let first = tokens.iter().find_map(|token| match token {
                Token::Word(word) => Some(word.keyword),
                _ => None,
            });
            leaves.settings = match first {
                Some(Keyword::SET | Keyword::USE) => true,
                Some(Keyword::PRAGMA) => tokens.contains(&Token::Eq),
                _ => false,
            };
        }
    }
    leaves.settings |= touches_session(engine, sql);
    leaves
}

impl Leaves {
    fn add(&mut self, statement: &Statement) {
        match statement {
            // A T-SQL `BEGIN … END` block, which opens no transaction itself.
            Statement::StartTransaction {
                has_end_keyword: true,
                statements,
                ..
            } => statements.iter().for_each(|statement| self.add(statement)),
            Statement::StartTransaction { .. } => self.transaction = Some(true),
            // `AND CHAIN` opens the next one as it ends this one.
            Statement::Commit { chain, .. }
            | Statement::Rollback {
                chain,
                savepoint: None,
            } => self.transaction = Some(*chain),
            Statement::Set(_)
            | Statement::Use(_)
            | Statement::LockTables { .. }
            | Statement::Pragma { is_eq: true, .. } => self.settings = true,
            Statement::CreateTable(table) => {
                self.settings |= table.temporary || table.name.to_string().starts_with('#');
            }
            Statement::Query(query) => {
                if let SetExpr::Select(select) = query.body.as_ref()
                    && let Some(into) = &select.into
                {
                    self.settings |= into.temporary
                        || into
                            .targets
                            .iter()
                            .any(|target| target.to_string().starts_with(['#', '@']));
                }
            }
            _ => {}
        }
    }
}

/// The lowest mode that may run one statement: what its variant earns, raised
/// by whatever the `Query` it owns turns out to contain.
///
/// The second half is never optional. A `Query` in any position can carry a
/// data-modifying CTE, so
/// `COPY (WITH x AS (DELETE FROM t RETURNING a) SELECT a FROM x) TO STDOUT`
/// empties a table under a variant that reads as `COPY TO`.
fn statement_verdict(statement: &Statement) -> Verdict {
    let verdict = variant_verdict(statement);
    match owned_query(statement) {
        Some(query) => verdict.max(query_verdict(query)),
        None => verdict,
    }
}

/// The `Query` a statement owns, if it owns one.
///
/// One place rather than a recursion inside each arm, so a variant added to
/// `variant_verdict` inherits the CTE check instead of having to remember it.
/// Four variants here already held a `Query` the classifier never looked into
/// (spec §3.5), which is what that costs.
fn owned_query(statement: &Statement) -> Option<&Query> {
    match statement {
        Statement::Query(query) => Some(query),
        Statement::Copy {
            source: CopySource::Query(query),
            ..
        } => Some(query),
        Statement::CreateTable(create) => create.query.as_deref(),
        Statement::CreateView(create) => Some(&create.query),
        Statement::Insert(insert) => insert.source.as_deref(),
        _ => None,
    }
}

/// What a statement's variant alone says. Never called directly: the fold in
/// `statement_verdict` is the half that reads what the variant is carrying.
fn variant_verdict(statement: &Statement) -> Verdict {
    match statement {
        // Read only as a variant. Everything dangerous a query can hold is
        // inside it, and `statement_verdict` folds that in.
        Statement::Query(_) => Verdict::READ,

        // `EXPLAIN ANALYZE DELETE FROM t` runs the delete -- documented in
        // Postgres, and in MySQL since 8.0.18. dbdelve never sends it to SQLite:
        // `Engine::explain_prefix` returns None for that pair.
        Statement::Explain {
            analyze,
            options,
            statement,
            ..
        } => {
            let analyze = *analyze
                || options
                    .iter()
                    .flatten()
                    .any(analyze_option_is_on);
            if analyze {
                statement_verdict(statement)
            } else {
                Verdict::READ
            }
        }
        Statement::ExplainTable { .. } => Verdict::READ,

        // A Snowflake Scripting block, `BEGIN DELETE FROM t; END`, parses as the
        // same variant as a bare `BEGIN` with its body inside. Any exception
        // handler holds statements of its own, so one is not read into.
        Statement::StartTransaction {
            statements,
            exception,
            ..
        } if !statements.is_empty() || exception.is_some() => {
            if exception.is_some() {
                Verdict::FULL
            } else {
                statements
                    .iter()
                    .map(statement_verdict)
                    .fold(Verdict::READ, Verdict::max)
            }
        }

        // `IF … ELSE` (T-SQL's, and the `IF … END IF` Postgres, MySQL and
        // Snowflake parse to the same variant) is as dangerous as what it holds.
        // Every branch counts, since which one runs is the server's to decide,
        // and so does every condition, since each one runs.
        Statement::If(branching) => {
            let blocks = std::iter::once(&branching.if_block)
                .chain(&branching.elseif_blocks)
                .chain(&branching.else_block);
            blocks
                .clone()
                .filter_map(|block| block.condition.as_ref())
                .map(condition_verdict)
                .chain(
                    blocks
                        .flat_map(ConditionalStatementBlock::statements)
                        .map(statement_verdict),
                )
                .fold(Verdict::READ, Verdict::max)
        }

        Statement::ShowTables { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowCharset { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowObjects { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowViews { .. }
        // A deliberate exception to the wildcard: `USE db` repoints the session
        // and touches no data, so refusing it in Read-only would refuse
        // navigation, not damage.
        | Statement::Use { .. }
        | Statement::Set { .. }
        | Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        // Savepoints touch no data, and `ROLLBACK TO` above is already a read.
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => Verdict::READ,

        // COPY TO reads a table out to a file; COPY FROM loads rows in. Either
        // direction against a `PROGRAM` or a file runs on the *server*: a shell
        // command, or a read/write of the server's filesystem. Only the standard
        // streams stay inside the database.
        Statement::Copy { to, target, .. } => match target {
            CopyTarget::File { .. } | CopyTarget::Program { .. } => Verdict::FULL,
            _ if *to => Verdict::READ,
            _ => Verdict::WRITE,
        },

        // Replacing an object that holds data drops that data first: `CREATE OR
        // REPLACE SCHEMA s` empties every table in `s`, and `INSERT OVERWRITE`
        // truncates before it inserts. A replaced view, function or procedure
        // holds no rows, so those stay with the object they create.
        Statement::CreateTable(create) if create.or_replace => {
            Verdict::destroys(Destructive::Drop)
        }
        Statement::CreateSchema {
            or_replace: true, ..
        }
        | Statement::CreateDatabase {
            or_replace: true, ..
        }
        // An internal stage holds the files put into it.
        | Statement::CreateStage {
            or_replace: true, ..
        } => Verdict::destroys(Destructive::Drop),
        Statement::Insert(insert) if insert.overwrite => {
            Verdict::destroys(Destructive::Truncate)
        }

        Statement::Insert { .. }
        | Statement::Update { .. }
        | Statement::CreateTable { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateView { .. }
        | Statement::CreateSchema { .. }
        | Statement::Comment { .. }
        | Statement::Analyze { .. }
        | Statement::Vacuum { .. } => Verdict::WRITE,

        Statement::Delete(delete) => {
            if delete.selection.is_some() {
                Verdict::WRITE
            } else {
                Verdict::destroys(Destructive::UnfilteredDelete)
            }
        }

        Statement::Drop { .. } => Verdict::destroys(Destructive::Drop),
        Statement::Truncate { .. } => Verdict::destroys(Destructive::Truncate),

        // One ALTER can carry several operations, so the verdict is the maximum
        // over them and not the first.
        Statement::AlterTable(alter) => alter
            .operations
            .iter()
            .map(alter_verdict)
            .fold(Verdict::WRITE, Verdict::max),

        // Everything else needs Full: a statement this function has not been
        // taught about is one nobody has decided is safe. A statement type added
        // by a later crate version lands here silently, so the arm has to be the
        // conservative one -- `Statement` is not `#[non_exhaustive]`, and the
        // build will not break to ask.
        _ => Verdict::FULL,
    }
}

/// Whether an `EXPLAIN (…)` option turns ANALYZE on -- which runs the statement
/// for real.
///
/// sqlparser only sets the `analyze` flag for the keyword form
/// (`EXPLAIN ANALYZE …`); the parenthesized form lands in `options` untouched,
/// so `EXPLAIN (ANALYZE TRUE) DELETE FROM t` read as a plain EXPLAIN and
/// deleted the rows. Anything but an explicit off counts as on: an argument
/// this does not recognise is not a reason to call a write a read.
fn analyze_option_is_on(option: &UtilityOption) -> bool {
    if !option.name.value.eq_ignore_ascii_case("analyze") {
        return false;
    }
    match &option.arg {
        Some(arg) => !matches!(
            arg.to_string().to_ascii_lowercase().as_str(),
            "false" | "off" | "0"
        ),
        None => true,
    }
}

/// A CTE body can be a `DELETE`. `WITH x AS (DELETE FROM t RETURNING *) SELECT *
/// FROM x` parses as `Statement::Query`, so reading only the top-level variant
/// calls a statement that empties a table a read.
fn query_verdict(query: &Query) -> Verdict {
    let mut verdict = set_expr_verdict(&query.body);
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            verdict = verdict.max(query_verdict(&cte.query));
        }
    }
    verdict
}

fn set_expr_verdict(body: &SetExpr) -> Verdict {
    match body {
        // `SELECT * INTO newt FROM t` is DDL wearing a select's clothes: same
        // variant as a read, one field apart, and it creates a table.
        SetExpr::Select(select) if select.into.is_some() => Verdict::WRITE,
        SetExpr::Select(_) | SetExpr::Values(_) | SetExpr::Table(_) => Verdict::READ,
        SetExpr::Query(query) => query_verdict(query),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_verdict(left).max(set_expr_verdict(right))
        }
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_verdict(statement),
    }
}

/// What evaluating an `IF`'s condition can do. Before `IF` had an arm it
/// needed Full on every engine, and a condition calling a function can still
/// write anything a Postgres or Snowflake function can, so everything but
/// operators over names, values and subqueries keeps that verdict. A subquery
/// is read like any other query.
fn condition_verdict(condition: &Expr) -> Verdict {
    match condition {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => Verdict::READ,
        Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. } => condition_verdict(expr),
        Expr::BinaryOp { left, right, .. }
        | Expr::Like {
            expr: left,
            pattern: right,
            escape_char: None,
            ..
        }
        | Expr::ILike {
            expr: left,
            pattern: right,
            escape_char: None,
            ..
        } => condition_verdict(left).max(condition_verdict(right)),
        Expr::Between {
            expr, low, high, ..
        } => condition_verdict(expr)
            .max(condition_verdict(low))
            .max(condition_verdict(high)),
        Expr::InList { expr, list, .. } => list
            .iter()
            .map(condition_verdict)
            .fold(condition_verdict(expr), Verdict::max),
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => query_verdict(subquery),
        Expr::InSubquery { expr, subquery, .. } => {
            condition_verdict(expr).max(query_verdict(subquery))
        }
        _ => Verdict::FULL,
    }
}

/// Additive is `ADD COLUMN` and nothing else. Deliberately strict: an operation
/// this does not name -- and there are around sixty -- is treated as able to
/// lose data, for the same reason the top-level wildcard is.
fn alter_verdict(operation: &AlterTableOperation) -> Verdict {
    match operation {
        AlterTableOperation::AddColumn { .. } => Verdict::WRITE,
        _ => Verdict::FULL,
    }
}

/// Why the mode stopped a statement. Answered by `gate`, which decides; saying
/// so is the dialog's job and lives elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The connection is not allowed to run this. The mode carried is exactly
    /// what the statement needs -- never a higher one.
    Upgrade(Mode),
    /// Allowed, but it destroys something and this connection has not silenced
    /// that kind.
    Confirm(Destructive),
    /// dbdelve could not read it. Runs once on confirmation and changes nothing.
    RunOnce,
}

/// Whether the mode stops this statement. `None` means run it.
pub(crate) fn gate(verdict: &Verdict, mode: Mode, confirmed: &[Destructive]) -> Option<Stop> {
    // Tested before the mode comparison, because it is the one verdict whose
    // remedy is not a mode change: every typo lands here, and asking someone to
    // raise a connection to Full to get a syntax error back would teach them to
    // live in Full.
    //
    // The one mode it can ask for is Read-write, where no server-side hold
    // stands behind Read-only (see `classify`).
    if verdict.destructive.contains(&Destructive::Unreadable) {
        if verdict.mode > mode {
            return Some(Stop::Upgrade(verdict.mode));
        }
        return Some(Stop::RunOnce);
    }
    if verdict.mode > mode {
        return Some(Stop::Upgrade(verdict.mode));
    }
    // The first kind nobody has silenced, and only that one: the dialog names a
    // single kind, and confirming it runs the whole submission -- so
    // `DROP TABLE a; TRUNCATE TABLE b` asks about the `DROP` and then runs both.
    // What the set buys is narrower than one confirmation per kind: it stops a
    // silenced kind from masking an unsilenced one, which a single slot did.
    // The dialog renders the submission, so what Run covers is on screen.
    verdict
        .destructive
        .iter()
        .find(|kind| !confirmed.contains(kind))
        .copied()
        .map(Stop::Confirm)
}

/// `sql` laid out over several lines: the same tokens, indented. Refused when
/// the buffer holds a dollar-quoted body, which this cannot reflow safely.
///
/// Token-level reformatting rather than a round trip through the `sqlparser`
/// AST this module already parses with for `classify`. Regenerating a statement
/// from that AST drops every comment the user wrote and quietly rewrites the
/// dialect corners sqlparser only half-supports, which is hard rule 1 — dbdelve
/// never rewrites SQL behind the user's back.
///
/// A tokenizer is meant to be unable to do either, and sqlformat is one
/// everywhere except `$$`: it tokenizes *inside* a dollar-quoted body instead of
/// carrying it through whole, so `select $$hello   world$$` comes back as
/// `$$hello world$$` — a different value, not a different layout. Single-quoted
/// literals are safe. So the rule is refuse, never guess: a buffer holding one
/// is returned unformatted rather than silently edited.
//
// ponytail: refuses the whole buffer over one dollar quote, where splitting on
// the quoted regions and formatting only the text between them would still
// format the rest. Worth doing when someone is actually editing plpgsql here.
//
// ponytail: `FormatOptions`' `dialect` is Generic except on SQL Server, where
// Generic splits `[my col]` into `[ my col ]`, another name. Postgres's dialect
// is the upgrade path if its output ever looks wrong there.
///
/// Each statement is reflowed on its own, and the text between statements --
/// blank lines, comments, the `;`s and SQL Server's `GO` lines -- is kept as
/// written, but for two things: a statement sharing a line with the one before
/// it starts a line of its own, and one indented on its line comes back flush.
/// Reflowed with its batch, `GO 5` becomes a `GO` and a stray `5` on the next
/// line: a count the refusal no longer sees.
///
/// Statements are cut at the `;`s sqlparser reads in the engine's dialect,
/// which knows MySQL's `\'` where `Buffer`'s split does not and would cut a
/// literal in two. SQL Server's are `Buffer`'s, which reads batches and
/// `BEGIN … END`, and T-SQL has no backslash escapes.
///
/// The result must tokenize as the buffer did, whitespace aside, or it is
/// refused: sqlformat respaces some tokens into others, like `[my col]`
/// outside SQL Server's dialect or MySQL's `0x0A`.
///
/// Err is the refusal, said to the user as it stands. A MongoDB buffer is not
/// SQL, and sqlformat over it splits regexes and turns comment text into code,
/// so `mql::format` lays it out instead.
//
// ponytail: a selection that starts partway through a line comes back with its
// later lines at column 0, not under its first. Indenting them is not a plain
// prefix per line here, as it is in `mql::format`: sqlformat carries a
// multi-line string literal through as one token, and a prefix would edit its
// value.
pub(crate) fn format(engine: Engine, sql: &str) -> Result<String, &'static str> {
    match engine.syntax() {
        Syntax::Sql => {}
        Syntax::Mongo => return crate::mql::format(sql),
        Syntax::Redis => return Err("Not formatting: a Redis command is one line, as typed."),
    }
    let reader = parser(engine.spec().sql.parser_dialect);
    let layout = sqlformat_dialect(engine.spec().sql.format_dialect);
    if has_dollar_quote(sql) {
        return Err(DOLLAR_QUOTED);
    }
    let Some(tokens) = located_tokens(reader.as_ref(), sql) else {
        return Err(UNENDED);
    };
    let statements = match batches_on(engine) {
        true => Buffer::for_engine(engine, sql).statements().to_vec(),
        false => between_semicolons(&tokens),
    };
    let options = sqlformat::FormatOptions {
        dialect: layout,
        ..sqlformat::FormatOptions::default()
    };

    let mut formatted = String::with_capacity(sql.len());
    let mut written = 0;
    for (index, statement) in statements.iter().enumerate() {
        let gap = &sql[written..statement.start];
        let gap = match gap.rfind('\n') {
            Some(line) if gap[line + 1..].trim().is_empty() => &gap[..=line],
            Some(_) => gap,
            None if index > 0 => {
                formatted += gap.trim_end();
                "\n"
            }
            None => gap,
        };
        formatted += gap;
        formatted += &sqlformat::format(
            &sql[statement.clone()],
            &sqlformat::QueryParams::default(),
            &options,
        );
        written = statement.end;
    }
    formatted += &sql[written..];

    let before = layout_free(tokens.into_iter().map(|(token, _)| token));
    let after = Tokenizer::new(reader.as_ref(), &formatted)
        .tokenize()
        .ok()
        .map(layout_free);
    match after == Some(before) {
        true => Ok(formatted),
        false => Err("Not formatting: the new layout would read as different SQL."),
    }
}

fn sqlformat_dialect(dialect: FormatDialect) -> sqlformat::Dialect {
    match dialect {
        FormatDialect::Generic => sqlformat::Dialect::Generic,
        FormatDialect::SqlServer => sqlformat::Dialect::SQLServer,
    }
}

const DOLLAR_QUOTED: &str = "Not formatting: a dollar-quoted body would be rewritten.";
const UNENDED: &str = "Not formatting: a string, quoted name or comment does not end.";

/// [`format`] of the `selection` of `sql` alone.
///
/// Refused when either end of the selection falls inside a word, a literal, a
/// quoted name or a comment of the buffer around it: reflowed on its own, the
/// piece of a literal reads as code, and its text changes with the layout.
pub(crate) fn format_selection(
    engine: Engine,
    sql: &str,
    selection: Range<usize>,
) -> Result<String, &'static str> {
    let splits = match dialect(engine) {
        Some(reader) => {
            // Text past the end's line cannot change how the text up to it
            // reads, and a buffer can be megabytes.
            let line_end = sql[selection.end..]
                .find('\n')
                .map_or(sql.len(), |at| selection.end + at);
            let tokens = located_tokens(reader.as_ref(), &sql[..line_end]).ok_or(UNENDED)?;
            [selection.start, selection.end].iter().any(|&at| {
                tokens
                    .iter()
                    .any(|(token, range)| !is_layout(token) && range.start < at && at < range.end)
            })
        }
        None => [selection.start, selection.end]
            .iter()
            .any(|&at| crate::mql::splits_a_token(sql, at)),
    };
    if splits {
        return Err(
            "Not formatting: the selection starts or ends partway through a word, string or \
             comment.",
        );
    }
    format(engine, &sql[selection])
}

/// The statements in `tokens` as the dialect's tokenizer cuts them: from the
/// first token after a `;` that is neither whitespace nor a comment to the last
/// one before the next `;`.
fn between_semicolons(tokens: &[(Token, Range<usize>)]) -> Vec<Range<usize>> {
    let mut statements = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for (token, range) in tokens {
        match token {
            Token::SemiColon => statements.extend(current.take()),
            Token::Whitespace(_) => {}
            _ => {
                let start = current
                    .as_ref()
                    .map_or(range.start, |current| current.start);
                current = Some(start..range.end);
            }
        }
    }
    statements.extend(current);
    statements
}

fn is_layout(token: &Token) -> bool {
    matches!(
        token,
        Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
    )
}

/// `tokens` with the whitespace between them left out, and each comment
/// reduced to its words, which sqlformat respaces but never rewrites.
fn layout_free(tokens: impl IntoIterator<Item = Token>) -> Vec<Token> {
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    tokens
        .into_iter()
        .filter(|token| !is_layout(token))
        .map(|token| match token {
            Token::Whitespace(Whitespace::SingleLineComment { comment, prefix }) => {
                Token::Whitespace(Whitespace::SingleLineComment {
                    comment: words(&comment),
                    prefix,
                })
            }
            Token::Whitespace(Whitespace::MultiLineComment(comment)) => {
                Token::Whitespace(Whitespace::MultiLineComment(words(&comment)))
            }
            token => token,
        })
        .collect()
}

/// The tokens of `sql` with the byte range each covers. `None` when `sql` does
/// not tokenize.
fn located_tokens(dialect: &dyn Dialect, sql: &str) -> Option<Vec<(Token, Range<usize>)>> {
    let tokens = Tokenizer::new(dialect, sql).tokenize_with_location().ok()?;
    // Spans are lines and columns of chars, in source order, so one walk
    // forward through the text turns them all into byte offsets.
    let mut chars = sql.char_indices().peekable();
    let (mut line, mut column) = (1, 1);
    let mut offset_of = |location: Location| {
        while (line, column) < (location.line, location.column) {
            let Some((_, c)) = chars.next() else {
                break;
            };
            if c == '\n' {
                (line, column) = (line + 1, 1);
            } else {
                column += 1;
            }
        }
        chars.peek().map_or(sql.len(), |(offset, _)| *offset)
    };
    Some(
        tokens
            .into_iter()
            .map(|token| {
                let start = offset_of(token.span.start);
                (token.token, start..offset_of(token.span.end))
            })
            .collect(),
    )
}

/// Whether `sql` opens a `$$` or `$tag$` body anywhere.
///
/// Deliberately not a parse: it answers "is this text unsafe to reflow", and
/// erring towards yes only costs a refusal. The tag rule is Postgres's own, and
/// it is what keeps the `$1` placeholder out -- a tag is empty or starts with a
/// letter, so `$1` never reads as an opening quote.
fn has_dollar_quote(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        if b != b'$' {
            return false;
        }
        let tag = bytes[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count();
        let starts_valid = tag == 0 || !bytes[i + 1].is_ascii_digit();
        starts_valid && bytes.get(i + 1 + tag) == Some(&b'$')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_cycle_runs_read_only_to_full_and_back() {
        assert_eq!(Mode::ReadOnly.next(), Mode::ReadWrite);
        assert_eq!(Mode::ReadWrite.next(), Mode::Full);
        assert_eq!(Mode::Full.next(), Mode::ReadOnly);
    }

    #[test]
    fn a_grids_statement_is_found_wherever_it_stands_in_the_buffer() {
        let sql = "SELECT 1;\nSELECT * FROM t;\nDELETE FROM t;";
        let buffer = Buffer::parse(sql);
        let select = sql.find("SELECT *").unwrap();
        let delete = sql.find("DELETE").unwrap();
        // The cursor on another statement does not make that one the grid's.
        let grid = buffer.statement_at(select).unwrap();
        assert_eq!(
            buffer.find(Engine::Postgres, sql, delete, &sql[grid.clone()]),
            Some(grid)
        );
        let first = buffer.statement_at(0).unwrap();
        assert_eq!(
            buffer.find(Engine::Postgres, sql, 0, &sql[first.clone()]),
            Some(first)
        );
        assert_eq!(
            buffer.find(Engine::Postgres, sql, delete, "SELECT * FROM u"),
            None
        );
    }

    fn texts(sql: &str) -> Vec<&str> {
        Buffer::parse(sql)
            .statements()
            .iter()
            .map(|r| &sql[r.clone()])
            .collect()
    }

    #[test]
    fn a_sort_goes_in_before_the_limit() {
        // Appended after the limit it would not parse; applied after the limit
        // it would sort one arbitrary thousand rows of the table.
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                r#"SELECT * FROM "public"."measurements" LIMIT 1000"#,
                &[SortKey::new(r#""id""#, false)]
            )
            .unwrap(),
            r#"SELECT * FROM "public"."measurements" ORDER BY "id" DESC LIMIT 1000"#
        );
    }

    #[test]
    fn a_second_key_joins_the_first() {
        let sorted = with_order_by(
            Engine::Postgres,
            "SELECT * FROM t",
            &[SortKey::new(r#""a""#, true), SortKey::new("3", false)],
        )
        .unwrap();

        assert_eq!(sorted, r#"SELECT * FROM t ORDER BY "a" ASC, 3 DESC"#);
        assert_eq!(
            order_by(Engine::Postgres, &sorted).unwrap(),
            vec![SortKey::new(r#""a""#, true), SortKey::new("3", false)]
        );
    }

    #[test]
    fn sorting_again_replaces_the_clause_it_wrote() {
        let once = with_order_by(
            Engine::Postgres,
            "SELECT * FROM t LIMIT 5",
            &[SortKey::new("a", true)],
        )
        .unwrap();
        let twice = with_order_by(Engine::Postgres, &once, &[SortKey::new("b", false)]).unwrap();

        assert_eq!(twice, "SELECT * FROM t ORDER BY b DESC LIMIT 5");
        // And clearing it leaves the statement as it was, not a hole.
        assert_eq!(
            with_order_by(Engine::Postgres, &twice, &[]).unwrap(),
            "SELECT * FROM t LIMIT 5"
        );
    }

    #[test]
    fn a_key_the_user_wrote_reads_back_verbatim() {
        let keys = order_by(
            Engine::Postgres,
            "SELECT * FROM t ORDER BY lower(name), 2 DESC",
        )
        .unwrap();

        assert_eq!(
            keys,
            vec![SortKey::new("lower(name)", true), SortKey::new("2", false)]
        );
    }

    #[test]
    fn a_union_sorts_at_the_end_of_the_whole_query() {
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                "SELECT a FROM t UNION SELECT a FROM u LIMIT 3",
                &[SortKey::new("a", true)]
            )
            .unwrap(),
            "SELECT a FROM t UNION SELECT a FROM u ORDER BY a ASC LIMIT 3"
        );
    }

    #[test]
    fn a_subquerys_own_sort_is_left_alone() {
        // The inner ORDER BY belongs to the subquery. Reading it as the outer
        // query's sort would flip a clause the user wrote for another purpose.
        let sql = "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) s";

        assert_eq!(order_by(Engine::Postgres, sql).unwrap(), vec![]);
        assert_eq!(
            with_order_by(Engine::Postgres, sql, &[SortKey::new("a", false)]).unwrap(),
            "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) s ORDER BY a DESC"
        );
    }

    #[test]
    fn nothing_is_spliced_into_a_statement_dbdelve_cannot_read_whole() {
        // Every one of these is valid SQL the grammar does not cover. Guessing
        // where the clause goes would corrupt a statement the user wrote.
        for sql in [
            "SELECT * FROM t ORDER BY a NULLS FIRST",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t OFFSET 10 LIMIT 5",
        ] {
            assert!(
                order_by(Engine::Postgres, sql).is_none(),
                "{sql} should not be sortable"
            );
            assert!(
                with_order_by(Engine::Postgres, sql, &[]).is_none(),
                "{sql} was spliced"
            );
        }
    }

    #[test]
    fn only_a_query_takes_a_sort() {
        for sql in [
            "UPDATE t SET a = 1",
            "SELECT 1",
            "SELECT 1; SELECT 2",
            "BEGIN; SELECT 1; COMMIT",
            "-- nothing here",
        ] {
            assert!(
                order_by(Engine::Postgres, sql).is_none(),
                "{sql} should not be sortable"
            );
        }
    }

    #[test]
    fn a_plan_is_not_a_result_to_sort() {
        // The grammar flattens `EXPLAIN`'s payload into the statement, so the
        // explained query's `select` and `from` sit exactly where a sortable
        // statement's do. Left unguarded, a header click on the plan's one text
        // column spliced an `ORDER BY` into the query being explained -- which
        // both sorts nothing on screen and explains a different statement.
        for sql in [
            "EXPLAIN SELECT * FROM t",
            "EXPLAIN ANALYZE SELECT * FROM t",
            "explain analyze select id from accounts where x = 1",
            // Already carrying a sort of its own, which is the case where a
            // readout looks most convincingly like a sortable grid.
            "EXPLAIN ANALYZE SELECT * FROM t ORDER BY a",
        ] {
            assert!(
                order_by(Engine::Postgres, sql).is_none(),
                "{sql} reported a sort"
            );
            assert!(
                with_order_by(Engine::Postgres, sql, &[SortKey::new("a", true)]).is_none(),
                "{sql} was spliced"
            );
        }
    }

    #[test]
    fn grammar_loads() {
        assert_eq!(
            texts("SELECT 1;"),
            vec!["SELECT 1"],
            "SQL grammar failed to load"
        );
    }

    #[test]
    fn a_leading_comment_is_not_a_runnable_statement() {
        // Cursor at 0 in a buffer that opens with a header comment. Running the
        // comment returns an empty response with no error to explain it.
        let sql = "-- notes; about this\nSELECT 1;";
        let buffer = Buffer::parse(sql);

        assert_eq!(&sql[buffer.statement_at(0).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_trailing_comment_is_not_a_runnable_statement() {
        let sql = "SELECT 1; -- trailing";
        let buffer = Buffer::parse(sql);

        assert_eq!(&sql[buffer.statement_at(sql.len()).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_cursor_inside_a_gap_comment_selects_the_preceding_statement() {
        // The contract statement_at documents, which the block comment used to
        // win against by matching its own range.
        let sql = "SELECT 1;\n/* gap comment */\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let inside = sql.find("gap").unwrap();

        assert_eq!(&sql[buffer.statement_at(inside).unwrap()], "SELECT 1");
    }

    #[test]
    fn a_buffer_of_only_comments_has_nothing_to_run() {
        assert!(Buffer::parse("-- only a comment").statement_at(0).is_none());
        assert!(Buffer::parse(";;;").statement_at(0).is_none());
    }

    #[test]
    fn a_statement_the_grammar_has_never_heard_of_is_still_one() {
        // One dialect's grammar, four servers. None of these parse, all of
        // them are statements, and whether they are valid is the server's say.
        for sql in [
            "PRAGMA table_info(accounts);",
            "CALL refresh_totals();",
            "LISTEN jobs;",
            "USE dbdelve_dev",
        ] {
            let buffer = Buffer::parse(sql);
            let range = buffer.statement_at(0).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(&sql[range], sql.trim_end_matches(';').trim_end(), "{sql}");
        }
    }

    #[test]
    fn an_unread_statement_between_two_others_is_neither_of_them() {
        // It used to be glued onto the one before it, so running the first
        // line ran the second as well.
        assert_eq!(
            texts("SELECT 1;\nPRAGMA table_info(accounts);\nSELECT 2;"),
            vec!["SELECT 1", "PRAGMA table_info(accounts)", "SELECT 2"]
        );
    }

    #[test]
    fn a_semicolon_inside_unread_text_only_counts_where_it_separates() {
        // None of this parses, so the scan is all there is between a function
        // body and the server receiving half of it.
        let body = "CREATE PROCEDURE p() AS $fn$ BEGIN PERFORM 1; END $fn$";
        assert_eq!(texts(&format!("{body};\nCALL p()")), vec![body, "CALL p()"]);
        assert_eq!(
            texts("PRAGMA note = 'a;b'; -- c;d\nPRAGMA other"),
            vec!["PRAGMA note = 'a;b'", "-- c;d\nPRAGMA other"]
        );
        assert_eq!(
            texts("PRAGMA note = 1 /*/ ; */; PRAGMA other"),
            vec!["PRAGMA note = 1 /*/ ; */", "PRAGMA other"]
        );
    }

    #[test]
    fn a_statement_whose_opening_is_unread_is_sent_whole() {
        // The grammar reads this as an unknown `GRANT`, a `SELECT ON t`, and an
        // unknown `TO r`. It is one statement up to its `;`.
        assert_eq!(
            texts("GRANT SELECT ON t TO r;\nSELECT 2"),
            vec!["GRANT SELECT ON t TO r", "SELECT 2"]
        );
    }

    #[test]
    fn a_semicolon_in_a_comment_does_not_cut_a_tail_off_its_statement() {
        // The hazard the backwards merge exists for: the head of this alone is
        // an unqualified DELETE.
        let sql = "DELETE FROM t -- note; still the same statement\n WHERE !!! garbage";
        assert_eq!(texts(sql), vec![sql]);
    }

    #[test]
    fn a_non_ascii_character_in_unread_text_is_not_a_byte() {
        // The scanner walks bytes and slices characters. A bare `\u{e9}` used to
        // leave the cursor mid-character and panic the app rather than run
        // anything -- quoted it was always safe, since the quote is jumped whole.
        assert_eq!(texts("USE caf\u{e9}_db"), vec!["USE caf\u{e9}_db"]);
        assert_eq!(
            texts("CALL refresh_totals('caf\u{e9}')"),
            vec!["CALL refresh_totals('caf\u{e9}')"]
        );
        assert_eq!(
            texts("SELECT 1;\nUSE caf\u{e9}_db"),
            vec!["SELECT 1", "USE caf\u{e9}_db"]
        );
    }

    #[test]
    fn a_transaction_block_runs_as_one_statement() {
        assert_eq!(
            texts("BEGIN; SELECT 1; COMMIT;"),
            vec!["BEGIN; SELECT 1; COMMIT"]
        );
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(texts("SELECT 1;\nSELECT 2;"), vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn unterminated_final_statement_is_still_found() {
        assert_eq!(texts("SELECT 1;\nSELECT 2"), vec!["SELECT 1", "SELECT 2"]);
    }

    fn queued_texts<'a>(sql: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|r| &sql[r.clone()]).collect()
    }

    #[test]
    fn a_queue_is_every_statement_with_no_selection() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let ranges = queued_statements(Engine::Postgres, sql, None);
        assert_eq!(
            queued_texts(sql, &ranges),
            vec!["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn a_queue_includes_a_final_statement_with_no_trailing_semicolon() {
        let sql = "SELECT 1;\nSELECT 2";
        let ranges = queued_statements(Engine::Postgres, sql, None);
        assert_eq!(queued_texts(sql, &ranges), vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn a_queue_keeps_only_statements_the_selection_overlaps() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let second = sql.find("SELECT 2").unwrap();
        let third = sql.find("SELECT 3").unwrap();
        // Spans all of statement 2 and part of statement 3.
        let selection = second..third + "SELECT 3".len() / 2;
        let ranges = queued_statements(Engine::Postgres, sql, Some(selection));
        assert_eq!(queued_texts(sql, &ranges), vec!["SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn an_empty_selection_behaves_like_none() {
        let sql = "SELECT 1;\nSELECT 2;";
        let empty = 3..3;
        assert_eq!(
            queued_statements(Engine::Postgres, sql, Some(empty)),
            queued_statements(Engine::Postgres, sql, None)
        );
    }

    #[test]
    fn a_queue_on_sql_server_never_runs_the_go_line() {
        let sql = "SELECT 1;\nGO\nSELECT 2;";
        let ranges = queued_statements(Engine::SqlServer, sql, None);
        assert_eq!(queued_texts(sql, &ranges), vec!["SELECT 1;", "SELECT 2;"]);
    }

    /// A `GO` is a scope boundary and a semicolon is not, so cutting at the
    /// semicolon would send the `DECLARE` and the `SELECT` that reads it as
    /// two batches, and the second would not know the variable.
    #[test]
    fn a_sql_server_selection_runs_a_batch_at_a_time_not_a_statement() {
        let sql = "DECLARE @x int = 1;\nSELECT @x;";
        let ranges = queued_statements(Engine::SqlServer, sql, None);
        assert_eq!(queued_texts(sql, &ranges), vec![sql]);

        // The same text is two statements on an engine whose session carries
        // the declaration across submissions.
        assert_eq!(queued_statements(Engine::Postgres, sql, None).len(), 2);
    }

    fn split_texts(engine: Engine, sql: &str) -> Vec<&str> {
        let batches = queued_statements(engine, sql, Some(0..sql.len()));
        queued_texts(sql, &split_reads(engine, sql, &batches, 0..sql.len()))
    }

    #[test]
    fn a_sql_server_batch_of_plain_reads_splits_into_its_statements() {
        let sql = "SELECT 1 AS a;\nSELECT [b] FROM t WHERE n = '@x #y';";
        assert_eq!(
            split_texts(Engine::SqlServer, sql),
            vec!["SELECT 1 AS a", "SELECT [b] FROM t WHERE n = '@x #y'"]
        );
        assert_eq!(
            split_texts(
                Engine::SqlServer,
                "SELECT 1;\nGO\nSELECT 2; SELECT @@VERSION;"
            ),
            vec!["SELECT 1", "SELECT 2", "SELECT @@VERSION"]
        );

        // Only the statements the selection touches.
        let batches = queued_statements(Engine::SqlServer, sql, Some(0..3));
        assert_eq!(split_reads(Engine::SqlServer, sql, &batches, 0..3).len(), 1);

        assert!(split_texts(Engine::Postgres, "SELECT 1; SELECT 2;").is_empty());
    }

    /// Each of these needs the rest of its batch beside it, or is no plain
    /// read, so the batch is sent whole as it always was.
    #[test]
    fn a_sql_server_batch_with_anything_but_plain_reads_stays_whole() {
        for sql in [
            "DECLARE @x int = 1; SELECT @x;",
            "SELECT 1; SELECT @x;",
            "SELECT 1; SELECT * FROM #t;",
            "SELECT 1; SELECT * FROM ##t;",
            "SELECT 1; EXEC sp_who;",
            "SET NOCOUNT ON; SELECT 1;",
            "SELECT 1; IF 1 = 1 BEGIN SELECT 2; END",
            "BEGIN SELECT 1; SELECT 2; END",
            "SELECT 1;\nGO\nSELECT 2; SELECT @x;",
            "SELECT 1; UPDATE t SET a = 1;",
        ] {
            assert!(split_texts(Engine::SqlServer, sql).is_empty(), "{sql}");
        }
    }

    /// A batch is what SQL Server is sent, so a grid is reserved per statement
    /// inside it; everywhere else a submission is one statement and answers
    /// with at most one set.
    #[test]
    fn a_sql_server_batch_expects_a_result_set_per_statement_in_it() {
        let sql = "SELECT 1; SELECT 2;";
        assert_eq!(expected_sets(Engine::SqlServer, sql), 2);
        assert_eq!(expected_sets(Engine::Postgres, sql), 1);
        assert_eq!(expected_sets(Engine::SqlServer, "SELECT 1"), 1);
        assert_eq!(expected_sets(Engine::SqlServer, ""), 1);
    }

    #[test]
    fn a_counted_go_is_refused_rather_than_run_once() {
        let sql = "SELECT 1;\nGO 5\nSELECT 2;";
        assert!(batch_counts(Engine::SqlServer, sql).is_err());
        // A plain `GO` is a separator, not a repeat, and still runs.
        assert!(batch_counts(Engine::SqlServer, "SELECT 1;\nGO\nSELECT 2;").is_ok());
        // Nowhere else is `GO` anything but a name.
        assert!(batch_counts(Engine::Postgres, sql).is_ok());
    }

    #[test]
    fn semicolon_inside_a_string_literal_does_not_split() {
        // The case that breaks every naive splitter.
        assert_eq!(
            texts("SELECT ';' AS sep;\nSELECT 2;"),
            vec!["SELECT ';' AS sep", "SELECT 2"]
        );
    }

    fn tsql(sql: &str) -> Vec<&str> {
        Buffer::for_engine(Engine::SqlServer, sql)
            .statements()
            .iter()
            .map(|r| &sql[r.clone()])
            .collect()
    }

    #[test]
    fn a_t_sql_routine_runs_to_the_end_of_its_batch() {
        assert_eq!(
            tsql("CREATE PROCEDURE p AS\nSET NOCOUNT ON;\nUPDATE t SET x = 1 WHERE id = 2;"),
            ["CREATE PROCEDURE p AS\nSET NOCOUNT ON;\nUPDATE t SET x = 1 WHERE id = 2"]
        );
        for opening in [
            "CREATE OR ALTER FUNCTION",
            "ALTER PROC",
            "create trigger",
            "CREATE  OR  ALTER\nPROCEDURE",
        ] {
            let sql = format!("SELECT 0;\nGO\n{opening} r AS SELECT 1; SELECT 2;\nGO\nSELECT 3");
            let routine = format!("{opening} r AS SELECT 1; SELECT 2");
            assert_eq!(
                tsql(&sql),
                ["SELECT 0", routine.as_str(), "SELECT 3"],
                "{opening}"
            );
        }
    }

    #[test]
    fn a_t_sql_block_is_one_statement() {
        let cases: &[(&str, &[&str])] = &[
            (
                "IF 1 = 1 BEGIN DELETE FROM t WHERE id = 1; DELETE FROM u WHERE id = 2; END",
                &["IF 1 = 1 BEGIN DELETE FROM t WHERE id = 1; DELETE FROM u WHERE id = 2; END"],
            ),
            (
                "BEGIN TRY SELECT 1; SELECT 2; END TRY BEGIN CATCH SELECT 3; END CATCH; SELECT 4",
                &[
                    "BEGIN TRY SELECT 1; SELECT 2; END TRY BEGIN CATCH SELECT 3; END CATCH",
                    "SELECT 4",
                ],
            ),
            // Nested, with a `CASE … END` inside that closes nothing else.
            (
                "WHILE 1 = 1 BEGIN IF 1 = 1 BEGIN SELECT CASE WHEN 1 = 1 THEN 1 END; BREAK; END; \
                 SELECT 2; END; SELECT 3",
                &[
                    "WHILE 1 = 1 BEGIN IF 1 = 1 BEGIN SELECT CASE WHEN 1 = 1 THEN 1 END; BREAK; END; \
                     SELECT 2; END",
                    "SELECT 3",
                ],
            ),
            // Statements, not blocks.
            (
                "BEGIN TRAN; UPDATE t SET x = 1; COMMIT; SELECT 5",
                &["BEGIN TRAN; UPDATE t SET x = 1; COMMIT", "SELECT 5"],
            ),
            (
                "BEGIN DISTRIBUTED TRANSACTION; SELECT 1",
                &["BEGIN DISTRIBUTED TRANSACTION", "SELECT 1"],
            ),
            // Keywords inside strings, comments and quoted names are not keywords.
            (
                "SELECT 'BEGIN'; SELECT [begin], \"case\"; /* BEGIN */ SELECT 3; -- BEGIN\nSELECT 4",
                &[
                    "SELECT 'BEGIN'",
                    "SELECT [begin], \"case\"",
                    "SELECT 3",
                    "SELECT 4",
                ],
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(tsql(sql), *expected, "{sql}");
        }
    }

    #[test]
    fn a_t_sql_bracketed_name_is_quoted() {
        assert_eq!(tsql("SELECT [a;b] FROM t"), ["SELECT [a;b] FROM t"]);
        assert_eq!(
            tsql("SELECT [it's] FROM t; DELETE FROM u WHERE id = 1"),
            ["SELECT [it's] FROM t", "DELETE FROM u WHERE id = 1"]
        );
        assert_eq!(
            tsql("SELECT [x]]y;z], [ünï;code] FROM t; SELECT 2"),
            ["SELECT [x]]y;z], [ünï;code] FROM t", "SELECT 2"]
        );
    }

    #[test]
    fn a_go_line_separates_t_sql_batches_and_is_never_sent() {
        let cases: &[(&str, &[&str])] = &[
            ("SELECT 1\nGO", &["SELECT 1"]),
            ("SELECT 1;\nGO 5\nSELECT 2;", &["SELECT 1", "SELECT 2"]),
            (
                "SELECT 1\n  go  -- done\nSELECT 2",
                &["SELECT 1", "SELECT 2"],
            ),
            // Not alone on its line, or inside a string or a comment: not a `GO`.
            ("SELECT 1 GO", &["SELECT 1 GO"]),
            (
                "SELECT 'a\nGO\nb'; SELECT 2",
                &["SELECT 'a\nGO\nb'", "SELECT 2"],
            ),
            ("SELECT 1 /*\nGO\n*/", &["SELECT 1"]),
            // `/*/` opens a comment and does not close it.
            ("SELECT 1 /*/\nGO\n*/ AS x", &["SELECT 1 /*/\nGO\n*/ AS x"]),
            (
                "SELECT 1 /*/ BEGIN */; DELETE FROM t WHERE id = 1; SELECT 2;",
                &["SELECT 1", "DELETE FROM t WHERE id = 1", "SELECT 2"],
            ),
            ("SELECT 1\nGOTO x", &["SELECT 1\nGOTO x"]),
        ];
        for (sql, expected) in cases {
            assert_eq!(tsql(sql), *expected, "{sql:?}");
        }
    }

    #[test]
    fn a_selection_is_sent_as_its_one_batch() {
        assert_eq!(
            one_batch(Engine::SqlServer, "SELECT 1\nGO\n"),
            Ok("SELECT 1")
        );
        assert_eq!(one_batch(Engine::SqlServer, "GO\n"), Ok(""));
        assert_eq!(
            one_batch(Engine::SqlServer, "SELECT 1\nGO\n-- done\n"),
            Ok("SELECT 1")
        );
        assert!(one_batch(Engine::SqlServer, "SELECT 1\nGO\nSELECT 2").is_err());
        assert_eq!(
            one_batch(Engine::SqlServer, "INSERT INTO t DEFAULT VALUES\nGO 5"),
            Err("GO 5 repeats its batch, and dbdelve runs a batch once.".into())
        );
        assert_eq!(
            one_batch(Engine::SqlServer, "SELECT 1\nGO 1"),
            Ok("SELECT 1")
        );
        assert_eq!(
            one_batch(Engine::Postgres, "SELECT 1\nGO"),
            Ok("SELECT 1\nGO")
        );

        let sql = "INSERT INTO t DEFAULT VALUES;\nSELECT 1\nGO 3\nSELECT 2\nGO";
        assert!(batch_repeats(Engine::SqlServer, sql, 0).is_err());
        assert!(batch_repeats(Engine::SqlServer, sql, sql.find("SELECT 2").unwrap()).is_ok());
        assert!(batch_repeats(Engine::Postgres, sql, 0).is_ok());
    }

    #[test]
    fn only_sql_server_reads_t_sql_boundaries() {
        for sql in [
            "SELECT ARRAY['a;]'] FROM t; SELECT arr[1] FROM t",
            "SELECT [it's] FROM t; DELETE FROM u WHERE id = 1",
            "SELECT `a;b` FROM t; SELECT 2",
            "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT 1",
            "BEGIN SELECT 1; SELECT 2; END",
            "SELECT 1\nGO\nSELECT 2",
        ] {
            for engine in Engine::ALL
                .into_iter()
                .filter(|e| !matches!(e, Engine::SqlServer | Engine::MongoDb | Engine::Redis))
            {
                assert_eq!(
                    Buffer::for_engine(engine, sql).statements(),
                    Buffer::parse(sql).statements(),
                    "{engine:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn a_mongodb_buffer_runs_by_mongosh_statements_and_read_only_fails_closed() {
        let buffer =
            "db.accounts.find({ plan: 'free' })\n  .sort({ name: 1 })\ndb.accounts.deleteMany({})";
        let statements = queued_statements(Engine::MongoDb, buffer, None);
        assert_eq!(
            statements
                .iter()
                .map(|range| &buffer[range.clone()])
                .collect::<Vec<_>>(),
            [
                "db.accounts.find({ plan: 'free' })\n  .sort({ name: 1 })",
                "db.accounts.deleteMany({})"
            ]
        );

        let stopped =
            |statement: &str| gate(&classify(Engine::MongoDb, statement), Mode::ReadOnly, &[]);
        assert_eq!(stopped("db.accounts.find({ plan: 'free' })"), None);
        assert_eq!(
            stopped("db.accounts.insertOne({})"),
            Some(Stop::Upgrade(Mode::ReadWrite))
        );
        // Nothing on the server holds a Mongo session to reads, so what the
        // classifier cannot read is never run once in Read-only.
        for unreadable in [
            "db.accounts.find(",
            "db.runCommand({ eval: 'x' })",
            "SELECT 1",
        ] {
            assert_eq!(
                stopped(unreadable),
                Some(Stop::Upgrade(Mode::ReadWrite)),
                "{unreadable}"
            );
        }
    }

    #[test]
    fn dollar_quoted_body_does_not_split() {
        // Two semicolons live inside the function body. A `;` split would
        // produce four fragments, none of them runnable.
        let sql = "CREATE FUNCTION f() RETURNS int AS $$\n\
                   BEGIN\n\
                   RETURN 1;\n\
                   END;\n\
                   $$ LANGUAGE plpgsql;\n\
                   SELECT 1;";
        let found = texts(sql);
        assert_eq!(found.len(), 2, "body was split: {found:#?}");
        assert!(found[0].contains("RETURN 1;"));
        assert!(found[0].contains("END;"));
        assert_eq!(found[1], "SELECT 1");
    }

    #[test]
    fn cursor_inside_a_statement_selects_it() {
        let sql = "SELECT 1;\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let inside_second = sql.find("SELECT 2").unwrap() + 3;
        assert_eq!(
            &sql[buffer.statement_at(inside_second).unwrap()],
            "SELECT 2"
        );
    }

    #[test]
    fn cursor_just_after_a_semicolon_selects_that_statement() {
        let sql = "SELECT 1;\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let after_first = sql.find(';').unwrap() + 1;
        assert_eq!(&sql[buffer.statement_at(after_first).unwrap()], "SELECT 1");
    }

    #[test]
    fn cursor_in_the_gap_selects_the_preceding_statement() {
        let sql = "SELECT 1;\n\n\nSELECT 2;";
        let buffer = Buffer::parse(sql);
        let gap = sql.find("\n\n").unwrap() + 2;
        assert_eq!(&sql[buffer.statement_at(gap).unwrap()], "SELECT 1");
    }

    #[test]
    fn empty_and_whitespace_buffers_yield_nothing() {
        assert!(Buffer::parse("").statements().is_empty());
        assert!(Buffer::parse("   \n\t ").statements().is_empty());
        assert!(Buffer::parse("").statement_at(0).is_none());
    }

    #[test]
    fn incomplete_input_still_reports_something_runnable() {
        // Half-typed queries must not panic or wipe the statement list. The
        // dangling `FROM` is unparsable, so it stays with the statement it was
        // typed into and the server explains the problem.
        let sql = "SELECT * FROM";
        let buffer = Buffer::parse(sql);

        assert!(buffer.statement_at(3).is_some());
        assert_eq!(
            &sql[buffer.statement_at(sql.len()).unwrap()],
            "SELECT * FROM"
        );
    }

    #[test]
    fn an_unparsable_tail_stays_with_the_statement_it_was_typed_into() {
        // The head of a half-typed `DELETE ... WHERE` is an unqualified
        // DELETE. Sending it because the grammar could not read the tail is
        // the worst thing this module could do, so the tail comes along and
        // the server is what rejects it.
        for sql in [
            "DELETE FROM t WHERE ",
            "UPDATE t SET a = 1 WHERE ",
            "DELETE FROM t WHERE a = 'x",
            "SELECT 1;\nDELETE FROM t WHERE ",
            "GRANT SELECT ON t TO r",
            "SELECT 1 LIMIT 1",
        ] {
            let buffer = Buffer::parse(sql);
            let run = &sql[buffer.statement_at(sql.len()).unwrap()];
            assert!(
                sql.trim_end().ends_with(run),
                "{sql:?} was truncated to {run:?}"
            );
        }
    }

    #[test]
    fn a_whole_buffer_selection_is_found_without_its_semicolon() {
        let sql = "select * from accounts;\n";
        let found = Buffer::for_engine(Engine::Postgres, sql).find(Engine::Postgres, sql, 0, sql);
        assert_eq!(
            found.map(|range| &sql[range]),
            Some("select * from accounts")
        );
    }

    #[test]
    fn a_statement_that_is_not_a_query_takes_no_sort() {
        // A header click asks dbdelve to write an ORDER BY. Hard rule 1 says it
        // never writes a destructive statement, and the grammar gives `DELETE`
        // the same `from` child a `SELECT` has -- so the guard is the presence
        // of a `select`, not of a `from`.
        for sql in [
            "DELETE FROM t WHERE a = 1",
            "DELETE FROM t WHERE a = 1 RETURNING *",
            "UPDATE t SET a = 1",
            "TRUNCATE t",
            "INSERT INTO t (a) VALUES (1) RETURNING *",
        ] {
            assert!(
                order_by(Engine::Postgres, sql).is_none(),
                "{sql} reported a sort"
            );
            assert!(
                with_order_by(Engine::Postgres, sql, &[SortKey::new("a", true)]).is_none(),
                "{sql} was spliced"
            );
        }
    }

    #[test]
    fn a_splice_changes_nothing_but_the_clause() {
        // Collapsing whitespace across the whole statement rewrites string
        // literals, quoted identifiers and indentation -- all of which change
        // what the statement means or how it reads.
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                "SELECT * FROM t WHERE note LIKE 'a  %' LIMIT 10",
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            "SELECT * FROM t WHERE note LIKE 'a  %' ORDER BY id ASC LIMIT 10"
        );
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                r#"SELECT * FROM "public"."my  table""#,
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            r#"SELECT * FROM "public"."my  table" ORDER BY id ASC"#
        );
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                "SELECT *\nFROM t\nWHERE a = 1\n  AND b = 2",
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            "SELECT *\nFROM t\nWHERE a = 1\n  AND b = 2 ORDER BY id ASC"
        );
    }

    #[test]
    fn a_generated_update_sets_every_column_it_was_given() {
        // One column and several. A missing separator between assignments is a
        // statement the server rejects; a missing one in the WHERE would be a
        // statement it accepts and applies to the wrong rows.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", set("ok"))],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'ok' WHERE "id" = '7'"#
        );
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", set("ok")), ("depth", set("12"))],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'ok', "depth" = '12' WHERE "id" = '7'"#
        );
    }

    #[test]
    fn a_composite_key_matches_on_all_of_its_columns() {
        // Joined by OR, or with a column dropped, this updates rows the user
        // never edited.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "app",
                "memberships",
                &[("role", set("owner"))],
                &[("org_id", "1"), ("user_id", "2")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "app"."memberships" SET "role" = 'owner' WHERE "org_id" = '1' AND "user_id" = '2'"#
        );
    }

    #[test]
    fn user_data_is_quoted_rather_than_interpolated() {
        // An apostrophe in a value and a double quote in a column name are the
        // two ways a cell's contents become SQL of its own.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "s",
                "t",
                &[("a", set("it's"))],
                &[("id", "o'hara")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "s"."t" SET "a" = 'it''s' WHERE "id" = 'o''hara'"#
        );
        assert_eq!(
            update_row(
                Engine::Postgres,
                "s",
                r#"od"d"#,
                &[(r#"we"ird"#, set("x"))],
                &[("id", "1")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "s"."od""d" SET "we""ird" = 'x' WHERE "id" = '1'"#
        );
    }

    #[test]
    fn a_null_goes_in_as_the_keyword_and_never_as_a_quoted_word() {
        // `'NULL'` is a four-letter string and `NULL` is the absence of a
        // value. The whole worth of the gesture is that the two differ.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", NewValue::Null)],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = NULL WHERE "id" = '7'"#
        );
        // Mixed, on the engine whose identifier quote is its own: a NULL beside
        // a value must not disturb the separator between them.
        assert_eq!(
            update_row(
                Engine::MySql,
                "dbdelve_dev",
                "measurements",
                &[("note", NewValue::Null), ("depth", set("12"))],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            "UPDATE `dbdelve_dev`.`measurements` SET `note` = NULL, `depth` = '12' WHERE `id` = '7'"
        );
        // And the word itself, typed into a cell, is still a string.
        assert_eq!(
            update_row(
                Engine::Sqlite,
                "main",
                "measurements",
                &[("note", set("NULL"))],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "main"."measurements" SET "note" = 'NULL' WHERE "id" = '7'"#
        );
        // The gate is untouched by this: `SET x = NULL` is an `update` node
        // like any other, and a test here is what proves it rather than hopes.
        let statement = update_row(
            Engine::Postgres,
            "s",
            "t",
            &[("a", NewValue::Null)],
            &[("id", "1")],
            &[],
        )
        .unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn a_default_goes_in_as_the_keyword_and_the_typed_word_stays_a_string() {
        // The same distinction NULL is under, and the one that makes the menu
        // entry worth having: quoted, `DEFAULT` is seven characters of data.
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", NewValue::Default)],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = DEFAULT WHERE "id" = '7'"#
        );
        assert_eq!(
            update_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", set("DEFAULT"))],
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"UPDATE "public"."measurements" SET "note" = 'DEFAULT' WHERE "id" = '7'"#
        );
        // And the gate takes it, as it takes `SET x = NULL`.
        let statement = update_row(
            Engine::Postgres,
            "s",
            "t",
            &[("a", NewValue::Default)],
            &[("id", "1")],
            &[],
        )
        .unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn an_update_with_nothing_to_match_on_is_refused() {
        // No WHERE rewrites every row in the table. It must not be possible to
        // produce that statement, so a caller with no key gets nothing.
        assert!(update_row(Engine::Postgres, "s", "t", &[("a", set("1"))], &[], &[]).is_none());
        assert!(update_row(Engine::Postgres, "s", "t", &[], &[("id", "1")], &[]).is_none());
    }

    #[test]
    fn a_generated_insert_names_only_the_columns_it_was_given() {
        // The omission is the design: a column absent from this list is absent
        // from the statement, so the server's default applies to it.
        assert_eq!(
            insert_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("note", Some("ok")), ("depth", None)],
                &[]
            )
            .unwrap(),
            r#"INSERT INTO "public"."measurements" ("note", "depth") VALUES ('ok', NULL)"#
        );
        assert_eq!(
            insert_row(Engine::Sqlite, "main", "t", &[("a", Some("o'hara"))], &[]).unwrap(),
            r#"INSERT INTO "main"."t" ("a") VALUES ('o''hara')"#
        );
        // The engine whose identifier quote and literal escape are both its
        // own: a backtick doubles, and a backslash doubles before the
        // apostrophe after it does.
        assert_eq!(
            insert_row(
                Engine::MySql,
                "dbdelve_dev",
                "me`as",
                &[("no`te", Some(r"a\'b"))],
                &[]
            )
            .unwrap(),
            r"INSERT INTO `dbdelve_dev`.`me``as` (`no``te`) VALUES ('a\\''b')"
        );
        // An empty form is not `INSERT INTO t DEFAULT VALUES`, which is a
        // statement dbdelve has never been asked for.
        assert!(insert_row(Engine::Postgres, "s", "t", &[], &[]).is_err());
    }

    #[test]
    fn an_insert_template_numbers_or_marks_its_placeholders_the_engines_way() {
        let columns = [column("id", None), column("email", None)];
        assert_eq!(
            insert_template(Engine::Postgres, "public", "users", &columns).unwrap(),
            r#"INSERT INTO "public"."users" ("id", "email") VALUES ($1, $2);"#
        );
        assert_eq!(
            insert_template(Engine::MySql, "app", "users", &columns).unwrap(),
            "INSERT INTO `app`.`users` (`id`, `email`) VALUES (?, ?);"
        );
        assert_eq!(
            insert_template(Engine::SqlServer, "dbo", "users", &columns).unwrap(),
            r#"INSERT INTO "dbo"."users" ("id", "email") VALUES (@p1, @p2);"#
        );
        assert_eq!(
            insert_template(Engine::MongoDb, "db", "users", &columns),
            None
        );
    }

    fn column(name: &str, default: Option<&str>) -> ColumnDefinition {
        ColumnDefinition {
            name: name.into(),
            data_type: "integer".into(),
            nullable: true,
            default: default.map(Into::into),
        }
    }

    #[test]
    fn an_insert_template_leaves_out_what_the_server_generates() {
        let columns = [
            column("id", Some("GENERATED ALWAYS AS IDENTITY")),
            column("total", Some("GENERATED ALWAYS AS ((price * qty)) STORED")),
            column("seq", Some("GENERATED BY DEFAULT AS IDENTITY")),
            column("created_at", Some("now()")),
        ];
        assert_eq!(
            insert_template(Engine::Postgres, "public", "orders", &columns).unwrap(),
            r#"INSERT INTO "public"."orders" ("seq", "created_at") VALUES ($1, $2);"#
        );
        let columns = [
            column("id", Some("IDENTITY(1,1)")),
            column("total", Some("AS ([price]*[qty]) PERSISTED")),
            column("created_at", Some("(getdate())")),
        ];
        assert_eq!(
            insert_template(Engine::SqlServer, "dbo", "orders", &columns).unwrap(),
            r#"INSERT INTO "dbo"."orders" ("created_at") VALUES (@p1);"#
        );
        assert_eq!(
            insert_template(Engine::SqlServer, "dbo", "orders", &columns[..2]),
            None
        );
    }

    #[test]
    fn the_gate_admits_an_insert_and_still_admits_an_update() {
        assert!(is_generated_write(
            r#"INSERT INTO "public"."t" ("a") VALUES ('1')"#
        ));
        assert!(is_generated_write("UPDATE t SET a = '1' WHERE id = '2'"));
        assert!(is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\n\
             UPDATE t SET a = '3' WHERE id = '4';\nCOMMIT;"
        ));
        // And what the generator writes, which is the test that keeps the two
        // from drifting apart.
        let statement = insert_row(
            Engine::Postgres,
            "public",
            "measurements",
            &[("note", Some("it's fine")), ("depth", None)],
            &[],
        )
        .unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn the_gate_refuses_an_insert_carrying_something_else() {
        // One insert, alone. A batch of them is a shape nothing generates, and
        // a `DELETE` riding along in a CTE is the shape an injected value takes.
        for sql in [
            "INSERT INTO t (a) VALUES ('1'); DROP TABLE t",
            "INSERT INTO t (a) VALUES ('1'); TRUNCATE t",
            "WITH x AS (DELETE FROM t RETURNING *) INSERT INTO u (a) VALUES ('1')",
            "INSERT INTO t (a) VALUES ('1'); INSERT INTO t (a) VALUES ('2')",
        ] {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_admits_a_multi_row_insert_of_literals() {
        for sql in [
            "INSERT INTO \"public\".\"t\" (\"a\", \"b\") VALUES ('1', NULL),\n('2', 'x')",
            "INSERT INTO \"dbo\".\"t\" (\"a\", \"b\") VALUES (N'é', 0xAB),\n('2024-01-02T03:04:05.000', NULL)",
            "INSERT INTO `d`.`t` (`a`) VALUES ('1') ON DUPLICATE KEY UPDATE `a` = `a`",
            "INSERT INTO \"main\".\"t\" (\"a\") VALUES ('1') ON CONFLICT DO NOTHING",
        ] {
            assert!(is_generated_write(sql), "{sql} was refused");
        }
    }

    #[test]
    fn the_gate_refuses_an_insert_whose_values_are_not_literals() {
        for sql in [
            "INSERT INTO t (a) VALUES ((SELECT 1))",
            "INSERT INTO t (a) VALUES (now())",
            "INSERT INTO t (a) SELECT '1'",
            "INSERT INTO t (a) VALUES ('1' || '2')",
            "INSERT INTO t (a) VALUES (b)",
            "INSERT INTO t (a) VALUES (\"b\")",
            "INSERT INTO t (a) VALUES (1)",
            "INSERT INTO t VALUES ('1')",
            "INSERT INTO t (a) VALUES ('1') RETURNING *",
            "INSERT INTO t (a) DEFAULT VALUES",
            "REPLACE INTO t (a) VALUES ('1')",
            "INSERT OR REPLACE INTO t (a) VALUES ('1')",
            "INSERT IGNORE INTO t (a) VALUES ('1')",
        ] {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_conflict_clause_other_than_the_fixed_form() {
        for tail in [
            "ON CONFLICT (a) DO NOTHING",
            "ON CONFLICT DO UPDATE SET a = '1'",
            "ON DUPLICATE KEY UPDATE a = b",
            "ON DUPLICATE KEY UPDATE a = '1'",
            "ON DUPLICATE KEY UPDATE a = a, b = b",
            "ON DUPLICATE KEY UPDATE a = VALUES(a)",
        ] {
            let sql = format!("INSERT INTO t (a) VALUES ('1') {tail}");
            assert!(!is_generated_write(&sql), "{sql} passed the gate");
        }
    }

    const SQL_ENGINES: [Engine; 8] = [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::Turso,
        Engine::DuckDb,
        Engine::Snowflake,
        Engine::SqlServer,
    ];

    #[test]
    fn the_gate_refuses_a_comment_anywhere_in_an_insert() {
        // MySQL runs a `/*! … */` comment as SQL, so a comment is never inert.
        for sql in [
            "INSERT INTO d/**/.t (a) VALUES ('1')",
            "INSERT INTO d/*!.u*/.t (a) VALUES ('1')",
            "INSERT INTO `d`/*! .u */.`t` (`a`) VALUES ('1')",
            "INSERT INTO d -- x\n.t (a) VALUES ('1')",
            "INSERT INTO d.t/**/ (a) VALUES ('1')",
            "INSERT /**/ INTO t (a) VALUES ('1')",
            "INSERT INTO t (\"a\"/**/) VALUES ('1')",
            "INSERT INTO t (/**/\"a\") VALUES ('1')",
            "INSERT INTO t (`a`/*!, b*/) VALUES ('1')",
            "INSERT INTO t (a) VALUES /**/ ('1')",
            "INSERT INTO t (a) VALUES ('1'/**/)",
            "INSERT INTO t (a) VALUES (/*!0x41*/'1')",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE `a`/**/ = `a`",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE `a` = /*!b*/`a`",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE a = a/*!, b = '1'*/",
            "INSERT INTO t (a) VALUES ('1') ON CONFLICT DO NOTHING/**/",
            "INSERT INTO t (a) VALUES ('1')/**/",
            "INSERT INTO t (a) VALUES ('1') -- x",
        ] {
            for engine in SQL_ENGINES {
                assert!(
                    !is_generated_write_on(engine, sql),
                    "{engine:?}: {sql} passed the gate"
                );
            }
        }
    }

    #[test]
    fn a_duplicate_key_skip_names_its_column_as_mysql_reads_a_name() {
        // `"a"` is a string under MySQL's default sql_mode, so `a = "a"`
        // assigns one.
        for sql in [
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE \"a\" = \"a\"",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE a = \"a\"",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE `a` = \"a\"",
        ] {
            for engine in SQL_ENGINES {
                assert!(
                    !is_generated_write_on(engine, sql),
                    "{engine:?}: {sql} passed the gate"
                );
            }
        }
        for sql in [
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE `a` = `a`",
            "INSERT INTO t (a) VALUES ('1') ON DUPLICATE KEY UPDATE a = a",
        ] {
            for engine in [Engine::MySql, Engine::MariaDb] {
                assert!(is_generated_write_on(engine, sql), "{engine:?}: {sql}");
            }
        }
    }

    #[test]
    fn the_gate_refuses_a_value_that_is_more_than_one_literal() {
        for sql in [
            "INSERT INTO t (a) VALUES ('1' '2')",
            "INSERT INTO t (a) VALUES ('1'\n'2')",
            "INSERT INTO t (a) VALUES (N'1' '2')",
            "INSERT INTO t (a) VALUES ('1' N'2')",
            "INSERT INTO t (a) VALUES ('1''2' '3')",
            "INSERT INTO t (a) VALUES (0x)",
            "INSERT INTO t (a) VALUES (0xAB 'c')",
            "INSERT INTO t (a) VALUES (NULL '1')",
            "DELETE FROM t WHERE id = '1' '2'",
        ] {
            for engine in SQL_ENGINES {
                assert!(
                    !is_generated_write_on(engine, sql),
                    "{engine:?}: {sql} passed the gate"
                );
            }
        }
    }

    #[test]
    fn every_literal_an_import_writes_passes_the_write_gate() {
        use crate::import_rows::Value;
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Number("-1.5e3".into()),
            Value::Text(String::new()),
            Value::Text("it's".into()),
            Value::Text("''".into()),
            Value::Text("a\\b\\".into()),
            Value::Text("\\'".into()),
            Value::Text("é /* -- */ ;".into()),
            Value::Text("0xAB".into()),
            Value::Json(serde_json::from_str(r#"{"a": "b'c"}"#).unwrap()),
        ];
        let types = [
            "text",
            "varchar(10)",
            "nvarchar(10)",
            "integer",
            "varbinary(10)",
            "datetime",
            "datetime2",
            "bit",
        ];
        for engine in SQL_ENGINES {
            for data_type in types {
                let rows: Vec<Vec<Option<&Value>>> =
                    values.iter().map(|value| vec![Some(value)]).collect();
                for skip in [false, engine.skips_duplicates()] {
                    let statement =
                        insert_rows(engine, "s", "t", &[("a", data_type)], &rows, skip).unwrap();
                    assert!(
                        is_generated_write_on(engine, &statement),
                        "{engine:?} {data_type}: {statement}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_gate_admits_a_lone_transaction_bracket() {
        for sql in [
            "BEGIN",
            "BEGIN;",
            "BEGIN TRANSACTION",
            "COMMIT",
            "COMMIT;",
            "ROLLBACK",
            " ROLLBACK; ",
        ] {
            assert!(is_generated_write(sql), "{sql:?} was refused");
        }
    }

    #[test]
    fn the_gate_refuses_a_bracket_beside_anything_else() {
        for sql in [
            "BEGIN; INSERT INTO t (a) VALUES ('1')",
            "INSERT INTO t (a) VALUES ('1'); COMMIT",
            "COMMIT; DROP TABLE t",
            "ROLLBACK; COMMIT",
            "BEGIN; COMMIT",
            "begin",
            "START TRANSACTION",
            "ROLLBACK TO SAVEPOINT s",
            "COMMIT AND CHAIN",
            "BEGIN WORK",
            "COMMIT;;",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_quote_a_backslash_escapes_where_the_engine_reads_one() {
        // The grammar ends each of these strings at the second `'`; MySQL and
        // Snowflake read `\'` as a quote inside it, and the rest as SQL.
        let probes = [
            r"INSERT INTO t (a, b) VALUES ('a\', ', (SELECT password FROM users LIMIT 1)) # ')",
            r"INSERT INTO t (a, b) VALUES ('a\', ', sleep(5)) -- ')",
            r"DELETE FROM t WHERE id = 'a\' AND b = ' OR 1=1 # '",
            r"UPDATE t SET a = 'x\', b = ' WHERE 1=1 -- ' WHERE id = '1'",
            r"UPDATE t SET a = 'x\', b = ' WHERE 1=1; DROP TABLE t; -- ' WHERE id = '1'",
            r"UPDATE t SET a = '1' WHERE id = 'a\' OR b = ' OR 1=1 -- '",
            r#"UPDATE t SET a = "x\", b = " WHERE 1=1 -- " WHERE id = '1'"#,
        ];
        for sql in probes {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
        for engine in [Engine::MySql, Engine::MariaDb, Engine::Snowflake] {
            for sql in probes {
                assert!(
                    !is_generated_write_on(engine, sql),
                    "{sql} passed the gate on {engine:?}"
                );
            }
            assert!(!delete_matches_key_on(engine, probes[2], &["id", "b"]));
            // What `quote_literal` writes there still reads as one literal.
            for value in [r"a\", "it's", r"it\'s", r"\\", r"\", r#"a\"b"#] {
                let literal = engine.quote_literal(value);
                for sql in [
                    format!("INSERT INTO t (a) VALUES ({literal})"),
                    format!("DELETE FROM t WHERE id = {literal}"),
                    format!("UPDATE t SET a = {literal} WHERE id = {literal}"),
                ] {
                    assert!(
                        is_generated_write_on(engine, &sql),
                        "{sql} was refused on {engine:?}"
                    );
                }
            }
        }
        // A backslash is an ordinary character where the engine says so.
        for engine in [Engine::Postgres, Engine::Sqlite, Engine::SqlServer] {
            for sql in [
                r"INSERT INTO t (a) VALUES ('C:\')",
                r"DELETE FROM t WHERE id = 'C:\'",
                r"UPDATE t SET a = 'C:\' WHERE id = 'C:\'",
            ] {
                assert!(
                    is_generated_write_on(engine, sql),
                    "{sql} was refused on {engine:?}"
                );
            }
            assert!(delete_matches_key_on(
                engine,
                r"DELETE FROM t WHERE id = 'C:\'",
                &["id"]
            ));
        }
    }

    #[test]
    fn the_gate_still_refuses_a_delete_matching_null() {
        // `= NULL` matches no row, and NULL is a value only an insert writes.
        assert!(!is_generated_write("DELETE FROM t WHERE id = NULL"));
        assert!(!is_generated_write(
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t WHERE id = NULL"
        ));
    }

    #[test]
    fn the_gate_accepts_an_update_and_a_batch_of_updates() {
        assert!(is_generated_write("UPDATE t SET a = '1' WHERE id = '2'"));
        assert!(is_generated_write(
            "UPDATE t SET a = '1' WHERE id = '2'; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_accepts_a_batch_bracketed_by_a_transaction() {
        // What dbdelve writes for an engine that commits each statement on its
        // own. The brackets are part of the generated statement, so the gate
        // has to know the shape or it would refuse dbdelve's own output.
        assert!(is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\n\
             UPDATE t SET a = '3' WHERE id = '4';\nCOMMIT;"
        ));
    }

    #[test]
    fn the_gate_refuses_a_transaction_it_does_not_see_closed() {
        // A BEGIN whose COMMIT went missing leaves the session holding an open
        // transaction the user never wrote, which is worse than not applying
        // the edit at all.
        for sql in [
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';",
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\nROLLBACK;",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_destructive_statement_inside_the_brackets() {
        // Seeing through the transaction must not mean trusting what is in it.
        assert!(!is_generated_write(
            "BEGIN;\nUPDATE t SET a = '1' WHERE id = '2';\nDELETE FROM t;\nCOMMIT;"
        ));
    }

    #[test]
    fn the_gate_refuses_everything_that_is_not_a_write_dbdelve_writes() {
        // Hard rule 1 in code: DROP and TRUNCATE never leave dbdelve, whatever
        // the user asked for. SELECT is here because the gate is a whitelist --
        // being harmless is not the test, being one of the shapes dbdelve
        // generates is. A keyed DELETE is no longer in this list because it is
        // one of those shapes; `delete_matches_key` is what asks whether the key
        // it names is the row's.
        for sql in [
            "DROP TABLE t",
            "DROP VIEW v",
            "DROP DATABASE d",
            "TRUNCATE t",
            "TRUNCATE TABLE t",
            "SELECT 1",
        ] {
            assert!(!is_generated_write(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn the_gate_refuses_a_batch_with_one_destructive_statement_in_it() {
        // Every statement is checked, not the first one. A DELETE appended to a
        // run of legitimate updates is the shape an injected value would take.
        assert!(!is_generated_write(
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t; UPDATE t SET a = '3' WHERE id = '4'"
        ));
    }

    #[test]
    fn the_gate_refuses_a_destructive_statement_wrapped_in_a_cte() {
        // The root statement's first child here really is an `update` node, so
        // the whitelist passes it and only the subtree scan catches it.
        assert!(!is_generated_write(
            "WITH x AS (DELETE FROM t RETURNING *) UPDATE u SET a = '1' WHERE id = '2'"
        ));
    }

    #[test]
    fn the_gate_refuses_what_the_grammar_cannot_read_whole() {
        // An unreadable tree says nothing about what the statement does, and a
        // gate that cannot see has to refuse. The empty buffer is here because
        // it parses cleanly into no statements at all.
        for sql in [
            "not sql at all !!",
            "UPDATE t SET a = ",
            "-- UPDATE t SET a = '1'",
            "",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn the_gate_accepts_what_update_row_writes() {
        // The one test that keeps the generator and the gate from drifting
        // apart: whatever quoting or clause order changes here, the statement
        // dbdelve builds is still one the gate can read as an UPDATE.
        let statement = update_row(
            Engine::Postgres,
            "public",
            "measurements",
            &[("note", set("it's fine")), ("depth", set("12"))],
            &[("id", "7"), ("run", "a'b")],
            &[],
        )
        .unwrap();

        assert!(is_generated_write(&statement), "{statement} was refused");
        assert!(is_generated_write(&format!("{statement}; {statement}")));
    }

    #[test]
    fn a_cte_still_takes_a_sort() {
        // `WITH` puts the outer SELECT and its FROM at the top level, beside
        // the cte. The select-child guard must not read the cte's own.
        assert_eq!(
            with_order_by(
                Engine::Postgres,
                "WITH x AS (SELECT 1 AS a) SELECT * FROM x",
                &[SortKey::new("a", true)]
            )
            .unwrap(),
            "WITH x AS (SELECT 1 AS a) SELECT * FROM x ORDER BY a ASC"
        );
    }

    #[test]
    fn the_select_gate_accepts_the_shape_a_preview_has() {
        for sql in [
            r#"SELECT * FROM "public"."accounts" LIMIT 1000"#,
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' LIMIT 1000"#,
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' ORDER BY "id" ASC LIMIT 100 OFFSET 200"#,
            "SELECT * FROM `dbdelve_dev`.`accounts` WHERE `state` = 'ok' LIMIT 100",
            r#"WITH x AS (SELECT 1 AS a) SELECT * FROM x LIMIT 10"#,
        ] {
            assert!(
                is_generated_select(Engine::Postgres, sql),
                "{sql} was refused"
            );
        }
    }

    #[test]
    fn the_select_gate_refuses_a_filter_carrying_a_second_statement() {
        // The reason this gate exists. Whether each is refused for having two
        // roots or for not parsing is not the point -- refused is the point.
        for sql in [
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; DROP TABLE "t" LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1' LIMIT 1000; DROP TABLE "t""#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; DELETE FROM "t" LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = '1'; TRUNCATE "t" LIMIT 1000"#,
        ] {
            assert!(
                !is_generated_select(Engine::Postgres, sql),
                "{sql} passed the gate"
            );
        }
    }

    #[test]
    fn the_select_gate_refuses_a_filter_that_does_not_parse() {
        for sql in [
            r#"SELECT * FROM "public"."t" WHERE "id" = ((( LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE "id" = 'unclosed LIMIT 1000"#,
            r#"SELECT * FROM "public"."t" WHERE LIMIT 1000"#,
            "",
        ] {
            assert!(
                !is_generated_select(Engine::Postgres, sql),
                "{sql:?} passed the gate"
            );
        }
    }

    #[test]
    fn the_select_gate_refuses_a_destructive_statement_hidden_in_a_cte() {
        // The root's first named child here is a `select`, so the whitelist
        // alone passes it and only the recursive scan catches it. THIS TEST IS
        // LOAD-BEARING: a later task splits `destructive` apart for the DELETE
        // path, and this is what fails if the SELECT gate is not updated too.
        for sql in [
            r#"WITH x AS (DELETE FROM "t" RETURNING *) SELECT * FROM x LIMIT 1000"#,
            r#"WITH x AS (SELECT 1 AS a) SELECT * FROM x WHERE a IN (SELECT 1); DROP TABLE "t""#,
        ] {
            assert!(
                !is_generated_select(Engine::Postgres, sql),
                "{sql} passed the gate"
            );
        }
    }

    #[test]
    fn the_select_gate_admits_no_write_at_all() {
        // A whitelist, not a blocklist: being harmless is not the test, being
        // a SELECT is.
        for sql in [
            "UPDATE t SET a = '1' WHERE id = '2'",
            "INSERT INTO t (a) VALUES ('1')",
            "DELETE FROM t WHERE a = '1'",
            "DROP TABLE t",
            "TRUNCATE t",
            "BEGIN; SELECT 1; COMMIT",
            "SELECT 1; SELECT 2",
            "-- SELECT * FROM t",
        ] {
            assert!(
                !is_generated_select(Engine::Postgres, sql),
                "{sql} passed the gate"
            );
        }
    }

    #[test]
    fn a_generated_delete_names_the_row_and_only_the_row() {
        assert_eq!(
            delete_row(
                Engine::Postgres,
                "public",
                "measurements",
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            r#"DELETE FROM "public"."measurements" WHERE "id" = '7'"#
        );
        // Joined by OR, or with a column dropped, this deletes rows the user
        // never pointed at.
        assert_eq!(
            delete_row(
                Engine::Postgres,
                "app",
                "memberships",
                &[("org_id", "1"), ("user_id", "2")],
                &[]
            )
            .unwrap(),
            r#"DELETE FROM "app"."memberships" WHERE "org_id" = '1' AND "user_id" = '2'"#
        );
        assert_eq!(
            delete_row(
                Engine::MySql,
                "dbdelve_demo",
                "measurements",
                &[("id", "7")],
                &[]
            )
            .unwrap(),
            "DELETE FROM `dbdelve_demo`.`measurements` WHERE `id` = '7'"
        );
        assert_eq!(
            delete_row(Engine::Sqlite, "main", "t", &[("id", "o'hara")], &[]).unwrap(),
            r#"DELETE FROM "main"."t" WHERE "id" = 'o''hara'"#
        );
        // No WHERE empties the table, so it must not be possible to produce.
        assert!(delete_row(Engine::Postgres, "s", "t", &[], &[]).is_none());
    }

    #[test]
    fn the_gate_admits_the_delete_dbdelve_writes_and_reads_its_key_back() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let statement = delete_row(engine, "s", "t", &[("id", "7")], &[]).unwrap();
            assert!(is_generated_write(&statement), "{statement} was refused");
            assert!(delete_matches_key(&statement, &["id"]), "{statement}");

            let composite =
                delete_row(engine, "s", "t", &[("org_id", "1"), ("user_id", "2")], &[]).unwrap();
            assert!(is_generated_write(&composite), "{composite} was refused");
            assert!(delete_matches_key(&composite, &["org_id", "user_id"]));
            // Set equality: the key is a set of columns, not a sequence.
            assert!(delete_matches_key(&composite, &["user_id", "org_id"]));
        }
        // A value carrying the quote character still reads back.
        let statement = delete_row(Engine::Postgres, "s", "t", &[("id", "o'hara")], &[]).unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
        assert!(delete_matches_key(&statement, &["id"]));

        // A column name carrying one does not: `"we""ird"` is two adjacent
        // strings to this grammar and the whole statement fails to parse, so
        // the gate refuses dbdelve's own output. That is the safe direction --
        // the row stays -- and a gate that guessed past an unreadable tree is
        // the unsafe one.
        let odd = delete_row(Engine::Postgres, "s", "t", &[(r#"we"ird"#, "x")], &[]).unwrap();
        assert!(!is_generated_write(&odd), "{odd} passed the gate");
    }

    #[test]
    fn sql_server_writes_a_key_the_way_its_column_type_compares() {
        let types = [
            ("hash".to_string(), "binary".to_string()),
            ("code".to_string(), "varchar".to_string()),
            ("name".to_string(), "nvarchar".to_string()),
        ];
        let delete = delete_row(
            Engine::SqlServer,
            "dbo",
            "t",
            &[("hash", "0x00FF"), ("code", "a'b")],
            &types,
        )
        .unwrap();
        assert_eq!(
            delete,
            r#"DELETE FROM "dbo"."t" WHERE "hash" = 0x00FF AND "code" = 'a''b'"#
        );
        assert!(is_generated_write(&delete), "{delete} was refused");
        assert!(delete_matches_key(&delete, &["hash", "code"]));
        assert_eq!(classify(Engine::SqlServer, &delete), Verdict::WRITE);

        let update = update_row(
            Engine::SqlServer,
            "dbo",
            "t",
            &[("name", set("李")), ("code", set("x"))],
            &[("hash", "0x00FF")],
            &types,
        )
        .unwrap();
        assert_eq!(
            update,
            r#"UPDATE "dbo"."t" SET "name" = N'李', "code" = 'x' WHERE "hash" = 0x00FF"#
        );
        assert!(is_generated_write(&update), "{update} was refused");
        assert_eq!(classify(Engine::SqlServer, &update), Verdict::WRITE);

        assert_eq!(
            insert_row(
                Engine::SqlServer,
                "dbo",
                "t",
                &[("hash", Some("0xAB"))],
                &types
            )
            .unwrap(),
            r#"INSERT INTO "dbo"."t" ("hash") VALUES (0xAB)"#
        );

        // A value that is not exactly a hex literal stays quoted, and a bare
        // number that is not one is no key the gate reads.
        let odd = delete_row(
            Engine::SqlServer,
            "dbo",
            "t",
            &[("hash", "0x1 OR 1=1")],
            &types,
        )
        .unwrap();
        assert!(odd.ends_with(r#""hash" = N'0x1 OR 1=1'"#), "{odd}");
        assert!(!is_generated_write(r#"DELETE FROM t WHERE "id" = 7"#));
        assert!(!is_generated_write(r#"DELETE FROM t WHERE "id" = 0x"#));
    }

    #[test]
    fn the_gate_refuses_every_delete_that_is_not_one_named_row() {
        for sql in [
            "DELETE FROM t",
            r#"DELETE FROM "public"."t""#,
            r#"DELETE FROM t WHERE "id" = '1' OR "id" = '2'"#,
            r#"DELETE FROM t WHERE "id" = '1' AND ("a" = '2' OR "b" = '3')"#,
            r#"DELETE FROM t WHERE "id" > '1'"#,
            r#"DELETE FROM t WHERE "id" <> '1'"#,
            r#"DELETE FROM t WHERE "id" LIKE '1%'"#,
            r#"DELETE FROM t WHERE "id" IS NULL"#,
            "DELETE FROM t WHERE id IN (SELECT id FROM u)",
            "DELETE FROM t WHERE id = lower('a')",
            "WITH x AS (SELECT 1) DELETE FROM t WHERE id = '1'",
            "DELETE FROM t WHERE id = '1' LIMIT 1",
            "DELETE FROM t WHERE id = '1' RETURNING *",
            "DELETE FROM t WHERE id = '1'; DELETE FROM t WHERE id = '2'",
            "DELETE FROM t WHERE id = '1'; DROP TABLE t",
            "UPDATE t SET a = '1' WHERE id = '2'; DELETE FROM t WHERE id = '3'",
            r#"DELETE FROM t WHERE "id" = "other""#,
            "DELETE FROM t WHERE t.id = '1'",
            "WITH x AS (DROP TABLE u) DELETE FROM t WHERE id = '1'",
            "WITH x AS (DROP TABLE u) UPDATE t SET a = '1' WHERE id = '2'",
        ] {
            assert!(!is_generated_write(sql), "{sql:?} passed the gate");
        }
    }

    #[test]
    fn a_delete_keyed_on_the_wrong_columns_is_refused_by_the_key_check() {
        // These are the right SHAPE -- is_generated_write admits the first two,
        // and must, since it has no key to compare against. delete_matches_key
        // is what refuses them, and a caller runs both.
        let wrong_column = r#"DELETE FROM "s"."t" WHERE "note" = 'x'"#;
        assert!(is_generated_write(wrong_column));
        assert!(!delete_matches_key(wrong_column, &["id"]));

        let half = r#"DELETE FROM "s"."t" WHERE "org_id" = '1'"#;
        assert!(is_generated_write(half));
        assert!(!delete_matches_key(half, &["org_id", "user_id"]));

        // A column named twice would read as a one-column key. This one the
        // shape check itself refuses, and the readout agrees.
        let twice = r#"DELETE FROM "s"."t" WHERE "id" = '1' AND "id" = '2'"#;
        assert!(!is_generated_write(twice));
        assert!(!delete_matches_key(twice, &["id"]));

        // And a statement that is not a delete at all answers no here too.
        assert!(!delete_matches_key(
            r#"UPDATE "s"."t" SET "a" = '1' WHERE "id" = '2'"#,
            &["id"]
        ));
        assert!(!delete_matches_key("DROP TABLE t", &["id"]));
    }

    fn set(value: &str) -> NewValue {
        NewValue::Value(value.into())
    }

    fn pending_row(sets: &[(&str, NewValue)], keys: &[(&str, &str)]) -> PendingRow {
        fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(column, value)| (column.to_string(), value.to_string()))
                .collect()
        }
        PendingRow {
            schema: "public".to_string(),
            table: "accounts".to_string(),
            sets: sets
                .iter()
                .map(|(column, value)| (column.to_string(), value.clone()))
                .collect(),
            keys: owned(keys),
            types: Vec::new(),
            held: Vec::new(),
        }
    }

    #[test]
    fn a_nulled_cell_reaches_the_batch_as_the_keyword() {
        let rows = vec![pending_row(&[("name", NewValue::Null)], &[("id", "1")])];
        assert_eq!(
            update_batch(Engine::Postgres, &rows).unwrap(),
            "UPDATE \"public\".\"accounts\" SET \"name\" = NULL WHERE \"id\" = '1';"
        );
    }

    #[test]
    fn several_pending_rows_become_one_semicolon_joined_batch() {
        let rows = vec![
            pending_row(&[("name", set("Ada"))], &[("id", "1")]),
            pending_row(&[("name", set("Bo"))], &[("id", "2")]),
        ];

        let batch = update_batch(Engine::Postgres, &rows).unwrap();
        assert_eq!(
            batch,
            "UPDATE \"public\".\"accounts\" SET \"name\" = 'Ada' WHERE \"id\" = '1';\n\
             UPDATE \"public\".\"accounts\" SET \"name\" = 'Bo' WHERE \"id\" = '2';"
        );
        // The batch dbdelve builds has to pass the same gate dbdelve checks every
        // generated statement against, or the generator and the gate have
        // drifted apart.
        assert!(is_generated_write(&batch));
    }

    #[test]
    fn an_engine_without_an_implicit_transaction_gets_explicit_brackets() {
        // MySQL and SQLite commit each statement on its own, so an unbracketed
        // batch could apply half the user's edits and report the failure of the
        // rest.
        let rows = vec![
            pending_row(&[("name", set("Ada"))], &[("id", "1")]),
            pending_row(&[("name", set("Bo"))], &[("id", "2")]),
        ];

        for engine in [Engine::MySql, Engine::Sqlite] {
            let batch = update_batch(engine, &rows).unwrap();
            assert!(batch.starts_with("BEGIN;\n"), "{engine:?} {batch}");
            assert!(batch.ends_with("\nCOMMIT;"), "{engine:?} {batch}");
            assert!(is_generated_write(&batch), "{engine:?} {batch}");

            // One statement is already atomic, so brackets round it would be
            // ceremony the user has to read past.
            let single = update_batch(engine, &rows[..1]).unwrap();
            assert!(!single.contains("BEGIN"), "{engine:?} {single}");
            assert!(is_generated_write(&single), "{engine:?} {single}");
        }

        let postgres = update_batch(Engine::Postgres, &rows).unwrap();
        assert!(!postgres.contains("BEGIN"), "{postgres}");
    }

    #[test]
    fn a_sql_server_batch_opens_the_way_t_sql_spells_it_and_passes_the_gate() {
        // A bare `BEGIN` opens a statement block in T-SQL, so the brackets are
        // `BEGIN TRANSACTION`; the gate has to see through that spelling too.
        let rows = vec![
            pending_row(&[("name", set("李"))], &[("id", "1")]),
            pending_row(&[("name", set("Bo"))], &[("id", "2")]),
        ];
        let batch = update_batch(Engine::SqlServer, &rows).unwrap();
        assert_eq!(
            batch,
            "BEGIN TRANSACTION;\n\
             UPDATE \"public\".\"accounts\" SET \"name\" = N'李' WHERE \"id\" = N'1';\n\
             UPDATE \"public\".\"accounts\" SET \"name\" = N'Bo' WHERE \"id\" = N'2';\n\
             COMMIT;"
        );
        assert!(is_generated_write(&batch), "{batch}");
    }

    #[test]
    fn the_gate_reads_sql_server_spellings_and_refuses_what_it_always_refused() {
        for admitted in [
            "INSERT INTO \"dbo\".\"accounts\" (\"name\") VALUES (N'x')",
            "DELETE FROM \"dbo\".\"accounts\" WHERE \"id\" = N'1'",
            "BEGIN TRANSACTION; UPDATE \"dbo\".\"a\" SET \"x\" = N'1' WHERE \"id\" = N'1'; COMMIT;",
        ] {
            assert!(is_generated_write(admitted), "{admitted}");
        }
        assert!(delete_matches_key(
            "DELETE FROM \"dbo\".\"accounts\" WHERE \"id\" = N'1'",
            &["id"]
        ));
        for refused in [
            // A transaction it cannot see closed, and one hiding a drop.
            "BEGIN TRANSACTION; UPDATE \"dbo\".\"a\" SET \"x\" = N'1' WHERE \"id\" = N'1';",
            "BEGIN TRANSACTION; DROP TABLE x; COMMIT;",
            // Brackets are T-SQL's own quoting and the pinned grammar's error,
            // which is why dbdelve never writes them.
            "UPDATE [dbo].[a] SET [x] = N'1' WHERE [id] = N'1'",
            // A national prefix on a column is still a column, not a literal.
            "DELETE FROM \"dbo\".\"accounts\" WHERE \"id\" = N\"other\"",
        ] {
            assert!(!is_generated_write(refused), "{refused}");
        }
    }

    #[test]
    fn a_sql_server_preview_is_paged_with_offset_and_fetch() {
        let statement = |limit: &str| format!("SELECT * FROM \"dbo\".\"accounts\"{limit}");
        // `OFFSET` needs an `ORDER BY`, so an unsorted page gets one that
        // orders nothing.
        assert_eq!(
            paged(Engine::SqlServer, &statement(" LIMIT 1000"), &[]).as_deref(),
            Some(
                "SELECT * FROM \"dbo\".\"accounts\" ORDER BY (SELECT NULL) \
                 OFFSET 0 ROWS FETCH NEXT 1000 ROWS ONLY"
            )
        );
        let sorted = with_order_by(
            Engine::SqlServer,
            "SELECT * FROM \"dbo\".\"accounts\" WHERE \"name\" LIKE N'%a[%]%' LIMIT 10 OFFSET 20",
            &[SortKey::new("\"id\"", false)],
        )
        .unwrap();
        assert!(is_generated_select(Engine::SqlServer, &sorted), "{sorted}");
        assert_eq!(
            paged(Engine::SqlServer, &sorted, &[]).as_deref(),
            Some(
                "SELECT * FROM \"dbo\".\"accounts\" WHERE \"name\" LIKE N'%a[%]%' \
                 ORDER BY \"id\" DESC OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
            )
        );
        // Every other engine runs the statement the gate read, unchanged.
        for engine in Engine::ALL.into_iter().filter(|e| *e != Engine::SqlServer) {
            assert_eq!(
                paged(engine, &sorted, &[]).as_deref(),
                Some(sorted.as_str()),
                "{engine:?}"
            );
        }
        // No limit is one page, which needs no `OFFSET` and so no order.
        assert_eq!(
            paged(Engine::SqlServer, &statement(""), &[]).as_deref(),
            Some(statement("").as_str())
        );
        assert_eq!(paged(Engine::SqlServer, "DELETE FROM t LIMIT 1", &[]), None);
    }

    #[test]
    fn an_unpaged_preview_reads_back_the_sort_it_runs_with() {
        let unsorted = "SELECT * FROM \"dbo\".\"accounts\" LIMIT 1000 OFFSET 0";
        let sorted = with_order_by(
            Engine::SqlServer,
            unsorted,
            &[SortKey::new("\"id\"", false)],
        )
        .unwrap();
        for statement in [unsorted, sorted.as_str()] {
            let paged = paged(Engine::SqlServer, statement, &[]).unwrap();
            assert_eq!(
                order_by(Engine::SqlServer, &paged),
                None,
                "the grammar cannot read {paged}"
            );
            assert_eq!(
                order_by(Engine::SqlServer, &unpaged(&paged)),
                order_by(Engine::SqlServer, statement),
                "{paged}"
            );
        }
        assert_eq!(
            order_by(
                Engine::SqlServer,
                &unpaged(&paged(Engine::SqlServer, &sorted, &[]).unwrap())
            ),
            Some(vec![SortKey::new("\"id\"", false)])
        );
        // Not `paged`'s shape: left alone.
        let typed = "SELECT * FROM t ORDER BY id OFFSET 5 ROWS";
        assert_eq!(unpaged(typed), typed);
    }

    #[test]
    fn an_unsorted_sql_server_page_is_ordered_by_the_key() {
        let unsorted = "SELECT * FROM \"dbo\".\"order lines\" LIMIT 100 OFFSET 200";
        let key = ["order_id".to_string(), "line]no".to_string()];
        let page = paged(Engine::SqlServer, unsorted, &key).unwrap();
        assert_eq!(
            page,
            "SELECT * FROM \"dbo\".\"order lines\" ORDER BY (SELECT NULL), \"order_id\", \
             \"line]no\" OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        // Read back as the unsorted preview it is, not as a sort on the key.
        assert_eq!(unpaged(&page), unsorted);
        assert_eq!(
            order_by(Engine::SqlServer, &unpaged(&page)),
            Some(Vec::new())
        );

        // A sort the user asked for is the order, key or no key.
        let sorted =
            with_order_by(Engine::SqlServer, unsorted, &[SortKey::new("\"id\"", true)]).unwrap();
        assert_eq!(
            paged(Engine::SqlServer, &sorted, &key),
            paged(Engine::SqlServer, &sorted, &[])
        );
        // One inside a filter's string is the user's text, not paged's marker.
        let quoted = with_order_by(
            Engine::SqlServer,
            "SELECT * FROM t WHERE a = ' ORDER BY (SELECT NULL)' LIMIT 5 OFFSET 0",
            &[SortKey::new("a", false)],
        )
        .unwrap();
        let quoted_page = paged(Engine::SqlServer, &quoted, &key).unwrap();
        assert_eq!(unpaged(&quoted_page), quoted);
    }

    #[test]
    fn classify_reads_t_sql_spellings() {
        let cases: &[(&str, Verdict)] = &[
            ("SELECT [id] FROM [dbo].[accounts]", Verdict::READ),
            ("SELECT TOP 10 * FROM t", Verdict::READ),
            (
                "SELECT * FROM t ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY",
                Verdict::READ,
            ),
            ("SELECT N'李' AS name", Verdict::READ),
            (
                "UPDATE [dbo].[t] SET [x] = N'a' WHERE [id] = 1",
                Verdict::WRITE,
            ),
            (
                "BEGIN TRANSACTION; UPDATE t SET x = N'1' WHERE id = N'1'; COMMIT;",
                Verdict::WRITE,
            ),
            ("TRUNCATE TABLE t", Verdict::destroys(Destructive::Truncate)),
            ("DROP TABLE [t]", Verdict::destroys(Destructive::Drop)),
            (
                "BEGIN TRAN; DELETE FROM t; COMMIT TRAN",
                Verdict::destroys(Destructive::UnfilteredDelete),
            ),
            // Every branch counts: which one runs is the server's decision.
            (
                "IF 1 = 1 SELECT 1 ELSE DELETE FROM t",
                Verdict::destroys(Destructive::UnfilteredDelete),
            ),
            ("IF 1 = 1 SELECT 1", Verdict::READ),
            ("EXEC dbo.deactivate_account 1", Verdict::FULL),
        ];
        for (sql, verdict) in cases {
            assert_eq!(classify(Engine::SqlServer, sql), *verdict, "{sql}");
        }
    }

    #[test]
    fn an_if_is_as_strict_as_its_conditions() {
        let cases: &[(Engine, &str, Verdict)] = &[
            (
                Engine::SqlServer,
                "IF EXISTS (SELECT 1 FROM t) SELECT 1",
                Verdict::READ,
            ),
            (Engine::SqlServer, "IF @x IS NULL SELECT 1", Verdict::READ),
            (Engine::SqlServer, "IF dbo.f() = 1 SELECT 1", Verdict::FULL),
            // Before `IF` had an arm it needed Full everywhere; a condition that
            // calls something still does, on every engine that parses one.
            (
                Engine::Snowflake,
                "BEGIN IF (side_effect() = 1) THEN SELECT 1; END IF; END",
                Verdict::FULL,
            ),
            (
                Engine::Snowflake,
                "BEGIN IF (1 = 1) THEN SELECT 1; END IF; END",
                Verdict::READ,
            ),
            (
                Engine::MySql,
                "IF 1 = 1 THEN SELECT 1; ELSEIF f() THEN SELECT 2; END IF",
                Verdict::FULL,
            ),
            (
                Engine::Postgres,
                "IF 1 = 1 THEN SELECT 1; ELSEIF 2 IN (1, 2) THEN SELECT 2; END IF",
                Verdict::READ,
            ),
        ];
        for (engine, sql, verdict) in cases {
            assert_eq!(classify(*engine, sql), *verdict, "{engine:?}: {sql}");
        }
    }

    #[test]
    fn a_row_with_no_key_to_find_it_by_refuses_the_whole_batch() {
        let rows = vec![
            pending_row(&[("name", set("Ada"))], &[("id", "1")]),
            // No keys at all: update_row refuses this one, since there is
            // nothing to identify the row it would touch.
            pending_row(&[("name", set("Bo"))], &[]),
        ];

        assert!(
            update_row(
                Engine::Postgres,
                "public",
                "accounts",
                &[("name", set("Bo"))],
                &[],
                &[]
            )
            .is_none()
        );
        assert!(update_batch(Engine::Postgres, &rows).is_err());
    }

    #[test]
    fn an_empty_batch_of_rows_has_nothing_to_send() {
        assert!(update_batch(Engine::Postgres, &[]).is_err());
    }

    #[test]
    fn a_statement_run_again_moves_to_the_front_rather_than_doubling() {
        let mut history = vec!["SELECT 2".to_string(), "SELECT 1".to_string()];
        remember_statement(&mut history, "SELECT 1");

        assert_eq!(history, ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn a_recalled_statement_starts_on_the_line_the_cursor_is_sent_to() {
        // The line `recall_statement` computes, against the text it computes it
        // from. A cursor on the wrong line runs the wrong statement.
        for (buffer, recalled) in [
            ("", "SELECT 1"),
            ("SELECT 2", "SELECT 1"),
            ("SELECT 2;\n", "SELECT\n  1"),
        ] {
            let appended = appended_statement(Engine::Postgres, buffer, recalled);
            let line = appended.lines().count() - recalled.lines().count();

            assert_eq!(
                appended.lines().nth(line),
                recalled.lines().next(),
                "{appended:?}"
            );
        }
    }

    #[test]
    fn an_unterminated_buffer_is_terminated_before_the_appended_statement() {
        // Without the semicolon, "SELECT 1" and "UPDATE ..." would read back
        // as a single statement, and cmd+enter would send both at once.
        assert_eq!(
            appended_statement(Engine::Postgres, "SELECT 1", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn a_redis_buffer_takes_its_appended_command_on_a_line_of_its_own() {
        let appended = appended_statement(Engine::Redis, "GET a  \n", "SET a 1 KEEPTTL");
        assert_eq!(appended, "GET a\n\nSET a 1 KEEPTTL");
        assert_eq!(crate::redis_cmd::parse(&appended).unwrap().len(), 2);
    }

    #[test]
    fn an_already_terminated_buffer_keeps_a_single_semicolon() {
        assert_eq!(
            appended_statement(Engine::Postgres, "SELECT 1;", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn a_sort_spliced_after_a_line_comment_starts_its_own_line() {
        let by_name = [SortKey::new("name", true)];
        for (statement, expected) in [
            (
                "select * from t -- recent\nlimit 10",
                "select * from t -- recent\nORDER BY name ASC limit 10",
            ),
            (
                "select * from t -- recent\norder by id limit 10",
                "select * from t -- recent\nORDER BY name ASC limit 10",
            ),
            (
                "select * from t /* recent */ limit 10",
                "select * from t /* recent */ ORDER BY name ASC limit 10",
            ),
            (
                "select * from t where note = 'a--b' limit 10",
                "select * from t where note = 'a--b' ORDER BY name ASC limit 10",
            ),
        ] {
            assert_eq!(
                with_order_by(Engine::Sqlite, statement, &by_name).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            with_order_by(
                Engine::Sqlite,
                "select * from t -- recent\norder by id limit 10",
                &[]
            )
            .as_deref(),
            Some("select * from t -- recent\nlimit 10")
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_still_unpages_on_sql_server() {
        for value in ["'a--b'", "'a#b'"] {
            let preview =
                format!("SELECT * FROM \"t\" WHERE \"name\" = {value} LIMIT 100 OFFSET 0");
            let paged = paged(Engine::SqlServer, &preview, &["id".into()]).unwrap();

            assert_eq!(unpaged(&paged), preview, "{paged}");
        }
    }

    #[test]
    fn a_trailing_comment_does_not_swallow_the_terminator() {
        for (buffer, expected) in [
            ("SELECT 1 -- note", "SELECT 1; -- note"),
            ("SELECT 1 -- note;", "SELECT 1; -- note;"),
            ("SELECT 1 /* note */", "SELECT 1; /* note */"),
            ("SELECT 1; -- note", "SELECT 1; -- note"),
            ("SELECT 1; /* note; */", "SELECT 1; /* note; */"),
            ("SELECT 'a;'", "SELECT 'a;';"),
            ("SELECT 1;;", "SELECT 1;;"),
        ] {
            let appended = appended_statement(Engine::Postgres, buffer, "UPDATE t SET a = 1");

            assert_eq!(appended, format!("{expected}\n\nUPDATE t SET a = 1"));
            assert_eq!(
                Buffer::parse(&appended).statements().len(),
                2,
                "{appended:?}"
            );
        }
    }

    #[test]
    fn an_empty_buffer_yields_just_the_statement() {
        assert_eq!(
            appended_statement(Engine::Postgres, "", "UPDATE t SET a = 1"),
            "UPDATE t SET a = 1"
        );
    }

    #[test]
    fn trailing_whitespace_in_the_buffer_does_not_ragged_the_join() {
        assert_eq!(
            appended_statement(Engine::Postgres, "SELECT 1\n\n  ", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn classify_separates_reads_writes_and_destruction() {
        let cases: &[(&str, Mode, Option<Destructive>)] = &[
            ("SELECT 1", Mode::ReadOnly, None),
            ("WITH x AS (SELECT 1) SELECT * FROM x", Mode::ReadOnly, None),
            ("SHOW TABLES", Mode::ReadOnly, None),
            ("EXPLAIN SELECT 1", Mode::ReadOnly, None),
            ("EXPLAIN ANALYZE SELECT 1", Mode::ReadOnly, None),
            ("", Mode::ReadOnly, None),
            ("-- nothing here", Mode::ReadOnly, None),
            // The semicolon is inside a literal, so this is one read and not a DROP.
            ("SELECT ';DROP TABLE t'", Mode::ReadOnly, None),
            ("INSERT INTO t (a) VALUES (1)", Mode::ReadWrite, None),
            ("UPDATE t SET a = 1 WHERE id = 2", Mode::ReadWrite, None),
            ("DELETE FROM t WHERE id = 2", Mode::ReadWrite, None),
            // A predicate that narrows nothing is still a predicate: spec §8.
            ("DELETE FROM t WHERE 1 = 1", Mode::ReadWrite, None),
            ("CREATE TABLE t (a int)", Mode::ReadWrite, None),
            ("CREATE INDEX i ON t (a)", Mode::ReadWrite, None),
            ("ALTER TABLE t ADD COLUMN c int", Mode::ReadWrite, None),
            (
                "DELETE FROM t",
                Mode::Full,
                Some(Destructive::UnfilteredDelete),
            ),
            (
                "EXPLAIN ANALYZE DELETE FROM t",
                Mode::Full,
                Some(Destructive::UnfilteredDelete),
            ),
            ("DROP TABLE t", Mode::Full, Some(Destructive::Drop)),
            ("TRUNCATE TABLE t", Mode::Full, Some(Destructive::Truncate)),
            ("ALTER TABLE t DROP COLUMN c", Mode::Full, None),
            ("ALTER TABLE t RENAME TO u", Mode::Full, None),
            // The maximum over the operations, not the first.
            (
                "ALTER TABLE t ADD COLUMN a int, DROP COLUMN b",
                Mode::Full,
                None,
            ),
            // The case tree-sitter got wrong, kept as a regression test.
            ("GRANT SELECT ON t TO u", Mode::Full, None),
            ("REVOKE SELECT ON t FROM u", Mode::Full, None),
            // Opaque bodies: dbdelve cannot see what these run.
            ("CALL p()", Mode::Full, None),
            // The maximum over the statements, not the first.
            (
                "SELECT 1; DROP TABLE t",
                Mode::Full,
                Some(Destructive::Drop),
            ),
            // Read-only: Postgres holds the session to reads, so what the gate
            // cannot read the server still can.
            ("SELCT 1", Mode::ReadOnly, Some(Destructive::Unreadable)),
            (
                "DO $$ BEGIN NULL; END $$",
                Mode::ReadOnly,
                Some(Destructive::Unreadable),
            ),
            // A normal thing to type at a SQLite database, and the crate does not
            // accept it under any dialect -- spec §3.4, and the reason shape C
            // exists at all.
            (
                "PRAGMA table_info(t)",
                Mode::ReadOnly,
                Some(Destructive::Unreadable),
            ),
        ];

        for (sql, mode, destructive) in cases {
            assert_eq!(
                classify(Engine::Postgres, sql),
                Verdict {
                    mode: *mode,
                    destructive: destructive.iter().copied().collect(),
                },
                "{sql}"
            );
        }
    }

    /// The single most likely bug in the feature: this parses as `Statement::Query`,
    /// so a match on the top-level variant calls a table-emptying statement a read.
    /// Spec §3.5.
    #[test]
    fn only_a_read_is_rerun_to_reload_its_rows() {
        assert!(rerunnable(Engine::Postgres, "SELECT * FROM t"));
        for sql in [
            "INSERT INTO t VALUES (1) RETURNING *",
            "UPDATE t SET v = 1 RETURNING *",
            "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
            "SELEC * FROM t",
        ] {
            assert!(!rerunnable(Engine::Postgres, sql), "{sql}");
        }
    }

    #[test]
    fn only_a_lone_read_is_stopped_at_the_limit() {
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::Snowflake,
            Engine::SqlServer,
        ] {
            assert!(stoppable(engine, "SELECT * FROM t"), "{engine:?}");
            assert!(stoppable(engine, "SELECT * FROM t;"), "{engine:?}");
            assert!(
                stoppable(engine, "-- the big one\nSELECT * FROM t"),
                "{engine:?}"
            );
            for sql in [
                "SELECT * FROM t; COMMIT",
                "SET x = 1; SELECT * FROM t",
                "BEGIN; SELECT * FROM t; COMMIT",
                "SELECT 1; SELECT * FROM t",
                "UPDATE t SET v = 1",
            ] {
                assert!(!stoppable(engine, sql), "{engine:?}: {sql}");
            }
        }
        assert!(!stoppable(
            Engine::SqlServer,
            "SELECT * FROM t\nSELECT * FROM u"
        ));
        // SQL Server caps a stoppable read with `SET ROWCOUNT`, which caps a
        // write just the same, so nothing that writes may get through.
        for sql in [
            "SELECT * INTO #copy FROM t",
            "SELECT * INTO copy FROM t",
            "EXEC dbo.report",
            "EXECUTE dbo.report 1",
            "dbo.report",
            "INSERT INTO t SELECT * FROM u",
            "SELECT * FROM t; DELETE FROM t",
            "IF 1 = 1 DELETE FROM t",
            "BEGIN SELECT * FROM t; DELETE FROM t END",
            "WITH c AS (SELECT 1 AS a) DELETE FROM t",
            "MERGE t USING u ON t.id = u.id WHEN MATCHED THEN DELETE;",
        ] {
            assert!(!stoppable(Engine::SqlServer, sql), "{sql}");
        }
        assert!(stoppable(
            Engine::SqlServer,
            "WITH c AS (SELECT 1 AS a) SELECT * FROM c"
        ));
        for (engine, sql) in [
            (
                Engine::Postgres,
                "SELECT set_config('search_path', 'other', false), * FROM t",
            ),
            (Engine::Postgres, "SELECT pg_advisory_lock(1), * FROM t"),
            (Engine::MySql, "SELECT GET_LOCK('held', 0), t.* FROM t"),
            (Engine::MySql, "SELECT @n := @n + 1, t.* FROM t"),
        ] {
            assert!(!stoppable(engine, sql), "{engine:?}: {sql}");
        }
        assert!(stoppable(Engine::MongoDb, "db.accounts.find({})"));
        assert!(stoppable(
            Engine::MongoDb,
            "db.accounts\n  .find({})\n  .sort({a: 1})"
        ));
        assert!(!stoppable(
            Engine::MongoDb,
            "db.accounts.find({})\ndb.other.find({})"
        ));
        assert!(!stoppable(
            Engine::MongoDb,
            "db.accounts.aggregate([{$out: 'copy'}])"
        ));
    }

    /// Every MongoDB write returns a reply grid, so a restored one is offered
    /// Refresh only when this holds.
    #[test]
    fn a_mongo_write_is_never_rerun_to_reload_its_reply() {
        for sql in [
            "db.accounts.find({})",
            "db.accounts.aggregate([{$match: {}}])",
        ] {
            assert!(rerunnable(Engine::MongoDb, sql), "{sql}");
        }
        for sql in [
            "db.accounts.insertOne({a: 1})",
            "db.accounts.updateOne({_id: 1}, {$set: {a: 2}})",
            "db.accounts.deleteOne({_id: 1})",
            "db.accounts.findOneAndUpdate({_id: 1}, {$set: {a: 2}})",
            "db.accounts.aggregate([{$out: 'copy'}])",
            "db.accounts.aggregate([{$merge: 'copy'}])",
            "db.accounts.find({}",
        ] {
            assert!(!rerunnable(Engine::MongoDb, sql), "{sql}");
        }
    }

    #[test]
    fn classify_sees_through_data_modifying_ctes() {
        let delete = "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x";
        assert_eq!(
            classify(Engine::Postgres, delete),
            Verdict {
                mode: Mode::Full,
                destructive: vec![Destructive::UnfilteredDelete],
            }
        );

        let insert = "WITH x AS (INSERT INTO t (a) VALUES (1) RETURNING *) SELECT * FROM x";
        assert_eq!(classify(Engine::Postgres, insert).mode, Mode::ReadWrite);

        let update = "WITH x AS (UPDATE t SET a = 1 WHERE id = 2 RETURNING *) SELECT * FROM x";
        assert_eq!(classify(Engine::Postgres, update).mode, Mode::ReadWrite);
    }

    /// The parenthesized option list is a second spelling of ANALYZE, and it
    /// runs the statement just as the keyword does: live-verified on Postgres
    /// 18.6, where the row was gone after a classification of ReadOnly.
    #[test]
    fn classify_reads_analyze_from_the_explain_option_list() {
        let cases = [
            "EXPLAIN (ANALYZE) DELETE FROM t",
            "EXPLAIN (ANALYZE TRUE, COSTS FALSE) DELETE FROM t",
            "EXPLAIN (COSTS FALSE, ANALYZE ON) DELETE FROM t",
            "EXPLAIN (analyze true) DELETE FROM t",
        ];

        for sql in cases {
            assert_eq!(
                classify(Engine::Postgres, sql),
                Verdict {
                    mode: Mode::Full,
                    destructive: vec![Destructive::UnfilteredDelete],
                },
                "{sql}"
            );
        }

        for sql in [
            "EXPLAIN (ANALYZE FALSE) DELETE FROM t",
            "EXPLAIN (ANALYZE OFF) DELETE FROM t",
            "EXPLAIN (COSTS TRUE) SELECT 1",
        ] {
            assert_eq!(
                classify(Engine::Postgres, sql).mode,
                Mode::ReadOnly,
                "{sql}"
            );
        }
    }

    /// MariaDB's Explain Analyze is `ANALYZE <statement>`, which runs the
    /// statement just as MySQL's `EXPLAIN ANALYZE` does.
    #[test]
    fn classify_reads_mariadb_analyze_by_the_statement_it_runs() {
        for sql in [
            "ANALYZE SELECT * FROM t",
            "  /* why */ analyze\nselect id from accounts where x = 1",
            "ANALYZE FORMAT=JSON SELECT * FROM t",
            "ANALYZE WITH x AS (SELECT 1) SELECT * FROM x",
        ] {
            assert_eq!(classify(Engine::MariaDb, sql), Verdict::READ, "{sql}");
            assert!(rerunnable(Engine::MariaDb, sql), "{sql}");
        }

        assert_eq!(
            classify(Engine::MariaDb, "ANALYZE DELETE FROM t"),
            classify(Engine::MySql, "EXPLAIN ANALYZE DELETE FROM t"),
        );
        assert_eq!(
            classify(Engine::MariaDb, "ANALYZE DELETE FROM t"),
            Verdict::destroys(Destructive::UnfilteredDelete),
        );
        for sql in [
            "ANALYZE UPDATE t SET a = 1 WHERE id = 2",
            "ANALYZE FORMAT=JSON DELETE FROM t WHERE id = 2",
            "ANALYZE INSERT INTO t (a) VALUES (1)",
            "ANALYZE SELECT 1; UPDATE t SET a = 1 WHERE id = 2",
        ] {
            assert_eq!(classify(Engine::MariaDb, sql), Verdict::WRITE, "{sql}");
        }

        // The table-statistics statement, which only an allowlist keeps from
        // reading as an EXPLAIN of the query `TABLE t`.
        for sql in [
            "ANALYZE TABLE t",
            "ANALYZE NO_WRITE_TO_BINLOG TABLE t",
            "ANALYZE LOCAL TABLE t",
            "ANALYZE t",
            "ANALYZE FORMAT=JSON TABLE t",
        ] {
            assert_eq!(
                classify(Engine::MariaDb, sql),
                classify(Engine::MySql, sql),
                "{sql}"
            );
            assert!(!rerunnable(Engine::MariaDb, sql), "{sql}");
        }

        assert_eq!(
            classify(Engine::MySql, "ANALYZE SELECT * FROM t").destructive,
            vec![Destructive::Unreadable],
        );
    }

    #[test]
    fn a_typed_explain_is_read_from_its_leading_words() {
        use ExplainMode::{Analyze, Plan};
        let cases = [
            (Engine::Postgres, "EXPLAIN SELECT 1", Some(Plan)),
            (Engine::Postgres, "explain verbose select 1", Some(Plan)),
            (Engine::Postgres, "EXPLAIN ANALYZE SELECT 1", Some(Analyze)),
            (
                Engine::Postgres,
                "EXPLAIN ANALYSE VERBOSE SELECT 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "-- why\nEXPLAIN ANALYZE SELECT 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "  /* why */ explain analyze select 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (ANALYZE, BUFFERS) SELECT 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (analyze true) SELECT 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (COSTS FALSE, ANALYZE ON) SELECT 1",
                Some(Analyze),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (ANALYZE false) SELECT 1",
                Some(Plan),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (ANALYZE OFF, BUFFERS) SELECT 1",
                Some(Plan),
            ),
            (
                Engine::Postgres,
                "EXPLAIN (FORMAT TEXT, ANALYZE) SELECT 1",
                Some(Analyze),
            ),
            (Engine::Postgres, "EXPLAIN (FORMAT JSON) SELECT 1", None),
            (
                Engine::Postgres,
                "EXPLAIN (ANALYZE, format yaml) SELECT 1",
                None,
            ),
            (Engine::Postgres, "EXPLAIN (FORMAT XML) SELECT 1", None),
            (Engine::Postgres, "SELECT 'EXPLAIN ANALYZE'", None),
            (Engine::Postgres, "ANALYZE t", None),
            (Engine::Postgres, "-- EXPLAIN\nSELECT 1", None),
            (Engine::MySql, "EXPLAIN ANALYZE SELECT 1", Some(Analyze)),
            (
                Engine::MySql,
                "/* why */ explain analyze select 1",
                Some(Analyze),
            ),
            (
                Engine::MySql,
                "EXPLAIN ANALYZE FORMAT=TREE SELECT 1",
                Some(Analyze),
            ),
            (Engine::MySql, "EXPLAIN FORMAT=TREE SELECT 1", Some(Plan)),
            (Engine::MySql, "explain format = tree select 1", Some(Plan)),
            (Engine::MySql, "EXPLAIN ANALYZE FORMAT=JSON SELECT 1", None),
            (Engine::MySql, "EXPLAIN FORMAT=JSON SELECT 1", None),
            // Tabular, a shape the plan pane does not read.
            (Engine::MySql, "EXPLAIN SELECT 1", None),
            (Engine::MySql, "EXPLAIN FORMAT=TRADITIONAL SELECT 1", None),
            (Engine::MySql, "EXPLAIN t", None),
            (Engine::MySql, "DESCRIBE SELECT 1", None),
            (Engine::MySql, "DESC t", None),
            // Both of MariaDB's are tabular.
            (Engine::MariaDb, "EXPLAIN SELECT 1", None),
            (Engine::MariaDb, "ANALYZE SELECT 1", None),
            (Engine::MariaDb, "ANALYZE FORMAT=JSON SELECT 1", None),
            (Engine::MariaDb, "ANALYZE TABLE t", None),
            (Engine::Sqlite, "EXPLAIN QUERY PLAN SELECT 1", Some(Plan)),
            (
                Engine::Sqlite,
                "-- why\nexplain query plan select 1",
                Some(Plan),
            ),
            // Bytecode, not a plan.
            (Engine::Sqlite, "EXPLAIN SELECT 1", None),
            (Engine::Snowflake, "EXPLAIN SELECT 1", None),
            (Engine::SqlServer, "EXPLAIN SELECT 1", None),
            (Engine::MongoDb, "db.c.find({a: 1}).explain()", Some(Plan)),
            (
                Engine::MongoDb,
                "db.c.find().explain('queryPlanner')",
                Some(Plan),
            ),
            (Engine::MongoDb, "db.c.find().explain(false)", Some(Plan)),
            (
                Engine::MongoDb,
                "db.c.find().sort({a: 1}).explain('executionStats')",
                Some(Analyze),
            ),
            (
                Engine::MongoDb,
                "db.c.aggregate([]).explain(\"allPlansExecution\")",
                Some(Analyze),
            ),
            (
                Engine::MongoDb,
                "db.c.countDocuments({}).explain(true)",
                Some(Analyze),
            ),
            (Engine::MongoDb, "// why\ndb.c.find().explain()", Some(Plan)),
            (Engine::MongoDb, "db.c.find()", None),
            (Engine::MongoDb, "db.c.find().explain(); db.c.find()", None),
        ];
        for (engine, sql, mode) in cases {
            assert_eq!(typed_explain(engine, sql), mode, "{engine:?} {sql}");
        }
    }

    /// `TO STDOUT` hands rows to the client; a `PROGRAM` or file target runs a
    /// shell command or touches the server's filesystem. Live-verified on
    /// Postgres 18.6: both wrote a file on the server while classifying ReadOnly.
    #[test]
    fn classify_treats_server_side_copy_targets_as_full() {
        assert_eq!(
            classify(Engine::Postgres, "COPY (SELECT 1) TO STDOUT").mode,
            Mode::ReadOnly
        );
        assert_eq!(
            classify(Engine::Postgres, "COPY t FROM STDIN").mode,
            Mode::ReadWrite
        );

        for sql in [
            "COPY (SELECT 1) TO PROGRAM 'touch /tmp/x'",
            "COPY (SELECT 1) TO '/tmp/x'",
            "COPY t FROM PROGRAM 'cat /etc/passwd'",
            "COPY t FROM '/tmp/x'",
        ] {
            assert_eq!(classify(Engine::Postgres, sql).mode, Mode::Full, "{sql}");
        }
    }

    /// Four variants own a `Query` besides `Statement::Query`, and classifying
    /// any of them by its variant alone runs a DELETE from the mode that
    /// forbids it. The first two were verified against a live Postgres: three
    /// rows became zero, with no dialog. Spec §3.5.
    #[test]
    fn classify_sees_a_cte_wherever_the_query_hangs() {
        let cases = [
            "COPY (WITH x AS (DELETE FROM t RETURNING a) SELECT a FROM x) TO STDOUT",
            "CREATE TABLE n AS WITH x AS (DELETE FROM t RETURNING a) SELECT a FROM x",
            "CREATE VIEW v AS WITH x AS (DELETE FROM t RETURNING a) SELECT a FROM x",
            "INSERT INTO n (a) WITH x AS (DELETE FROM t RETURNING a) SELECT a FROM x",
        ];

        for sql in cases {
            assert_eq!(
                classify(Engine::Postgres, sql),
                Verdict {
                    mode: Mode::Full,
                    destructive: vec![Destructive::UnfilteredDelete],
                },
                "{sql}"
            );
        }
    }

    /// `SELECT … INTO` creates a table. It is `Statement::Query` over a
    /// `SetExpr::Select`, so only `Select.into` tells it apart from a read.
    #[test]
    fn select_into_is_a_write() {
        assert_eq!(
            classify(Engine::Postgres, "SELECT * INTO newt FROM t").mode,
            Mode::ReadWrite
        );
        assert_eq!(
            classify(Engine::Postgres, "SELECT * FROM t").mode,
            Mode::ReadOnly
        );
    }

    /// Every one of these parses as the variant a plain write does, one flag
    /// apart, and destroys what the object held before.
    #[test]
    fn classify_calls_replacing_a_data_holding_object_destructive() {
        let cases: &[(Engine, &str, Destructive)] = &[
            (
                Engine::Snowflake,
                "CREATE OR REPLACE TABLE t (a int)",
                Destructive::Drop,
            ),
            (
                Engine::Snowflake,
                "CREATE OR REPLACE TABLE t AS SELECT 1 AS a",
                Destructive::Drop,
            ),
            (
                Engine::Snowflake,
                "CREATE OR REPLACE SCHEMA s",
                Destructive::Drop,
            ),
            (
                Engine::Snowflake,
                "CREATE OR REPLACE DATABASE d",
                Destructive::Drop,
            ),
            (
                Engine::Snowflake,
                "CREATE OR REPLACE STAGE st",
                Destructive::Drop,
            ),
            (
                Engine::Snowflake,
                "INSERT OVERWRITE INTO t SELECT * FROM u",
                Destructive::Truncate,
            ),
        ];
        for (engine, sql, kind) in cases {
            assert_eq!(
                classify(*engine, sql),
                Verdict {
                    mode: Mode::Full,
                    destructive: vec![*kind],
                },
                "{sql}"
            );
        }

        for sql in [
            "CREATE OR REPLACE VIEW v AS SELECT 1 AS a",
            "CREATE TABLE t (a int)",
            "CREATE SCHEMA s",
            "INSERT INTO t SELECT * FROM u",
        ] {
            assert_eq!(classify(Engine::Snowflake, sql), Verdict::WRITE, "{sql}");
        }
        for engine in [Engine::Postgres, Engine::MySql] {
            assert_eq!(
                classify(engine, "CREATE OR REPLACE VIEW v AS SELECT 1"),
                Verdict::WRITE,
                "{engine:?}"
            );
            assert_eq!(
                classify(engine, "CREATE TABLE t (a int)"),
                Verdict::WRITE,
                "{engine:?}"
            );
        }
    }

    /// One AST, three dialects. Two statements genuinely differ and are asserted as
    /// differing rather than skipped.
    #[test]
    fn classify_agrees_across_engines() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            assert_eq!(
                classify(engine, "SELECT 1").mode,
                Mode::ReadOnly,
                "{engine:?}"
            );
            assert_eq!(
                classify(engine, "DROP TABLE t").destructive,
                vec![Destructive::Drop],
                "{engine:?}"
            );
            assert_eq!(
                classify(engine, "DELETE FROM t").destructive,
                vec![Destructive::UnfilteredDelete],
                "{engine:?}"
            );
        }

        // CREATE FUNCTION parses only under Postgres, where its opaque body makes
        // it Full outright; elsewhere it is unreadable, which is Full too but by a
        // different route and with a different dialog.
        let create_function = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$";
        assert_eq!(
            classify(Engine::Postgres, create_function),
            Verdict {
                mode: Mode::Full,
                destructive: Vec::new(),
            }
        );
        assert_eq!(
            classify(Engine::MySql, create_function).destructive,
            vec![Destructive::Unreadable]
        );

        // COMMENT ON is the other measured divergence: a write on Postgres, and
        // syntax the crate does not accept under the other two dialects.
        let comment = "COMMENT ON TABLE t IS 'hello'";
        assert_eq!(classify(Engine::Postgres, comment).mode, Mode::ReadWrite);
        for engine in [Engine::MySql, Engine::Sqlite] {
            assert_eq!(
                classify(engine, comment).destructive,
                vec![Destructive::Unreadable],
                "{engine:?}"
            );
        }
    }

    /// What `restore_profile` reads off disk. A slug this build does not have
    /// must be answerable, not fatal -- the alternative was one unknown value
    /// costing the user every connection in the file.
    #[test]
    fn a_stored_slug_this_build_cannot_read_is_answerable() {
        for mode in Mode::ALL {
            assert_eq!(Mode::from_slug(mode.slug()), Some(mode));
        }
        assert_eq!(Mode::from_slug("read-append"), None);

        for kind in Destructive::SUPPRESSIBLE {
            assert_eq!(Destructive::from_slug(kind.slug()), Some(kind));
        }
        assert_eq!(Destructive::from_slug("shred"), None);
        // Never silenceable however it got written there, which is also what
        // keeps `Destructive::label` from ever being asked about it.
        assert_eq!(Destructive::from_slug(Destructive::Unreadable.slug()), None);
    }

    #[test]
    fn unreadable_is_never_suppressible() {
        assert!(!Destructive::Unreadable.suppressible());
        assert!(Destructive::Drop.suppressible());
    }

    #[test]
    fn the_gate_offers_exactly_the_mode_a_statement_needs() {
        let write = Verdict {
            mode: Mode::ReadWrite,
            destructive: Vec::new(),
        };
        let drop = Verdict {
            mode: Mode::Full,
            destructive: vec![Destructive::Drop],
        };

        assert_eq!(gate(&Verdict::READ, Mode::ReadOnly, &[]), None);
        assert_eq!(
            gate(&write, Mode::ReadOnly, &[]),
            Some(Stop::Upgrade(Mode::ReadWrite))
        );
        // Full, not Read-write: offering an intermediate mode that still refuses is
        // a second dialog dressed as a first.
        assert_eq!(
            gate(&drop, Mode::ReadOnly, &[]),
            Some(Stop::Upgrade(Mode::Full))
        );
        assert_eq!(gate(&write, Mode::ReadWrite, &[]), None);
        assert_eq!(
            gate(&drop, Mode::Full, &[]),
            Some(Stop::Confirm(Destructive::Drop))
        );
        assert_eq!(gate(&drop, Mode::Full, &[Destructive::Drop]), None);
        // Per kind: silencing DROP says nothing about TRUNCATE.
        assert_eq!(
            gate(&drop, Mode::Full, &[Destructive::Truncate]),
            Some(Stop::Confirm(Destructive::Drop))
        );
    }

    /// One silenced kind must not mask another. With DROP silenced,
    /// `DROP TABLE a; TRUNCATE TABLE b` used to gate to None and run both --
    /// and the TRUNCATE had never been confirmed on that connection.
    #[test]
    fn a_silenced_kind_does_not_silence_the_one_beside_it() {
        let both = classify(Engine::Postgres, "DROP TABLE a; TRUNCATE TABLE b");
        assert_eq!(
            both.destructive,
            vec![Destructive::Drop, Destructive::Truncate]
        );

        assert_eq!(
            gate(&both, Mode::Full, &[Destructive::Drop]),
            Some(Stop::Confirm(Destructive::Truncate))
        );
        // One kind at a time, in the order the submission carries them.
        assert_eq!(
            gate(&both, Mode::Full, &[]),
            Some(Stop::Confirm(Destructive::Drop))
        );
        assert_eq!(
            gate(
                &both,
                Mode::Full,
                &[Destructive::Drop, Destructive::Truncate]
            ),
            None
        );
    }

    #[test]
    fn an_unreadable_statement_never_offers_a_mode_and_never_goes_quiet() {
        // Not Upgrade, in any mode: a typo must never ask to raise a connection to
        // Full in order to receive a syntax error.
        for engine in [Engine::Postgres, Engine::MySql] {
            let unreadable = classify(engine, "UNDROP TABLE t");
            for mode in Mode::ALL {
                assert_eq!(
                    gate(&unreadable, mode, &[]),
                    Some(Stop::RunOnce),
                    "{engine:?} {mode:?}"
                );
            }
        }
        let unreadable = classify(Engine::Postgres, "SELCT 1");

        // Not suppressible even if something contrived writes it into the list.
        assert_eq!(
            gate(&unreadable, Mode::Full, &[Destructive::Unreadable]),
            Some(Stop::RunOnce)
        );
    }

    /// With no server-side hold behind Read-only, a statement the gate cannot
    /// read could write with nothing to stop it -- and each of these can.
    #[test]
    fn read_only_refuses_what_it_cannot_read_where_the_server_would_not() {
        let cases: &[(Engine, &str)] = &[
            (Engine::Snowflake, "UNDROP TABLE t"),
            (Engine::Snowflake, "PUT file:///tmp/a.csv @st"),
            (Engine::Sqlite, "PRAGMA table_info(t)"),
        ];
        for (engine, sql) in cases {
            let verdict = classify(*engine, sql);
            assert_eq!(verdict.destructive, vec![Destructive::Unreadable], "{sql}");
            assert_eq!(
                gate(&verdict, Mode::ReadOnly, &[]),
                Some(Stop::Upgrade(Mode::ReadWrite)),
                "{sql}"
            );
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(gate(&verdict, mode, &[]), Some(Stop::RunOnce), "{sql}");
            }
            assert!(!rerunnable(*engine, sql), "{sql}");
        }
        assert!(!rerunnable(Engine::Postgres, "SELCT 1"));

        // A scripting block parses, and is as dangerous as what it holds.
        assert_eq!(
            classify(Engine::Snowflake, "BEGIN DELETE FROM t; END").destructive,
            vec![Destructive::UnfilteredDelete]
        );
        assert_eq!(
            classify(Engine::Snowflake, "BEGIN SELECT 1; END"),
            Verdict::READ
        );
        assert_eq!(classify(Engine::Snowflake, "BEGIN"), Verdict::READ);
        assert_eq!(
            classify(
                Engine::Snowflake,
                "BEGIN SELECT 1; EXCEPTION WHEN OTHER THEN DELETE FROM t; END"
            ),
            Verdict::FULL
        );

        // This one parses, and its opaque body makes it Full like `CALL`.
        assert_eq!(
            classify(
                Engine::Snowflake,
                "EXECUTE IMMEDIATE $$ BEGIN DELETE FROM t; END $$"
            ),
            Verdict::FULL
        );
    }

    #[test]
    fn a_comment_the_user_wrote_survives_formatting() {
        // The whole reason formatting is token-level: an AST round trip would
        // drop this line, and the user would not get it back.
        let formatted = format(
            Engine::Postgres,
            "select a -- the one we care about\nfrom t",
        )
        .unwrap();

        assert!(
            formatted.contains("-- the one we care about"),
            "{formatted}"
        );
    }

    // sqlformat reflows the inside of a `$$` body, which edits the literal
    // rather than its layout. Refusing the buffer is the only answer that keeps
    // hard rule 1; this is the test that catches the day that stops being true.
    #[test]
    fn a_buffer_holding_a_dollar_quoted_body_is_refused() {
        assert!(format(Engine::Postgres, "DO $$ BEGIN DELETE FROM t; END $$").is_err());
        assert!(format(Engine::Postgres, "select $tag$ x; y $tag$").is_err());
    }

    // A placeholder is not a quote, and reading it as one would refuse to format
    // every parameterised statement anybody writes.
    #[test]
    fn a_numbered_placeholder_still_formats() {
        assert!(format(Engine::Postgres, "select a from t where id = $1").is_ok());
    }

    #[test]
    fn formatting_an_already_formatted_statement_changes_nothing() {
        let once = format(Engine::Postgres, "select a, b from t where x = 1").unwrap();

        assert_eq!(format(Engine::Postgres, &once).unwrap(), once);
    }

    #[test]
    fn sql_server_formatting_keeps_bracketed_names_and_go_lines() {
        let sql = "select [my col] from t\nGO 5\nselect 2\ngo\n";
        let formatted = format(Engine::SqlServer, sql).unwrap();
        assert!(formatted.contains("[my col]"), "{formatted}");
        assert!(formatted.contains("\nGO 5\n"), "{formatted}");
        assert_eq!(go_lines(&formatted).len(), 2, "{formatted}");
        assert_eq!(format(Engine::SqlServer, &formatted).unwrap(), formatted);
    }

    #[test]
    fn formatting_keeps_the_text_between_statements_as_written() {
        let sql = "\n  select a,b from t;  -- why\n\n\n-- next\nselect 2;\n";
        let formatted = format(Engine::Postgres, sql).unwrap();

        assert_eq!(
            formatted,
            "\nselect\n  a,\n  b\nfrom\n  t;  -- why\n\n\n-- next\nselect\n  2;\n"
        );
    }

    #[test]
    fn statements_on_one_line_are_formatted_onto_lines_of_their_own() {
        assert_eq!(
            format(Engine::Postgres, "select 1; select 2").unwrap(),
            "select\n  1;\nselect\n  2"
        );
    }

    // `Buffer`'s split does not read `\'`, and would cut this literal at its
    // `;`, or run the rest of the buffer into one statement after it.
    #[test]
    fn a_backslash_escaped_literal_keeps_its_text_and_the_lines_after_it_on_mysql() {
        let sql =
            "select 1;\n\ninsert into t values ('Don\\'t; be    late');\n\n\n-- keep\nselect   3;";
        assert_eq!(
            format(Engine::MySql, sql).unwrap(),
            "select\n  1;\n\ninsert into\n  t\nvalues\n  ('Don\\'t; be    late');\n\n\n-- keep\nselect\n  3;"
        );
    }

    #[test]
    fn an_indented_statement_comes_back_flush() {
        assert_eq!(
            format(Engine::Postgres, "select 1;\n    select a from t;").unwrap(),
            "select\n  1;\nselect\n  a\nfrom\n  t;"
        );
    }

    #[test]
    fn a_respaced_token_is_refused_rather_than_formatted() {
        assert!(format(Engine::Sqlite, "select [my col] from t").is_err());
        assert!(format(Engine::MySql, "select 0x0A").is_err());
    }

    #[test]
    fn a_selection_cutting_through_a_literal_or_comment_is_refused() {
        let sql = "insert into t values ('line one\nline   two');\n/* a   b */ select  1;";
        let from = |text: &str| sql.find(text).unwrap();
        // Line by line through a literal that spans two.
        assert!(
            format_selection(Engine::Postgres, sql, from("line   two")..from(";\n/*")).is_err()
        );
        assert!(format_selection(Engine::Postgres, sql, from("b */")..sql.len()).is_err());
        assert!(format_selection(Engine::Postgres, sql, from("ect  1")..sql.len()).is_err());
        assert_eq!(
            format_selection(Engine::Postgres, sql, from("/*")..sql.len()).unwrap(),
            "/* a   b */ select\n  1;"
        );
    }

    #[test]
    fn a_selection_inside_a_dollar_quoted_body_is_refused() {
        for engine in [Engine::Postgres, Engine::Snowflake] {
            let sql = "select $$hello   world$$";
            let start = sql.find("hello").unwrap();
            assert!(format_selection(engine, sql, start..start + 13).is_err());
        }
    }

    #[test]
    fn a_selection_beside_a_dollar_quoted_body_formats() {
        let sql =
            "create function f() returns int as $$ select 1 $$ language sql;\n\nselect a,b from t;";
        let start = sql.find("select a").unwrap();
        assert_eq!(
            format_selection(Engine::Postgres, sql, start..sql.len()).unwrap(),
            "select\n  a,\n  b\nfrom\n  t;"
        );
    }

    #[test]
    fn a_comment_ahead_of_an_indented_statement_stays() {
        assert_eq!(
            format(Engine::Postgres, "select 1;\n/* c */ select 2;").unwrap(),
            "select\n  1;\n/* c */ select\n  2;"
        );
    }

    #[test]
    fn a_mongodb_selection_cutting_through_a_comment_is_refused() {
        let sql = "// old: db.t.find({a:   1})\ndb.t.find({b:   2})";
        let inside = sql.find("db.t").unwrap();
        let next = sql.rfind("db.t").unwrap();
        assert!(format_selection(Engine::MongoDb, sql, inside..sql.len()).is_err());
        assert!(format_selection(Engine::MongoDb, sql, next..sql.len()).is_ok());
    }

    #[test]
    fn only_the_text_up_to_a_selection_must_tokenize() {
        let sql = "select 1 from t;\nselect 'unfinished";
        assert!(format_selection(Engine::Postgres, sql, 0..16).is_ok());
    }

    #[test]
    fn a_flat_statement_gains_line_breaks() {
        let formatted = format(Engine::Postgres, "select a, b from t where x = 1").unwrap();

        assert!(formatted.lines().count() > 1, "{formatted}");
        assert!(formatted.contains("  "), "{formatted}");
    }

    #[test]
    fn a_query_calling_on_its_session_is_no_plain_read() {
        for (engine, statement) in [
            (Engine::Postgres, "SELECT nextval('s')"),
            (
                Engine::Postgres,
                "SELECT pg_catalog.currval('s'), lastval()",
            ),
            (Engine::Postgres, "SELECT setval('s', 5)"),
            (
                Engine::Postgres,
                "SELECT set_config('search_path', 'x', false)",
            ),
            (Engine::Postgres, "SELECT pg_advisory_lock(1)"),
            (Engine::Postgres, "SELECT pg_try_advisory_xact_lock(1)"),
            (Engine::MySql, "SELECT @x := 1"),
            (Engine::MariaDb, "SELECT @n := @n + 1 FROM t"),
            (Engine::MySql, "SELECT LAST_INSERT_ID()"),
            (Engine::MySql, "SELECT GET_LOCK('a', 10)"),
            (Engine::MySql, "SELECT RELEASE_LOCK('a')"),
            (Engine::SqlServer, "SELECT SCOPE_IDENTITY()"),
            (Engine::SqlServer, "SELECT @@IDENTITY"),
        ] {
            assert!(!plain_read(engine, statement), "{statement}");
        }
        assert!(plain_read(Engine::Postgres, "SELECT nextval FROM t"));
        assert!(plain_read(Engine::MySql, "SELECT @x, 1"));
        assert!(plain_read(Engine::SqlServer, "SELECT @@VERSION"));
    }

    #[test]
    fn only_an_open_transaction_or_a_changed_session_is_left_behind() {
        let open = Leaves {
            transaction: Some(true),
            settings: false,
        };
        let closed = Leaves {
            transaction: Some(false),
            settings: false,
        };
        let settings = Leaves {
            transaction: None,
            settings: true,
        };
        for (engine, statement, left) in [
            (Engine::Postgres, "BEGIN", &open),
            (Engine::Postgres, "START TRANSACTION", &open),
            (Engine::MySql, "START TRANSACTION", &open),
            (Engine::SqlServer, "BEGIN TRANSACTION", &open),
            (Engine::Postgres, "COMMIT", &closed),
            (Engine::Postgres, "END", &closed),
            (Engine::MySql, "ROLLBACK", &closed),
            (Engine::Postgres, "COMMIT AND CHAIN", &open),
            (
                Engine::Postgres,
                "BEGIN; UPDATE t SET a = 1; COMMIT",
                &closed,
            ),
            (Engine::Postgres, "SET search_path TO other", &settings),
            (Engine::MySql, "SET autocommit = 0", &settings),
            (Engine::MySql, "SET @x = 1", &settings),
            (Engine::MySql, "USE other", &settings),
            (Engine::SqlServer, "SET NOCOUNT ON", &settings),
            (
                Engine::Postgres,
                "CREATE TEMPORARY TABLE t (a int)",
                &settings,
            ),
            (Engine::SqlServer, "CREATE TABLE #t (a int)", &settings),
            (Engine::SqlServer, "SELECT * INTO #t FROM s", &settings),
            (Engine::MySql, "SELECT 1 INTO @x", &settings),
            (Engine::Sqlite, "PRAGMA foreign_keys = OFF", &settings),
            (
                Engine::Postgres,
                "SELECT set_config('search_path', 'other', false)",
                &settings,
            ),
            (Engine::Postgres, "SELECT pg_advisory_lock(1)", &settings),
            (Engine::MySql, "SELECT GET_LOCK('a', 10)", &settings),
            (Engine::MySql, "SELECT @x := 1", &settings),
        ] {
            assert_eq!(&leaves(engine, statement), left, "{statement}");
        }
        for (engine, statement) in [
            (Engine::Postgres, "SELECT 1"),
            (Engine::Postgres, "SHOW search_path"),
            (Engine::MySql, "DESCRIBE t"),
            (Engine::Postgres, "EXPLAIN SELECT 1"),
            (Engine::Postgres, "SELEC 1"),
            (Engine::Postgres, "INSERT INTO t VALUES (1)"),
            (Engine::MySql, "CREATE TABLE t (a int)"),
            (Engine::Postgres, "SELECT * INTO t2 FROM t"),
            (Engine::Postgres, "ROLLBACK TO SAVEPOINT a"),
            (Engine::Sqlite, "PRAGMA table_info(t)"),
            (Engine::Snowflake, "BEGIN"),
            (Engine::MongoDb, "db.a.insertOne({ x: 1 })"),
        ] {
            assert_eq!(leaves(engine, statement), Leaves::default(), "{statement}");
        }
    }

    #[test]
    fn insert_rows_writes_one_multi_row_insert_in_each_engines_spelling() {
        use crate::import_rows::Value;
        let (one, two, it, yes, no) = (
            Value::Number("1".into()),
            Value::Number("2".into()),
            Value::Text("it's".into()),
            Value::Bool(true),
            Value::Bool(false),
        );
        let cols = [("id", "integer"), ("note", "text"), ("ok", "boolean")];
        assert_eq!(
            insert_rows(
                Engine::Postgres,
                "public",
                "t",
                &cols,
                &[
                    vec![Some(&one), Some(&it), Some(&yes)],
                    vec![Some(&two), Some(&Value::Null), Some(&no)]
                ],
                true
            )
            .unwrap(),
            "INSERT INTO \"public\".\"t\" (\"id\", \"note\", \"ok\") VALUES ('1', 'it''s', 'true'),\n('2', NULL, 'false') ON CONFLICT DO NOTHING"
        );
        assert_eq!(
            insert_rows(
                Engine::MySql,
                "d",
                "t",
                &cols[..2],
                &[vec![Some(&one), Some(&Value::Text("a\\b".into()))]],
                true
            )
            .unwrap(),
            "INSERT INTO `d`.`t` (`id`, `note`) VALUES ('1', 'a\\\\b') ON DUPLICATE KEY UPDATE `id` = `id`"
        );
        assert_eq!(
            insert_rows(
                Engine::SqlServer,
                "dbo",
                "t",
                &[("note", "nvarchar(10)"), ("ok", "bit")],
                &[vec![Some(&Value::Text("é".into())), Some(&yes)]],
                false
            )
            .unwrap(),
            "INSERT INTO \"dbo\".\"t\" (\"note\", \"ok\") VALUES (N'é', '1')"
        );
        assert_eq!(
            insert_rows(
                Engine::MongoDb,
                "db",
                "t",
                &cols[..2],
                &[
                    vec![Some(&one), Some(&Value::Text("x".into()))],
                    vec![Some(&two), None]
                ],
                false
            )
            .unwrap(),
            "db.getCollection(\"t\").insertMany([{\"id\": 1, \"note\": \"x\"},\n{\"id\": 2}])"
        );
    }

    #[test]
    fn insert_rows_quotes_every_name() {
        let v = crate::import_rows::Value::Text("x".into());
        assert_eq!(
            insert_rows(
                Engine::Postgres,
                "public",
                "t",
                &[("order", "text"), ("a\"b", "text")],
                &[vec![Some(&v), Some(&v)]],
                false
            )
            .unwrap(),
            "INSERT INTO \"public\".\"t\" (\"order\", \"a\"\"b\") VALUES ('x', 'x')"
        );
    }

    #[test]
    fn every_engines_import_statements_pass_the_write_gate_and_the_mode_check() {
        let v = crate::import_rows::Value::Text("x".into());
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::Turso,
            Engine::DuckDb,
            Engine::Snowflake,
            Engine::SqlServer,
            Engine::MongoDb,
        ] {
            let statement = insert_rows(
                engine,
                "s",
                "t",
                &[("a", "text")],
                &[vec![Some(&v)], vec![Some(&v)]],
                engine.skips_duplicates(),
            )
            .unwrap();
            assert!(
                is_generated_write_on(engine, &statement),
                "{engine:?}: {statement}"
            );
            assert_eq!(
                gate(&classify(engine, &statement), Mode::ReadWrite, &[]),
                None,
                "{engine:?}: {statement}"
            );
            assert_eq!(
                engine.skips_duplicates(),
                statement.contains(" ON "),
                "{engine:?}"
            );
            if let Ok(begin) = engine.all_or_nothing() {
                for bracket in [begin, "COMMIT", "ROLLBACK"] {
                    assert!(is_generated_write_on(engine, bracket));
                    assert_eq!(
                        gate(&classify(engine, bracket), Mode::ReadOnly, &[]),
                        None,
                        "{engine:?} {bracket}"
                    );
                }
            }
        }
    }
}

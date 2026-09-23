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
    AlterTableOperation, CopySource, CopyTarget, Query, SetExpr, Statement, UtilityOption,
};
use sqlparser::dialect::{
    Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
};
use sqlparser::parser::Parser as SqlParser;
use tree_sitter::{Node, Parser, Tree};

use crate::db::Engine;
use crate::result_grid::{NewValue, PendingRow};

/// The runnable statements of a query buffer, as byte ranges into it.
pub struct Buffer {
    statements: Vec<Range<usize>>,
}

impl Buffer {
    pub fn parse(sql: &str) -> Self {
        let mut parser = Parser::new();
        let statements = parser
            .set_language(&tree_sitter_sequel::LANGUAGE.into())
            .ok()
            .and_then(|_| parser.parse(sql, None))
            .map(|tree| collect_statements(&tree, sql))
            .unwrap_or_default();

        Self { statements }
    }

    /// Byte ranges of each statement, in source order, trimmed of surrounding
    /// whitespace. Empty if the buffer holds no statements.
    #[cfg(test)]
    pub fn statements(&self) -> &[Range<usize>] {
        &self.statements
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
pub fn order_by(statement: &str) -> Option<Vec<SortKey>> {
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
pub fn with_order_by(statement: &str, keys: &[SortKey]) -> Option<String> {
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
        return Some(splice(sql, existing.byte_range(), &clause));
    }

    if clause.is_empty() {
        return Some(sql.to_string());
    }

    let insert_at = child_of_kind(&anchor, "limit")
        .map(|limit| limit.byte_range().start)
        .unwrap_or(anchor.byte_range().end);

    Some(splice(sql, insert_at..insert_at, &clause))
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
/// `None` when either list is empty. A statement with no `WHERE` rewrites every
/// row in the table and one with no `SET` is not a statement at all, so a caller
/// that has lost the row's key gets nothing to run rather than something that
/// runs.
pub fn update_row(
    engine: Engine,
    schema: &str,
    table: &str,
    sets: &[(&str, NewValue)],
    keys: &[(&str, &str)],
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
        engine.qualified(schema, table),
        assignments(engine, sets, ", "),
        assignments(engine, &keys, " AND ")
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
/// `None` on an empty list. The alternative is `INSERT INTO t DEFAULT VALUES`,
/// a statement nobody has asked dbdelve for.
pub fn insert_row(
    engine: Engine,
    schema: &str,
    table: &str,
    columns: &[(&str, Option<&str>)],
) -> Option<String> {
    if columns.is_empty() {
        return None;
    }

    let names: Vec<String> = columns
        .iter()
        .map(|&(column, _)| engine.quote_identifier(column))
        .collect();
    let values: Vec<String> = columns
        .iter()
        // An insert leaves a default to apply by omitting the column outright,
        // so the third state the grid's edits carry has nothing to mean here.
        .map(|&(_, value)| {
            literal(
                engine,
                &value.map_or(NewValue::Null, |value| NewValue::Value(value.into())),
            )
        })
        .collect();
    Some(format!(
        "INSERT INTO {} ({}) VALUES ({})",
        engine.qualified(schema, table),
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
        engine.qualified(schema, table),
        assignments(engine, &keys, " AND ")
    ))
}

/// Whether `sql` is a statement dbdelve could have written: one or more `UPDATE`s,
/// a single `INSERT`, or a single `DELETE` naming one row, and nothing else at
/// all.
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
pub fn is_generated_write(sql: &str) -> bool {
    let Some(tree) = parse(sql) else {
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
        return delete_key_columns(sql).is_some();
    }

    // One insert alone, or a batch of updates. A batch of inserts is a shape
    // nothing generates, so admitting it would widen the gate for nobody.
    (kinds == ["insert"] || (!kinds.is_empty() && kinds.iter().all(|kind| *kind == "update")))
        && !deletes_anything(root)
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
pub fn delete_matches_key(sql: &str, keys: &[&str]) -> bool {
    let Some(columns) = delete_key_columns(sql) else {
        return false;
    };
    columns.len() == keys.len() && keys.iter().all(|key| columns.iter().any(|c| c == key))
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
pub fn is_generated_select(sql: &str) -> bool {
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
    let bracketed: Vec<_> = transaction.named_children(&mut cursor).collect();
    match bracketed.as_slice() {
        [begin, statements @ .., commit]
            if begin.kind() == "keyword_begin" && commit.kind() == "keyword_commit" =>
        {
            Some(statements.to_vec())
        }
        _ => None,
    }
}

fn assignments(engine: Engine, columns: &[(&str, NewValue)], separator: &str) -> String {
    columns
        .iter()
        .map(|(column, value)| {
            format!(
                "{} = {}",
                engine.quote_identifier(column),
                literal(engine, value)
            )
        })
        .collect::<Vec<_>>()
        .join(separator)
}

/// A value as it goes into a statement: quoted, or one of the two keywords that
/// stand for there being no value to quote. Unquoted is the only way to write
/// either — `'NULL'` and `'DEFAULT'` are the words, and a user who typed one of
/// them into a cell meant the word.
fn literal(engine: Engine, value: &NewValue) -> String {
    match value {
        NewValue::Value(value) => engine.quote_literal(value),
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
fn delete_key_columns(sql: &str) -> Option<Vec<String>> {
    let tree = parse(sql)?;
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
            let Some(value) = sql.get(right.byte_range()) else {
                return false;
            };
            // A value is a single-quoted literal and nothing else. `"other"` is
            // a `literal` to this grammar too, and matching a column against a
            // column is not naming a row.
            if right.kind() != "literal" || !value.starts_with('\'') {
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
fn splice(sql: &str, range: Range<usize>, clause: &str) -> String {
    let head = sql[..range.start].trim_end();
    let tail = sql[range.end..].trim_start();

    let mut spliced = String::with_capacity(head.len() + clause.len() + tail.len() + 2);
    spliced.push_str(head);
    for part in [clause, tail] {
        if part.is_empty() {
            continue;
        }
        if !spliced.is_empty() {
            spliced.push(' ');
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
            b'/' if rest.starts_with("/*") => rest.find("*/").map_or(rest.len(), |end| end + 2),
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

/// Every pending row as one `UPDATE`, joined into a single string.
///
/// All-or-nothing, which each engine reaches differently, and
/// `Engine::transaction_start` is where that per-engine answer lives. Postgres
/// runs one submission as a single implicit transaction and needs nothing;
/// MySQL and SQLite commit every statement on its own, so a batch of more than
/// one is bracketed — in the statement text itself, where the user can read,
/// edit and undo it, because dbdelve does not open a transaction behind anyone's
/// back.
///
/// `None` when there is nothing to apply, and `None` — rather than a shorter
/// batch — when any one row cannot be written: a partial apply is not the change
/// the user made, and dbdelve would have no way to say which part of it ran.
pub(crate) fn update_batch(engine: Engine, rows: &[PendingRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
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
            update_row(
                engine,
                &row.schema,
                &row.table,
                &borrowed_sets(&row.sets),
                &borrowed(&row.keys),
            )
            // Terminated, not separated: the last statement carries its
            // semicolon too, so appending to a buffer cannot fuse it onto
            // whatever the user writes next.
            .map(|statement| format!("{statement};"))
        })
        .collect();

    let batch = statements?.join("\n");
    let bracket = engine.transaction_start().filter(|_| rows.len() > 1);
    Some(match bracket {
        Some(start) => format!("{start};\n{batch}\nCOMMIT;"),
        None => batch,
    })
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
pub(crate) fn appended_statement(buffer: &str, statement: &str) -> String {
    let text = buffer.trim_end();
    if text.is_empty() {
        return statement.to_string();
    }

    let terminator = match text.ends_with(';') {
        true => "",
        false => ";",
    };
    format!("{text}{terminator}\n\n{statement}")
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
    const READ: Self = Self {
        mode: Mode::ReadOnly,
        destructive: Vec::new(),
    };
    const WRITE: Self = Self {
        mode: Mode::ReadWrite,
        destructive: Vec::new(),
    };
    const FULL: Self = Self {
        mode: Mode::Full,
        destructive: Vec::new(),
    };

    fn destroys(kind: Destructive) -> Self {
        Self {
            mode: Mode::Full,
            destructive: vec![kind],
        }
    }

    /// The more restrictive of two verdicts: the higher mode, and the union of
    /// what they destroy. A kind only ever arrives with `Mode::Full`, so taking
    /// the union never smuggles a destructive kind under a lower mode.
    fn max(mut self, other: Self) -> Self {
        self.mode = self.mode.max(other.mode);
        for kind in other.destructive {
            if !self.destructive.contains(&kind) {
                self.destructive.push(kind);
            }
        }
        self
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
    let dialect: Box<dyn Dialect> = match engine {
        Engine::Postgres => Box::new(PostgreSqlDialect {}),
        Engine::MySql => Box::new(MySqlDialect {}),
        Engine::Sqlite => Box::new(SQLiteDialect {}),
        Engine::Snowflake => Box::new(SnowflakeDialect {}),
    };

    // All or nothing: one statement it cannot read makes the whole submission
    // one it cannot vouch for.
    let Ok(statements) = SqlParser::parse_sql(dialect.as_ref(), sql) else {
        return Verdict::destroys(Destructive::Unreadable);
    };

    // An empty or comment-only string parses to no statements at all. There is
    // nothing there to be dangerous, and a dialog over nothing is noise.
    statements
        .iter()
        .map(statement_verdict)
        .fold(Verdict::READ, Verdict::max)
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
    if verdict.destructive.contains(&Destructive::Unreadable) {
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

/// `sql` laid out over several lines: the same tokens, indented. `None` when
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
// ponytail: `FormatOptions` carries a `dialect`, deliberately left at Generic.
// Plumbing the connection's real engine through is the upgrade path if the
// output ever looks wrong for a specific backend.
pub(crate) fn format(sql: &str) -> Option<String> {
    if has_dollar_quote(sql) {
        return None;
    }
    Some(sqlformat::format(
        sql,
        &sqlformat::QueryParams::default(),
        &sqlformat::FormatOptions::default(),
    ))
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
            "SELECT * FROM t",
            &[SortKey::new(r#""a""#, true), SortKey::new("3", false)],
        )
        .unwrap();

        assert_eq!(sorted, r#"SELECT * FROM t ORDER BY "a" ASC, 3 DESC"#);
        assert_eq!(
            order_by(&sorted).unwrap(),
            vec![SortKey::new(r#""a""#, true), SortKey::new("3", false)]
        );
    }

    #[test]
    fn sorting_again_replaces_the_clause_it_wrote() {
        let once = with_order_by("SELECT * FROM t LIMIT 5", &[SortKey::new("a", true)]).unwrap();
        let twice = with_order_by(&once, &[SortKey::new("b", false)]).unwrap();

        assert_eq!(twice, "SELECT * FROM t ORDER BY b DESC LIMIT 5");
        // And clearing it leaves the statement as it was, not a hole.
        assert_eq!(
            with_order_by(&twice, &[]).unwrap(),
            "SELECT * FROM t LIMIT 5"
        );
    }

    #[test]
    fn a_key_the_user_wrote_reads_back_verbatim() {
        let keys = order_by("SELECT * FROM t ORDER BY lower(name), 2 DESC").unwrap();

        assert_eq!(
            keys,
            vec![SortKey::new("lower(name)", true), SortKey::new("2", false)]
        );
    }

    #[test]
    fn a_union_sorts_at_the_end_of_the_whole_query() {
        assert_eq!(
            with_order_by(
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

        assert_eq!(order_by(sql).unwrap(), vec![]);
        assert_eq!(
            with_order_by(sql, &[SortKey::new("a", false)]).unwrap(),
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
            assert!(order_by(sql).is_none(), "{sql} should not be sortable");
            assert!(with_order_by(sql, &[]).is_none(), "{sql} was spliced");
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
            assert!(order_by(sql).is_none(), "{sql} should not be sortable");
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
            assert!(order_by(sql).is_none(), "{sql} reported a sort");
            assert!(
                with_order_by(sql, &[SortKey::new("a", true)]).is_none(),
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

    #[test]
    fn semicolon_inside_a_string_literal_does_not_split() {
        // The case that breaks every naive splitter.
        assert_eq!(
            texts("SELECT ';' AS sep;\nSELECT 2;"),
            vec!["SELECT ';' AS sep", "SELECT 2"]
        );
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
            assert!(order_by(sql).is_none(), "{sql} reported a sort");
            assert!(
                with_order_by(sql, &[SortKey::new("a", true)]).is_none(),
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
                "SELECT * FROM t WHERE note LIKE 'a  %' LIMIT 10",
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            "SELECT * FROM t WHERE note LIKE 'a  %' ORDER BY id ASC LIMIT 10"
        );
        assert_eq!(
            with_order_by(
                r#"SELECT * FROM "public"."my  table""#,
                &[SortKey::new("id", true)]
            )
            .unwrap(),
            r#"SELECT * FROM "public"."my  table" ORDER BY id ASC"#
        );
        assert_eq!(
            with_order_by(
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
                &[("id", "7")]
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
                &[("id", "7")]
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
                &[("org_id", "1"), ("user_id", "2")]
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
                &[("id", "o'hara")]
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
                &[("id", "1")]
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
                &[("id", "7")]
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
                &[("id", "7")]
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
                &[("id", "7")]
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
                &[("id", "7")]
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
                &[("id", "7")]
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
        )
        .unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
    }

    #[test]
    fn an_update_with_nothing_to_match_on_is_refused() {
        // No WHERE rewrites every row in the table. It must not be possible to
        // produce that statement, so a caller with no key gets nothing.
        assert!(update_row(Engine::Postgres, "s", "t", &[("a", set("1"))], &[]).is_none());
        assert!(update_row(Engine::Postgres, "s", "t", &[], &[("id", "1")]).is_none());
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
                &[("note", Some("ok")), ("depth", None)]
            )
            .unwrap(),
            r#"INSERT INTO "public"."measurements" ("note", "depth") VALUES ('ok', NULL)"#
        );
        assert_eq!(
            insert_row(Engine::Sqlite, "main", "t", &[("a", Some("o'hara"))]).unwrap(),
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
                &[("no`te", Some(r"a\'b"))]
            )
            .unwrap(),
            r"INSERT INTO `dbdelve_dev`.`me``as` (`no``te`) VALUES ('a\\''b')"
        );
        // An empty form is not `INSERT INTO t DEFAULT VALUES`, which is a
        // statement dbdelve has never been asked for.
        assert!(insert_row(Engine::Postgres, "s", "t", &[]).is_none());
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
        // being harmless is not the test, being one of the three shapes dbdelve
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
            assert!(is_generated_select(sql), "{sql} was refused");
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
            assert!(!is_generated_select(sql), "{sql} passed the gate");
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
            assert!(!is_generated_select(sql), "{sql:?} passed the gate");
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
            assert!(!is_generated_select(sql), "{sql} passed the gate");
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
            assert!(!is_generated_select(sql), "{sql} passed the gate");
        }
    }

    #[test]
    fn a_generated_delete_names_the_row_and_only_the_row() {
        assert_eq!(
            delete_row(Engine::Postgres, "public", "measurements", &[("id", "7")]).unwrap(),
            r#"DELETE FROM "public"."measurements" WHERE "id" = '7'"#
        );
        // Joined by OR, or with a column dropped, this deletes rows the user
        // never pointed at.
        assert_eq!(
            delete_row(
                Engine::Postgres,
                "app",
                "memberships",
                &[("org_id", "1"), ("user_id", "2")]
            )
            .unwrap(),
            r#"DELETE FROM "app"."memberships" WHERE "org_id" = '1' AND "user_id" = '2'"#
        );
        assert_eq!(
            delete_row(
                Engine::MySql,
                "dbdelve_demo",
                "measurements",
                &[("id", "7")]
            )
            .unwrap(),
            "DELETE FROM `dbdelve_demo`.`measurements` WHERE `id` = '7'"
        );
        assert_eq!(
            delete_row(Engine::Sqlite, "main", "t", &[("id", "o'hara")]).unwrap(),
            r#"DELETE FROM "main"."t" WHERE "id" = 'o''hara'"#
        );
        // No WHERE empties the table, so it must not be possible to produce.
        assert!(delete_row(Engine::Postgres, "s", "t", &[]).is_none());
    }

    #[test]
    fn the_gate_admits_the_delete_dbdelve_writes_and_reads_its_key_back() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let statement = delete_row(engine, "s", "t", &[("id", "7")]).unwrap();
            assert!(is_generated_write(&statement), "{statement} was refused");
            assert!(delete_matches_key(&statement, &["id"]), "{statement}");

            let composite =
                delete_row(engine, "s", "t", &[("org_id", "1"), ("user_id", "2")]).unwrap();
            assert!(is_generated_write(&composite), "{composite} was refused");
            assert!(delete_matches_key(&composite, &["org_id", "user_id"]));
            // Set equality: the key is a set of columns, not a sequence.
            assert!(delete_matches_key(&composite, &["user_id", "org_id"]));
        }
        // A value carrying the quote character still reads back.
        let statement = delete_row(Engine::Postgres, "s", "t", &[("id", "o'hara")]).unwrap();
        assert!(is_generated_write(&statement), "{statement} was refused");
        assert!(delete_matches_key(&statement, &["id"]));

        // A column name carrying one does not: `"we""ird"` is two adjacent
        // strings to this grammar and the whole statement fails to parse, so
        // the gate refuses dbdelve's own output. That is the safe direction --
        // the row stays -- and a gate that guessed past an unreadable tree is
        // the unsafe one.
        let odd = delete_row(Engine::Postgres, "s", "t", &[(r#"we"ird"#, "x")]).unwrap();
        assert!(!is_generated_write(&odd), "{odd} passed the gate");
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
                &[]
            )
            .is_none()
        );
        assert_eq!(update_batch(Engine::Postgres, &rows), None);
    }

    #[test]
    fn an_empty_batch_of_rows_has_nothing_to_send() {
        assert_eq!(update_batch(Engine::Postgres, &[]), None);
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
            let appended = appended_statement(buffer, recalled);
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
            appended_statement("SELECT 1", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn an_already_terminated_buffer_keeps_a_single_semicolon() {
        assert_eq!(
            appended_statement("SELECT 1;", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn an_empty_buffer_yields_just_the_statement() {
        assert_eq!(
            appended_statement("", "UPDATE t SET a = 1"),
            "UPDATE t SET a = 1"
        );
    }

    #[test]
    fn trailing_whitespace_in_the_buffer_does_not_ragged_the_join() {
        assert_eq!(
            appended_statement("SELECT 1\n\n  ", "UPDATE t SET a = 1"),
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
            ("SELCT 1", Mode::Full, Some(Destructive::Unreadable)),
            (
                "DO $$ BEGIN NULL; END $$",
                Mode::Full,
                Some(Destructive::Unreadable),
            ),
            // A normal thing to type at a SQLite database, and the crate does not
            // accept it under any dialect -- spec §3.4, and the reason shape C
            // exists at all.
            (
                "PRAGMA table_info(t)",
                Mode::Full,
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
        let unreadable = Verdict {
            mode: Mode::Full,
            destructive: vec![Destructive::Unreadable],
        };

        // Not Upgrade, in any mode: a typo must never ask to raise a connection to
        // Full in order to receive a syntax error.
        for mode in Mode::ALL {
            assert_eq!(
                gate(&unreadable, mode, &[]),
                Some(Stop::RunOnce),
                "{mode:?}"
            );
        }

        // Not suppressible even if something contrived writes it into the list.
        assert_eq!(
            gate(&unreadable, Mode::Full, &[Destructive::Unreadable]),
            Some(Stop::RunOnce)
        );
    }

    #[test]
    fn a_comment_the_user_wrote_survives_formatting() {
        // The whole reason formatting is token-level: an AST round trip would
        // drop this line, and the user would not get it back.
        let formatted = format("select a -- the one we care about\nfrom t").unwrap();

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
        assert_eq!(format("DO $$ BEGIN DELETE FROM t; END $$"), None);
        assert_eq!(format("select $tag$ x; y $tag$"), None);
    }

    // A placeholder is not a quote, and reading it as one would refuse to format
    // every parameterised statement anybody writes.
    #[test]
    fn a_numbered_placeholder_still_formats() {
        assert!(format("select a from t where id = $1").is_some());
    }

    #[test]
    fn formatting_an_already_formatted_statement_changes_nothing() {
        let once = format("select a, b from t where x = 1").unwrap();

        assert_eq!(format(&once).unwrap(), once);
    }

    #[test]
    fn a_flat_statement_gains_line_breaks() {
        let formatted = format("select a, b from t where x = 1").unwrap();

        assert!(formatted.lines().count() > 1, "{formatted}");
        assert!(formatted.contains("  "), "{formatted}");
    }
}

//! Browsing a collection: the statements an object tab generates and the sort
//! a header click splices into a buffer. The Mongo arms of
//! `explorer::preview_sql`, `sql::is_generated_select`, `sql::order_by` and
//! `sql::with_order_by` end here.
//!
//! A sort is spliced by editing the statement's text at the byte spans the
//! parse found, never by printing the parse back out, so every comment and
//! every spelling the user chose outside the sort survives.

use std::ops::Range;

use super::{CursorMethod, Method, Statement, Target, Value, classify, integer, parse};
use crate::sql::{SortKey, Verdict};

/// A JavaScript string literal, which a JSON string always is.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

/// Whether a field can be named in a generated filter or sort. A `.` names a
/// path into a subdocument there and a leading `$` an operator, so a top-level
/// field spelled with either cannot be named at all; an empty name is refused
/// with them rather than sent.
fn nameable(field: &str) -> bool {
    !field.is_empty() && !field.contains('.') && !field.starts_with('$')
}

/// How a header click names a field in a sort: `filter::sort_expression`'s
/// Mongo arm. `None` for a field a sort document cannot name.
pub(crate) fn sort_field(field: &str) -> Option<String> {
    nameable(field).then(|| quoted(field))
}

/// A sort document's keys, or `None` for one that is not a plain list of
/// fields each `1` or `-1` -- a `$meta` sort, or `"asc"` -- since a header
/// click would have to reprint it.
fn sort_keys(value: &Value) -> Option<Vec<SortKey>> {
    let Value::Document(fields) = value else {
        return None;
    };
    fields
        .iter()
        .map(|(name, direction)| {
            let ascending = match integer(direction)? {
                1 => true,
                -1 => false,
                _ => return None,
            };
            Some(SortKey::new(quoted(name), ascending))
        })
        .collect()
}

/// `keys` as a sort document. `None` for a key that is not a quoted field
/// name -- the positional key `filter::sort_expression` falls back to names
/// nothing here -- or names an operator.
fn sort_document(keys: &[SortKey]) -> Option<String> {
    let fields = keys
        .iter()
        .map(|key| {
            let name: String = serde_json::from_str(key.expression.trim()).ok()?;
            let direction = match key.ascending {
                true => 1,
                false => -1,
            };
            (!name.is_empty() && !name.starts_with('$'))
                .then(|| format!("{}: {direction}", quoted(&name)))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("{{{}}}", fields.join(", ")))
}

/// Where a statement's sort stands, or would.
enum Site {
    /// `find`'s `.sort(…)`: the call from its `.`, and its one argument.
    Call {
        call: Range<usize>,
        argument: Range<usize>,
        keys: Value,
    },
    /// A pipeline's last stage, when it is `{$sort: …}`.
    Stage { stage: Range<usize>, keys: Value },
    /// Where a sort goes on a statement without one: after `find(…)`, or as a
    /// pipeline's last stage (`first` when the pipeline is empty).
    Absent { at: usize, stage: bool, first: bool },
}

/// Only a `find` or an `aggregate` that reads, read whole and alone. Anything
/// else -- a write, a command, an `explain`, a statement that does not parse
/// -- has no sort this will touch, as `sql::with_order_by` refuses what it
/// cannot parse cleanly.
fn site(text: &str) -> Option<Site> {
    if classify(text) != Verdict::READ {
        return None;
    }
    let statements = parse(text).ok()?;
    let [
        Statement {
            target: Target::Collection { call, cursor, .. },
            ..
        },
    ] = statements.as_slice()
    else {
        return None;
    };
    if cursor
        .iter()
        .any(|link| link.method == CursorMethod::Explain)
    {
        return None;
    }
    let sorts: Vec<_> = cursor
        .iter()
        .filter(|link| link.method == CursorMethod::Sort)
        .collect();
    match call.method {
        Method::Find => match sorts.as_slice() {
            [] => Some(Site::Absent {
                at: call.span.end,
                stage: false,
                first: false,
            }),
            [sort] => match sort.args.as_slice() {
                [argument] => Some(Site::Call {
                    call: sort.span.clone(),
                    argument: argument.span.clone(),
                    keys: argument.value.clone(),
                }),
                _ => None,
            },
            // The later one wins in mongosh; which one a click should edit is
            // a guess.
            _ => None,
        },
        // `aggregate`'s cursor has no `sort`; one chained there is an error
        // the server should report, not one to edit around.
        Method::Aggregate if sorts.is_empty() => {
            let pipeline = call.args.first()?;
            let Value::Array(stages) = &pipeline.value else {
                return None;
            };
            match (stages.last(), pipeline.items.last()) {
                (Some(Value::Document(fields)), Some(stage))
                    if fields.len() == 1 && fields[0].0 == "$sort" =>
                {
                    Some(Site::Stage {
                        stage: stage.clone(),
                        keys: fields[0].1.clone(),
                    })
                }
                (_, Some(stage)) => Some(Site::Absent {
                    at: stage.end,
                    stage: true,
                    first: false,
                }),
                (_, None) => Some(Site::Absent {
                    at: pipeline.span.start + 1,
                    stage: true,
                    first: true,
                }),
            }
        }
        _ => None,
    }
}

/// `sql::order_by` for a mongosh statement: the keys of a `find`'s `.sort(…)`
/// or an `aggregate`'s trailing `$sort` stage. `Some(empty)` for one that
/// could carry a sort and does not; `None` for anything this cannot say
/// without guessing.
pub(crate) fn order_by(text: &str) -> Option<Vec<SortKey>> {
    match site(text)? {
        Site::Call { keys, .. } | Site::Stage { keys, .. } => sort_keys(&keys),
        Site::Absent { .. } => Some(Vec::new()),
    }
}

/// `sql::with_order_by` for a mongosh statement: `text` sorted by `keys`, or
/// with its sort removed when `keys` is empty.
///
/// A `find`'s existing `.sort(…)` has its argument replaced, or a `.sort(…)`
/// goes directly after `find(…)`. An `aggregate`'s trailing `$sort` stage is
/// replaced, or one is appended. Nothing else in the text moves.
///
/// The result is read back before it is returned, so a splice that landed
/// anywhere but where it was aimed is refused rather than run.
pub(crate) fn with_order_by(text: &str, keys: &[SortKey]) -> Option<String> {
    let document = sort_document(keys)?;
    let stage = format!("{{\"$sort\": {document}}}");
    let mut sorted = text.to_owned();
    match (site(text)?, keys.is_empty()) {
        (Site::Absent { .. }, true) => return Some(sorted),
        (
            Site::Absent {
                at, stage: false, ..
            },
            false,
        ) => {
            sorted.insert_str(at, &format!(".sort({document})"));
        }
        (Site::Absent { at, first, .. }, false) => {
            let separator = if first { "" } else { ", " };
            sorted.insert_str(at, &format!("{separator}{stage}"));
        }
        (Site::Call { argument, .. }, false) => sorted.replace_range(argument, &document),
        (Site::Stage { stage: span, .. }, false) => sorted.replace_range(span, &stage),
        (Site::Call { call, .. }, true) => sorted.replace_range(own_line(text, call), ""),
        (Site::Stage { stage, .. }, true) => sorted.replace_range(listed(text, stage)?, ""),
    }
    (sort_document(&order_by(&sorted)?)? == document).then_some(sorted)
}

/// `span`, widened to its whole line when nothing else stands on it, so a
/// chained call removed from a line of its own leaves no blank line behind.
fn own_line(text: &str, span: Range<usize>) -> Range<usize> {
    let Some(newline) = text[..span.start].rfind('\n') else {
        return span;
    };
    let alone = text[newline + 1..span.start].trim().is_empty()
        && text[span.end..]
            .split('\n')
            .next()
            .unwrap_or_default()
            .trim()
            .is_empty();
    match (alone, text[..newline].ends_with('\r')) {
        (true, true) => newline - 1..span.end,
        (true, false) => newline..span.end,
        (false, _) => span,
    }
}

/// A pipeline's last stage's span, widened to take one comma beside it: the
/// one before it, else a trailing one after it (and its line, when nothing
/// else is left on it). `None` when a comment stands between the stage and
/// every comma and the stage is not alone in the pipeline, since which comma
/// belongs to it is then a guess.
fn listed(text: &str, stage: Range<usize>) -> Option<Range<usize>> {
    let before = text[..stage.start].trim_end();
    if before.ends_with(',') {
        return Some(before.len() - 1..stage.end);
    }
    let after = text[stage.end..].trim_start();
    let next = text.len() - after.len();
    match after.chars().next() {
        Some(',') => Some(own_line(text, stage.start..next + 1)),
        Some(']') if before.ends_with('[') => Some(stage),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, ascending: bool) -> SortKey {
        SortKey::new(quoted(name), ascending)
    }

    fn sorted(text: &str, keys: &[SortKey]) -> String {
        with_order_by(text, keys).unwrap_or_else(|| panic!("{text} was refused"))
    }

    #[test]
    fn a_header_names_a_field_a_sort_can_name_and_no_other() {
        assert_eq!(sort_field("name").as_deref(), Some("\"name\""));
        assert_eq!(sort_field("say \"hi\"").as_deref(), Some(r#""say \"hi\"""#));
        // A dot is a path into a subdocument and a `$` an operator, so a
        // top-level field spelled with either would sort by something else.
        for field in ["a.b", "$x", ""] {
            assert_eq!(sort_field(field), None, "{field}");
        }
    }

    #[test]
    fn a_find_gets_its_sort_directly_after_find() {
        assert_eq!(
            sorted("db.c.find({a: 1}).limit(5)", &[key("b", true)]),
            r#"db.c.find({a: 1}).sort({"b": 1}).limit(5)"#
        );
        assert_eq!(
            sorted(
                "db.getCollection('c').find()",
                &[key("b", false), key("a", true)]
            ),
            r#"db.getCollection('c').find().sort({"b": -1, "a": 1})"#
        );
    }

    #[test]
    fn an_existing_sort_is_replaced_in_place_and_comments_survive() {
        let text = "// newest first\ndb.c.find({ /* all */ })\n  // the order\n  .sort({ at: -1 }) // here\n  .limit(5) // five";
        assert_eq!(order_by(text), Some(vec![key("at", false)]));
        assert_eq!(
            sorted(text, &[key("at", false), key("name", true)]),
            "// newest first\ndb.c.find({ /* all */ })\n  // the order\n  .sort({\"at\": -1, \"name\": 1}) // here\n  .limit(5) // five"
        );
        // A sort written before the limit or after it is the same sort.
        assert_eq!(
            sorted("db.c.find().limit(5).sort({a: 1})", &[key("a", false)]),
            r#"db.c.find().limit(5).sort({"a": -1})"#
        );
    }

    #[test]
    fn no_keys_takes_the_sort_out_and_its_line_with_it() {
        assert_eq!(
            sorted("db.c.find().sort({a: 1}).limit(5)", &[]),
            "db.c.find().limit(5)"
        );
        assert_eq!(
            sorted("db.c.find()\n  .sort({a: 1})\n  .limit(5)", &[]),
            "db.c.find()\n  .limit(5)"
        );
        assert_eq!(
            sorted("db.c.find()\r\n  .sort({a: 1})\r\n  .limit(5)", &[]),
            "db.c.find()\r\n  .limit(5)"
        );
        // Not the line when something else stands on it.
        assert_eq!(
            sorted("db.c.find()\n  .sort({a: 1}) // by a\n  .limit(5)", &[]),
            "db.c.find()\n   // by a\n  .limit(5)"
        );
        assert_eq!(sorted("db.c.find({a: 1})", &[]), "db.c.find({a: 1})");
    }

    #[test]
    fn a_pipeline_gets_a_trailing_sort_stage() {
        assert_eq!(
            sorted("db.c.aggregate([{ $match: {} }])", &[key("a", true)]),
            r#"db.c.aggregate([{ $match: {} }, {"$sort": {"a": 1}}])"#
        );
        assert_eq!(
            sorted("db.c.aggregate([])", &[key("a", true)]),
            r#"db.c.aggregate([{"$sort": {"a": 1}}])"#
        );
    }

    #[test]
    fn a_trailing_sort_stage_is_replaced_and_removed_with_its_comma() {
        let text = "db.c.aggregate([\n  { $match: {} }, // all\n  { $sort: { a: 1 } },\n])";
        assert_eq!(order_by(text), Some(vec![key("a", true)]));
        assert_eq!(
            sorted(text, &[key("b", false)]),
            "db.c.aggregate([\n  { $match: {} }, // all\n  {\"$sort\": {\"b\": -1}},\n])"
        );
        assert_eq!(
            sorted(text, &[]),
            "db.c.aggregate([\n  { $match: {} }, // all\n])"
        );
        assert_eq!(
            sorted("db.c.aggregate([{$sort: {a: 1}}, ])", &[]),
            "db.c.aggregate([ ])"
        );
        assert_eq!(
            sorted("db.c.aggregate([{$sort: {a: 1}}])", &[]),
            "db.c.aggregate([])"
        );
        // A `$sort` earlier in the pipeline is the user's, and a new one goes
        // after everything.
        assert_eq!(
            sorted(
                "db.c.aggregate([{$sort: {a: 1}}, {$limit: 5}])",
                &[key("b", true)]
            ),
            r#"db.c.aggregate([{$sort: {a: 1}}, {$limit: 5}, {"$sort": {"b": 1}}])"#
        );
        // A comment between the stage and its comma: which comma goes is a
        // guess.
        assert_eq!(
            with_order_by(
                "db.c.aggregate([{$match: {}}, /* x */ {$sort: {a: 1}}])",
                &[]
            ),
            None
        );
    }

    #[test]
    fn what_is_not_a_reading_find_or_aggregate_is_refused() {
        for text in [
            "db.c.insertOne({a: 1})",
            "db.c.deleteMany({})",
            "db.c.findOne({})",
            "db.c.countDocuments({})",
            "db.c.distinct('a')",
            "db.runCommand({find: 'c'})",
            "show collections",
            "db.c.aggregate([{$out: 'd'}])",
            "db.c.aggregate([{$merge: {into: 'd'}}])",
            "db.c.find().explain()",
            "db.c.find().sort({a: 1}).sort({b: 1})",
            "db.c.aggregate([]).sort({a: 1})",
            "db.c.aggregate({$match: {}})",
            "db.c.aggregate()",
            "db.c.find(); db.c.find()",
            "db.c.find(",
            "db.c.find().sort({a: 1}).limit(",
            "SELECT * FROM c",
        ] {
            assert_eq!(order_by(text), None, "{text}");
            assert_eq!(with_order_by(text, &[key("a", true)]), None, "{text}");
        }
    }

    #[test]
    fn a_sort_this_could_not_reprint_is_left_alone() {
        for text in [
            "db.c.find().sort({score: {$meta: 'textScore'}})",
            "db.c.find().sort({a: 'asc'})",
            "db.c.find().sort({a: 2})",
            "db.c.find().sort('a')",
        ] {
            assert_eq!(order_by(text), None, "{text}");
        }
        // Long and double ones are still one.
        assert_eq!(
            order_by("db.c.find().sort({a: NumberLong(-1), b: 1.0})"),
            Some(vec![key("a", false), key("b", true)])
        );
    }

    #[test]
    fn a_key_that_names_no_field_is_refused() {
        let text = "db.c.find()";
        // The positional key a duplicate column falls back to.
        assert_eq!(with_order_by(text, &[SortKey::new("2", true)]), None);
        assert_eq!(with_order_by(text, &[key("$where", true)]), None);
        assert_eq!(with_order_by(text, &[key("", true)]), None);
        // A path the user's own sort already named is theirs to keep.
        assert_eq!(
            sorted("db.c.find().sort({'a.b': 1})", &[key("a.b", false)]),
            r#"db.c.find().sort({"a.b": -1})"#
        );
    }

    #[test]
    fn every_sorted_statement_reads_back_and_classifies_as_a_read() {
        for text in [
            "db.c.find()",
            "db.c.find({a: {$gt: 1}}).sort({a: 1}).skip(5).limit(5)",
            "db.c.aggregate([{$match: {}}])",
        ] {
            let keys = [key("x", true), key("quote\"d", false)];
            let sorted = sorted(text, &keys);
            assert_eq!(order_by(&sorted), Some(keys.to_vec()), "{sorted}");
            assert_eq!(classify(&sorted), Verdict::READ, "{sorted}");
        }
    }
}

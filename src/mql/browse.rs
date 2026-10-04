//! Browsing a collection: the statements an object tab generates and the sort
//! a header click splices into a buffer. The Mongo arms of
//! `explorer::preview_sql`, `sql::is_generated_select`, `sql::order_by` and
//! `sql::with_order_by` end here.
//!
//! A sort is spliced by editing the statement's text at the byte spans the
//! parse found, never by printing the parse back out, so every comment and
//! every spelling the user chose outside the sort survives.

use std::ops::Range;

use super::{
    Call, CursorMethod, Method, Statement, Target, Value, classify, integer, parse, quoted,
    writable_field,
};
use crate::filter::{Conjunction, Operator};
use crate::sql::{SortKey, Verdict};

/// How a header click names a field in a sort: `filter::sort_expression`'s
/// Mongo arm. `None` for a field a sort document cannot name.
pub(crate) fn sort_field(field: &str) -> Option<String> {
    writable_field(field).then(|| quoted(field))
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
    /// A pipeline's last stage before any trailing `$skip` and `$limit`,
    /// when it is `{$sort: …}`.
    Stage { stage: Range<usize>, keys: Value },
    /// Where a sort goes on a statement without one: after `find(…)`, or as a
    /// pipeline stage, with the separators that stand before and after it.
    Absent {
        at: usize,
        stage: bool,
        around: (&'static str, &'static str),
    },
}

/// A `$skip` or `$limit` stage.
fn pages(stage: &Value) -> bool {
    matches!(stage, Value::Document(fields)
        if fields.len() == 1 && matches!(fields[0].0.as_str(), "$skip" | "$limit"))
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
    let sorted_by_options = matches!(
        call.args.get(2).map(|options| &options.value),
        Some(Value::Document(fields)) if fields.iter().any(|(name, _)| name == "sort")
    );
    match call.method {
        // A sort in the options is not where a click would edit one, and the
        // server takes only one of the two.
        Method::Find if sorted_by_options => None,
        Method::Find => match sorts.as_slice() {
            [] => Some(Site::Absent {
                at: call.span.end,
                stage: false,
                around: ("", ""),
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
            if stages.len() != pipeline.items.len() {
                return None;
            }
            // A sort after `$skip` or `$limit` would order only the page they
            // kept, so it stands before them, as `ORDER BY` stands before
            // `LIMIT`.
            let paging = stages.len() - stages.iter().rev().take_while(|s| pages(s)).count();
            let before = paging.checked_sub(1);
            match (
                before.map(|at| &stages[at]),
                before,
                pipeline.items.get(paging),
            ) {
                (Some(Value::Document(fields)), Some(at), _)
                    if fields.len() == 1 && fields[0].0 == "$sort" =>
                {
                    Some(Site::Stage {
                        stage: pipeline.items[at].clone(),
                        keys: fields[0].1.clone(),
                    })
                }
                (_, Some(at), _) => Some(Site::Absent {
                    at: pipeline.items[at].end,
                    stage: true,
                    around: (", ", ""),
                }),
                (_, None, Some(first)) => Some(Site::Absent {
                    at: first.start,
                    stage: true,
                    around: ("", ", "),
                }),
                (_, None, None) => Some(Site::Absent {
                    at: pipeline.span.start + 1,
                    stage: true,
                    around: ("", ""),
                }),
            }
        }
        _ => None,
    }
}

/// `sql::order_by` for a mongosh statement: the keys of a `find`'s `.sort(…)`
/// or an `aggregate`'s last `$sort` stage before any trailing `$skip` and
/// `$limit`. `Some(empty)` for one that
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
/// goes directly after `find(…)`. An `aggregate`'s `$sort` stage standing
/// last before any trailing `$skip` and `$limit` is replaced, or one goes
/// there. Nothing else in the text moves.
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
        (
            Site::Absent {
                at,
                around: (lead, trail),
                ..
            },
            false,
        ) => {
            sorted.insert_str(at, &format!("{lead}{stage}{trail}"));
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

/// A pipeline stage's span, widened to take one comma beside it: the one
/// before it, else the one after it with the spaces that follow it (and its
/// line, when nothing else is left on it). `None` when a comment stands between the stage and
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
        Some(',') => {
            let rest = &after[1..];
            let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            Some(own_line(text, stage.start..next + 1 + spaces))
        }
        Some(']') if before.ends_with('[') => Some(stage),
        _ => None,
    }
}

/// A collection's page, as an object tab runs it: `explorer::preview_sql`'s
/// Mongo arm. `filter` is a filter document's text, empty for every document.
/// A zero offset is left out, as the SQL previews leave out `OFFSET 0`.
pub(crate) fn find_preview(collection: &str, filter: &str, limit: usize, offset: usize) -> String {
    let mut statement = format!("{}.find({})", handle(collection), or_all(filter));
    if offset > 0 {
        statement.push_str(&format!(".skip({offset})"));
    }
    statement.push_str(&format!(".limit({limit})"));
    statement
}

/// The size of what [`find_preview`] pages through: `explorer::count_sql`'s
/// Mongo arm.
pub(crate) fn count_documents(collection: &str, filter: &str) -> String {
    format!("{}.countDocuments({})", handle(collection), or_all(filter))
}

/// By `getCollection` rather than as a property, so any name -- one with a
/// space, a dash, or a method's name -- is the string it is.
pub(crate) fn handle(collection: &str) -> String {
    format!("db.getCollection({})", quoted(collection))
}

fn or_all(filter: &str) -> &str {
    match filter.trim() {
        "" => "{}",
        filter => filter,
    }
}

/// `sql::is_generated_select` for a mongosh statement: whether `text` is a
/// statement an object tab could have generated, and nothing else.
///
/// Read whole and then written again from its own parts -- the collection, the
/// filter's text, the sort, the page -- with the builders above, and admitted
/// only when that is `text` exactly. So a second argument (a projection), a
/// cursor method the preview does not write, another database, or a second
/// statement are all refused without a list of them to keep up. The filter is
/// the one part taken as written, which is why it is checked here too:
/// a document, running no JavaScript on the server ([`runs_code`]). A limit of
/// zero is refused, since to the server it is no limit at all.
pub(crate) fn is_generated_read(text: &str) -> bool {
    let Ok(statements) = parse(text) else {
        return false;
    };
    let [
        Statement {
            target:
                Target::Collection {
                    database: None,
                    collection,
                    call,
                    cursor,
                },
            ..
        },
    ] = statements.as_slice()
    else {
        return false;
    };
    let [filter] = call.args.as_slice() else {
        return false;
    };
    if !matches!(filter.value, Value::Document(_)) || runs_code(&filter.value) {
        return false;
    }
    let filter = &text[filter.span.clone()];
    let rebuilt = match (call.method, cursor.as_slice()) {
        (Method::CountDocuments, []) => Some(count_documents(collection, filter)),
        (Method::Find, chain) => paged_find(collection, filter, chain),
        _ => None,
    };
    rebuilt.as_deref() == Some(text) && classify(text) == Verdict::READ
}

/// A `find`'s chain written again the way [`find_preview`] and
/// [`with_order_by`] write it, or `None` for a chain neither writes.
fn paged_find(collection: &str, filter: &str, chain: &[Call<CursorMethod>]) -> Option<String> {
    let (sort, page) = match chain {
        [sort, page @ ..] if sort.method == CursorMethod::Sort => (Some(sort), page),
        page => (None, page),
    };
    let count = |call: &Call<CursorMethod>| match call.args.as_slice() {
        [argument] => integer(&argument.value).and_then(|n| usize::try_from(n).ok()),
        _ => None,
    };
    let (offset, limit) = match page {
        [skip, limit]
            if skip.method == CursorMethod::Skip && limit.method == CursorMethod::Limit =>
        {
            (count(skip)?, count(limit)?)
        }
        [limit] if limit.method == CursorMethod::Limit => (0, count(limit)?),
        _ => return None,
    };
    if limit == 0 {
        return None;
    }
    let preview = find_preview(collection, filter, limit, offset);
    match sort {
        None => Some(preview),
        Some(sort) => {
            let [argument] = sort.args.as_slice() else {
                return None;
            };
            with_order_by(&preview, &sort_keys(&argument.value)?)
        }
    }
}

/// Whether a filter runs JavaScript on the server -- `$where`, `$function`,
/// `$accumulator` -- at any depth, or holds a string where `$and`, `$or` or
/// `$nor` takes a document: a raw bar that was not one document
/// ([`raw_filter`]).
fn runs_code(value: &Value) -> bool {
    match value {
        Value::Document(fields) => fields.iter().any(|(key, value)| {
            let documents = matches!(
                value,
                Value::Array(items) if items.iter().all(|item| matches!(item, Value::Document(_)))
            );
            matches!(key.as_str(), "$where" | "$function" | "$accumulator")
                || (matches!(key.as_str(), "$and" | "$or" | "$nor") && !documents)
                || runs_code(value)
        }),
        Value::Array(items) => items.iter().any(runs_code),
        _ => false,
    }
}

/// A raw filter bar's text as it goes into the filter: as typed when it is one
/// object literal and nothing else, else as a string, which the gate refuses
/// where a document belongs. So text that would close the `find(` early, add a
/// second argument or chain a method never reaches the statement as code, and
/// the refusal is still the gate's, said where the user can see it.
pub(crate) fn raw_filter(text: &str) -> String {
    let text = text.trim();
    let open = "db.c.find(\n";
    let probe = format!("{open}{text}\n)");
    let lone = match parse(&probe).as_deref() {
        Ok(
            [
                Statement {
                    target: Target::Collection { call, cursor, .. },
                    ..
                },
            ],
        ) => {
            cursor.is_empty()
                && matches!(
                    call.args.as_slice(),
                    [argument] if argument.span == (open.len()..open.len() + text.len())
                        && matches!(argument.value, Value::Document(_))
                )
        }
        _ => false,
    };
    match lone {
        true => text.to_owned(),
        false => quoted(text),
    }
}

/// Two bars' filters joined the way the bar below asks: `filter::derived_filter`'s
/// Mongo arm, which folds the stack left to right as it does a `WHERE`.
pub(crate) fn joined(conjunction: Conjunction, left: &str, right: &str) -> String {
    let operator = match conjunction {
        Conjunction::And => "$and",
        Conjunction::Or => "$or",
    };
    format!("{{\"{operator}\": [{left}, {right}]}}")
}

/// One field against one value under one operator, as a filter document:
/// `filter::filter_predicate`'s Mongo arm. `None` where the value does not add
/// up to a filter, or the field cannot be named in one.
///
/// `data_type` is the field's sampled type (`int | null`), which decides how
/// the typed text is written ([`literal`]). The substring operators match an
/// escaped `$regex`, so a `.` in the value is a dot.
pub(crate) fn filter_predicate(
    field: &str,
    data_type: Option<&str>,
    operator: Operator,
    value: &str,
) -> Option<String> {
    if !writable_field(field) {
        return None;
    }
    let literal = |value: &str| literal(data_type, value);
    let compared = |operator: &str| format!("{{\"{operator}\": {}}}", literal(value));
    let pattern = |pattern: String| format!("{{\"$regex\": {}}}", quoted(&pattern));
    let condition = match operator {
        Operator::Equals => compared("$eq"),
        Operator::NotEquals => compared("$ne"),
        Operator::Greater => compared("$gt"),
        Operator::GreaterOrEqual => compared("$gte"),
        Operator::Less => compared("$lt"),
        Operator::LessOrEqual => compared("$lte"),
        // Null or missing, as `{f: null}` matches on the server.
        Operator::IsNull => "null".to_owned(),
        Operator::IsNotNull => r#"{"$ne": null}"#.to_owned(),
        Operator::IsEmpty => r#"{"$eq": ""}"#.to_owned(),
        Operator::IsNotEmpty => r#"{"$ne": ""}"#.to_owned(),
        Operator::Contains => pattern(regex_escaped(value)),
        Operator::NotContains => format!("{{\"$not\": {}}}", pattern(regex_escaped(value))),
        Operator::StartsWith => pattern(format!("^{}", regex_escaped(value))),
        Operator::EndsWith => pattern(format!("{}$", regex_escaped(value))),
        Operator::Regex => pattern(value.to_owned()),
        Operator::InList | Operator::NotInList => {
            let items: Vec<_> = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(literal)
                .collect();
            if items.is_empty() {
                return None;
            }
            let operator = match operator {
                Operator::NotInList => "$nin",
                _ => "$in",
            };
            format!("{{\"{operator}\": [{}]}}", items.join(", "))
        }
        Operator::Between => {
            let (low, high) = value.split_once("..")?;
            let (low, high) = (low.trim(), high.trim());
            if low.is_empty() || high.is_empty() {
                return None;
            }
            format!(
                "{{\"$gte\": {}, \"$lte\": {}}}",
                literal(low),
                literal(high)
            )
        }
    };
    Some(format!("{{{}: {condition}}}", quoted(field)))
}

/// `text` with every regex metacharacter escaped, so it matches itself.
fn regex_escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\^$.|?*+()[]{}".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The typed text as a literal of the field's type, where the field has one
/// type besides null: a number for a numeric field (`NumberDecimal` for a
/// decimal one, `NumberLong` past 32 bits), an `ObjectId` from its hex (its cell
/// text) or `ObjectId('…')`, an `ISODate`, a boolean. A string otherwise --
/// for a string field, a field of mixed or unknown type, or text that is not
/// one of those -- since a string matching nothing is the honest answer to a
/// value the field cannot hold.
fn literal(data_type: Option<&str>, value: &str) -> String {
    let types: Vec<&str> = data_type
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|kind| !matches!(*kind, "" | "null"))
        .collect();
    let text = value.trim();
    let numeric = !types.is_empty()
        && types
            .iter()
            .all(|kind| matches!(*kind, "int" | "long" | "double" | "decimal"));
    let coerced = match types.as_slice() {
        _ if numeric => number_literal(text, types.contains(&"decimal")),
        ["objectId"] => object_id_literal(text),
        ["date"] => super::iso_millis(text).map(|_| format!("ISODate({})", quoted(text))),
        ["bool"] => matches!(text, "true" | "false").then(|| text.to_owned()),
        _ => None,
    };
    coerced.unwrap_or_else(|| quoted(value))
}

fn number_literal(text: &str, decimal: bool) -> Option<String> {
    if decimal {
        return super::decimal(text.to_owned())
            .ok()
            .map(|_| format!("NumberDecimal({})", quoted(text)));
    }
    if let Ok(n) = text.parse::<i64>() {
        return Some(match i32::try_from(n) {
            Ok(n) => n.to_string(),
            Err(_) => format!("NumberLong({})", quoted(&n.to_string())),
        });
    }
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| format!("{n:?}"))
}

fn object_id_literal(text: &str) -> Option<String> {
    let hex_text = text
        .strip_prefix("ObjectId(")
        .and_then(|inner| inner.strip_suffix(')'))
        .map_or(text, |inner| inner.trim().trim_matches(['\'', '"']));
    match super::object_id(hex_text) {
        Ok(Value::ObjectId(Some(bytes))) => {
            Some(format!("ObjectId({})", quoted(&hex::encode(bytes))))
        }
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
    fn a_pipeline_sorts_before_its_trailing_skip_and_limit() {
        let text = "db.c.aggregate([{$match: {}}, {$skip: 20}, {$limit: 10}])";
        assert_eq!(order_by(text), Some(vec![]));
        assert_eq!(
            sorted(text, &[key("a", true)]),
            r#"db.c.aggregate([{$match: {}}, {"$sort": {"a": 1}}, {$skip: 20}, {$limit: 10}])"#
        );
        assert_eq!(
            sorted("db.c.aggregate([{$limit: 10}])", &[key("a", true)]),
            r#"db.c.aggregate([{"$sort": {"a": 1}}, {$limit: 10}])"#
        );

        let text = "db.c.aggregate([{$sort: {a: 1}}, {$limit: 10}])";
        assert_eq!(order_by(text), Some(vec![key("a", true)]));
        assert_eq!(
            sorted(text, &[key("b", false)]),
            r#"db.c.aggregate([{"$sort": {"b": -1}}, {$limit: 10}])"#
        );
        assert_eq!(sorted(text, &[]), "db.c.aggregate([{$limit: 10}])");
        assert_eq!(
            sorted(
                "db.c.aggregate([{$match: {}}, {$sort: {a: 1}}, {$skip: 5}])",
                &[]
            ),
            "db.c.aggregate([{$match: {}}, {$skip: 5}])"
        );
        // A `$limit` with a stage after it is not the page's.
        assert_eq!(
            sorted(
                "db.c.aggregate([{$limit: 10}, {$match: {}}])",
                &[key("a", true)]
            ),
            r#"db.c.aggregate([{$limit: 10}, {$match: {}}, {"$sort": {"a": 1}}])"#
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
            "db.c.aggregate([])"
        );
        assert_eq!(
            sorted("db.c.aggregate([{$sort: {a: 1}}])", &[]),
            "db.c.aggregate([])"
        );
        // A `$sort` earlier in the pipeline is the user's, and a new one goes
        // after everything that is not paging.
        assert_eq!(
            sorted(
                "db.c.aggregate([{$sort: {a: 1}}, {$match: {}}])",
                &[key("b", true)]
            ),
            r#"db.c.aggregate([{$sort: {a: 1}}, {$match: {}}, {"$sort": {"b": 1}}])"#
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
            "db.c.find({}, {}, {sort: {b: 1}})",
            "db.c.find({}, {}, {sort: {b: 1}}).sort({a: 1})",
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

    #[test]
    fn a_preview_pages_with_skip_and_limit_and_counts_under_the_same_filter() {
        assert_eq!(
            find_preview("events", "", 100, 0),
            r#"db.getCollection("events").find({}).limit(100)"#
        );
        assert_eq!(
            find_preview("my \"odd\".coll", " {a: 1} ", 100, 200),
            r#"db.getCollection("my \"odd\".coll").find({a: 1}).skip(200).limit(100)"#
        );
        assert_eq!(
            count_documents("events", ""),
            r#"db.getCollection("events").countDocuments({})"#
        );
        assert_eq!(
            count_documents("events", "{a: 1}"),
            r#"db.getCollection("events").countDocuments({a: 1})"#
        );
    }

    #[test]
    fn every_generated_statement_passes_the_gate_and_reads() {
        for statement in [
            find_preview("events", "", 100, 0),
            find_preview("system.views", "{a: {$gt: 1}}", 1, 5),
            find_preview("db", "{$or: [{a: 1}, {b: /x/i}]}", 500, 0),
            sorted(
                &find_preview("c", "{a: 1}", 100, 300),
                &[key("a", false), key("b c", true)],
            ),
            count_documents("events", ""),
            count_documents("events", "{a: ObjectId('65a1b2c3d4e5f60718293a4b')}"),
        ] {
            assert!(is_generated_read(&statement), "{statement} was refused");
            assert_eq!(classify(&statement), Verdict::READ, "{statement}");
        }
    }

    #[test]
    fn the_gate_admits_nothing_the_builders_would_not_write() {
        for statement in [
            // Server-side JavaScript, at any depth.
            r#"db.getCollection("c").find({$where: "sleep(1000)"}).limit(100)"#,
            r#"db.getCollection("c").find({a: 1, $or: [{$where: "1"}]}).limit(100)"#,
            r#"db.getCollection("c").find({$expr: {$function: {body: "x", args: [], lang: "js"}}}).limit(100)"#,
            r#"db.getCollection("c").countDocuments({$expr: {$accumulator: {}}})"#,
            r#"db.getCollection("c").find({"\u0024where": "1"}).limit(100)"#,
            // A raw bar that was not one document, quoted into a string.
            r#"db.getCollection("c").find({"$and": ["{}).limit(1", {"a": 1}]}).limit(100)"#,
            r#"db.getCollection("c").find("{}").limit(100)"#,
            // Unbounded: a limit of zero is none to the server, and no limit
            // is none at all.
            r#"db.getCollection("c").find({}).limit(0)"#,
            r#"db.getCollection("c").find({})"#,
            // Anything beyond the shape: a projection, another cursor method,
            // another database, a skip of zero, an order the builders do not
            // write, another spelling, a second statement.
            r#"db.getCollection("c").find({}, {secret: 0}).limit(100)"#,
            r#"db.getCollection("c").find({}).limit(100).maxTimeMS(1)"#,
            r#"db.getCollection("c").find({}).limit(100).explain()"#,
            r#"db.getSiblingDB("admin").getCollection("c").find({}).limit(100)"#,
            r#"db.getCollection("c").find({}).skip(0).limit(100)"#,
            r#"db.getCollection("c").find({}).limit(100).skip(5)"#,
            r#"db.getCollection("c").find({}).limit(100).sort({"a": 1})"#,
            r#"db.getCollection("c").find({}).sort({a: 1}).limit(100)"#,
            r#"db.getCollection("c").find({}).sort({"a": 1}).sort({"b": 1}).limit(100)"#,
            r#"db.c.find({}).limit(100)"#,
            r#"db.getCollection('c').find({}).limit(100)"#,
            r#"db.getCollection("c").find({}).limit(100);"#,
            r#"db.getCollection("c").find({}).limit(100) // note"#,
            r#"db.getCollection("c").find({}).limit(100); db.getCollection("c").drop()"#,
            r#"db.getCollection("c").countDocuments({}, {limit: 1})"#,
            r#"db.getCollection("c").countDocuments({}).limit(1)"#,
            r#"db.getCollection("c").deleteMany({})"#,
            r#"db.getCollection("c").aggregate([{$out: "d"}])"#,
            r#"db.getCollection("c").findOne({})"#,
            r#"db.runCommand({find: "c"})"#,
            "SELECT * FROM c LIMIT 100",
            "",
        ] {
            assert!(!is_generated_read(statement), "{statement} passed the gate");
        }
    }

    fn filtered(filter: &str) -> String {
        find_preview("c", filter, 100, 0)
    }

    #[test]
    fn a_raw_bar_is_one_document_and_nothing_else() {
        for text in [
            "{}",
            " { a: 1 } ",
            "{a: {$in: [1, 2]}, 'b c': /x/i}",
            "{a: 1 /* note */}",
            "{_id: ObjectId('65a1b2c3d4e5f60718293a4b')}",
        ] {
            assert_eq!(raw_filter(text), text.trim(), "{text}");
            assert!(is_generated_read(&filtered(&raw_filter(text))), "{text}");
        }
        // Each of these would end the filter early or add to the statement;
        // as a string, each is refused where a document belongs.
        for text in [
            "{}).sort({\"a\": 1}",
            "{}).limit(1",
            "{}, {secret: 0}",
            "{}) ; db.c.drop(",
            "{}), db.c.drop({}",
            "{a: 1} // the rest",
            "{a: 1}\n.limit(1)",
            "a = 1",
            "[{a: 1}]",
            "'{}'",
            "{a: 1}}",
            "{a: 1",
            "db.c.drop()",
        ] {
            let quoted = raw_filter(text);
            assert_eq!(quoted, super::quoted(text.trim()), "{text}");
            assert!(!is_generated_read(&filtered(&quoted)), "{text} passed");
            let joined = joined(Conjunction::And, &quoted, r#"{"a": 1}"#);
            assert!(
                !is_generated_read(&filtered(&joined)),
                "{text} passed joined"
            );
        }
        // One document that runs JavaScript is still one document, and the
        // gate refuses it.
        for text in [
            "{$where: 'sleep(1000)'}",
            "{$expr: {$function: {body: 'x', args: [], lang: 'js'}}}",
        ] {
            assert_eq!(raw_filter(text), text);
            assert!(!is_generated_read(&filtered(text)), "{text} passed");
        }
    }

    #[test]
    fn bars_join_as_their_stack_reads() {
        let a = r#"{"a": {"$eq": 1}}"#;
        let b = r#"{"b": null}"#;
        let either = joined(Conjunction::Or, a, b);
        assert_eq!(either, r#"{"$or": [{"a": {"$eq": 1}}, {"b": null}]}"#);
        let both = joined(Conjunction::And, &either, "{c: 1}");
        assert_eq!(
            both,
            r#"{"$and": [{"$or": [{"a": {"$eq": 1}}, {"b": null}]}, {c: 1}]}"#
        );
        assert!(is_generated_read(&filtered(&both)), "{both}");
    }

    fn predicate(data_type: &str, operator: Operator, value: &str) -> String {
        filter_predicate("f", Some(data_type), operator, value)
            .unwrap_or_else(|| panic!("{operator:?} {value} was not applied"))
    }

    #[test]
    fn every_operator_is_spelled_as_mql_and_passes_the_gate() {
        let string = |operator, value| predicate("string", operator, value);
        assert_eq!(string(Operator::Equals, "ok"), r#"{"f": {"$eq": "ok"}}"#);
        assert_eq!(string(Operator::NotEquals, "ok"), r#"{"f": {"$ne": "ok"}}"#);
        assert_eq!(string(Operator::Greater, "ok"), r#"{"f": {"$gt": "ok"}}"#);
        assert_eq!(
            string(Operator::GreaterOrEqual, "ok"),
            r#"{"f": {"$gte": "ok"}}"#
        );
        assert_eq!(string(Operator::Less, "ok"), r#"{"f": {"$lt": "ok"}}"#);
        assert_eq!(
            string(Operator::LessOrEqual, "ok"),
            r#"{"f": {"$lte": "ok"}}"#
        );
        assert_eq!(string(Operator::IsNull, ""), r#"{"f": null}"#);
        assert_eq!(string(Operator::IsNotNull, ""), r#"{"f": {"$ne": null}}"#);
        assert_eq!(string(Operator::IsEmpty, ""), r#"{"f": {"$eq": ""}}"#);
        assert_eq!(string(Operator::IsNotEmpty, ""), r#"{"f": {"$ne": ""}}"#);
        assert_eq!(
            string(Operator::Contains, "a.b"),
            r#"{"f": {"$regex": "a\\.b"}}"#
        );
        assert_eq!(
            string(Operator::NotContains, "a"),
            r#"{"f": {"$not": {"$regex": "a"}}}"#
        );
        assert_eq!(
            string(Operator::StartsWith, "a"),
            r#"{"f": {"$regex": "^a"}}"#
        );
        assert_eq!(
            string(Operator::EndsWith, "a"),
            r#"{"f": {"$regex": "a$"}}"#
        );
        assert_eq!(
            string(Operator::Regex, "^a.b"),
            r#"{"f": {"$regex": "^a.b"}}"#
        );
        assert_eq!(
            string(Operator::InList, " a , b ,, c "),
            r#"{"f": {"$in": ["a", "b", "c"]}}"#
        );
        assert_eq!(
            string(Operator::NotInList, "a"),
            r#"{"f": {"$nin": ["a"]}}"#
        );
        assert_eq!(
            string(Operator::Between, "a .. b"),
            r#"{"f": {"$gte": "a", "$lte": "b"}}"#
        );
        assert_eq!(filter_predicate("f", None, Operator::InList, " , "), None);
        assert_eq!(filter_predicate("f", None, Operator::Between, "1.."), None);
        for operator in Operator::ALL {
            let value = match operator {
                Operator::Between => "1..9",
                Operator::InList | Operator::NotInList => "1, 2",
                _ => "1",
            };
            for data_type in ["string", "int | null", "objectId", "date", "mixed"] {
                let filter = predicate(data_type, operator, value);
                assert!(is_generated_read(&filtered(&filter)), "{filter}");
            }
        }
    }

    #[test]
    fn a_substring_matches_itself_and_not_a_pattern() {
        assert_eq!(
            regex_escaped(r"1+1=2? (a|b) [x]{2} ^$ \d."),
            r"1\+1=2\? \(a\|b\) \[x\]\{2\} \^\$ \\d\."
        );
        // The quoting is JSON's, so the escape's backslash survives as one.
        assert_eq!(
            predicate("string", Operator::Contains, r#"say "hi" \o/"#),
            r#"{"f": {"$regex": "say \"hi\" \\\\o/"}}"#
        );
    }

    #[test]
    fn a_value_is_written_as_its_fields_type() {
        let equals = |data_type, value| predicate(data_type, Operator::Equals, value);
        assert_eq!(equals("int", "42"), r#"{"f": {"$eq": 42}}"#);
        assert_eq!(equals("int | null", " -7 "), r#"{"f": {"$eq": -7}}"#);
        assert_eq!(
            equals("long", "4294967296"),
            r#"{"f": {"$eq": NumberLong("4294967296")}}"#
        );
        assert_eq!(equals("double", "2.5"), r#"{"f": {"$eq": 2.5}}"#);
        assert_eq!(equals("int | double", "1e21"), r#"{"f": {"$eq": 1e21}}"#);
        assert_eq!(
            equals("decimal", "125000.50"),
            r#"{"f": {"$eq": NumberDecimal("125000.50")}}"#
        );
        assert_eq!(
            equals("objectId", "65A1B2C3D4E5F60718293A4B"),
            r#"{"f": {"$eq": ObjectId("65a1b2c3d4e5f60718293a4b")}}"#
        );
        assert_eq!(
            equals("objectId", "ObjectId('65a1b2c3d4e5f60718293a4b')"),
            r#"{"f": {"$eq": ObjectId("65a1b2c3d4e5f60718293a4b")}}"#
        );
        assert_eq!(
            equals("date | null", "2024-01-15T09:30:00.123Z"),
            r#"{"f": {"$eq": ISODate("2024-01-15T09:30:00.123Z")}}"#
        );
        assert_eq!(equals("bool", "true"), r#"{"f": {"$eq": true}}"#);
        // Text the type cannot hold stays text, and so does every value of a
        // field whose type is mixed or unknown.
        assert_eq!(equals("int", "abc"), r#"{"f": {"$eq": "abc"}}"#);
        assert_eq!(equals("int", "NaN"), r#"{"f": {"$eq": "NaN"}}"#);
        assert_eq!(equals("objectId", "65a1"), r#"{"f": {"$eq": "65a1"}}"#);
        assert_eq!(equals("bool", "yes"), r#"{"f": {"$eq": "yes"}}"#);
        assert_eq!(equals("string", "42"), r#"{"f": {"$eq": "42"}}"#);
        assert_eq!(equals("string | int", "42"), r#"{"f": {"$eq": "42"}}"#);
        assert_eq!(
            filter_predicate("f", None, Operator::Equals, "42").as_deref(),
            Some(r#"{"f": {"$eq": "42"}}"#)
        );
        // Each one reads back as the type it was written as.
        for (data_type, value, read) in [
            ("int", "42", Value::Int32(42)),
            ("long", "4294967296", Value::Int64(4_294_967_296)),
            ("double", "2.5", Value::Double(2.5)),
            ("decimal", "1.10", Value::Decimal128("1.10".into())),
            ("date", "1970-01-02", Value::Date(86_400_000)),
            ("bool", "false", Value::Bool(false)),
        ] {
            let statement = filtered(&equals(data_type, value));
            let statements = parse(&statement).unwrap();
            let Target::Collection { call, .. } = &statements[0].target else {
                panic!("{statement}");
            };
            let Value::Document(fields) = &call.args[0].value else {
                panic!("{statement}");
            };
            assert_eq!(
                fields[0].1,
                Value::Document(vec![("$eq".into(), read)]),
                "{statement}"
            );
        }
    }

    #[test]
    fn a_field_a_filter_cannot_name_narrows_nothing() {
        // A dot is a path into a subdocument and a `$` an operator.
        for field in ["a.b", "$where", ""] {
            assert_eq!(filter_predicate(field, None, Operator::Equals, "1"), None);
        }
        assert_eq!(
            filter_predicate("say \"hi\"", None, Operator::IsNull, "").as_deref(),
            Some(r#"{"say \"hi\"": null}"#)
        );
    }
}

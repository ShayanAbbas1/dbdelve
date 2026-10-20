//! The keys grid: the `SCAN` its preview runs, the `DBSIZE` its count runs,
//! the `SCAN` options its filter bar writes, the read a key opens with, and
//! the gate the preview and the count pass.
//!
//! A tab's filter is the run of `SCAN` options its bars add up to (`MATCH
//! user:* TYPE hash`), so the preview is that run between the cursor and the
//! page size. `SCAN` has no `LIMIT`: the tab's row limit is the fetch's, which
//! stops following the cursor once the rows reach it.

use super::{parse, quoted};
use crate::filter::{Conjunction, Operator};

/// The one relation a Redis database lists: its keyspace.
pub(crate) const KEYS: &str = "keys";

/// The page size every preview hints at. Redis reads `COUNT` as how much of
/// the keyspace to walk per round trip, not how many keys to return.
const COUNT: &str = "1000";

/// How many of a collection's entries the keys grid reads to preview it, and
/// an edit of its JSON reads again to check it.
pub(crate) const PREVIEWED: usize = 100;

pub(crate) fn preview(filter: &str) -> String {
    match filter.trim() {
        "" => format!("SCAN 0 MATCH * COUNT {COUNT}"),
        options => format!("SCAN 0 {options} COUNT {COUNT}"),
    }
}

/// The status bar's Count. `DBSIZE` counts the whole keyspace and nothing
/// counts a pattern's keys but walking them, which is what the preview already
/// did, so under a filter there is no count to run: the empty text, which
/// Count reads as that and the gate refuses.
pub(crate) fn count(filter: &str) -> String {
    match filter.trim() {
        "" => "DBSIZE".into(),
        _ => String::new(),
    }
}

/// The raw bar: a `MATCH` glob, as typed.
pub(crate) fn raw_filter(pattern: &str) -> String {
    format!("MATCH {}", quoted(pattern))
}

/// Whether the bar offers `column` at all: a key's name and its type are what
/// `SCAN` can narrow by.
pub(crate) fn filters_on(column: &str) -> bool {
    matches!(column, "key" | "type")
}

/// Whether the bar offers `operator` on `column`, or on the bar's column
/// before one is picked: a key's name compares as a glob, a type only exactly.
pub(crate) fn offers(column: Option<&str>, operator: Operator) -> bool {
    matches!(
        (column, operator),
        (
            None | Some("key"),
            Operator::Equals | Operator::Contains | Operator::StartsWith | Operator::EndsWith,
        ) | (Some("type"), Operator::Equals)
    )
}

/// One bar as a `SCAN` option: a `MATCH` glob holding the value literally, or
/// a `TYPE`. `None` for what `SCAN` cannot narrow by.
pub(crate) fn filter_predicate(column: &str, operator: Operator, value: &str) -> Option<String> {
    let literal = literal(value);
    let glob = |pattern: String| Some(raw_filter(&pattern));
    match (column, operator) {
        ("key", Operator::Equals) => glob(literal),
        ("key", Operator::Contains) => glob(format!("*{literal}*")),
        ("key", Operator::StartsWith) => glob(format!("{literal}*")),
        ("key", Operator::EndsWith) => glob(format!("*{literal}")),
        ("type", Operator::Equals) => Some(format!("TYPE {}", quoted(value))),
        _ => None,
    }
}

/// `value` as glob text matching exactly itself: every character Redis's
/// `stringmatch` reads as a wildcard, a class or an escape, escaped.
fn literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Two bars' options as one run. `SCAN` takes one pattern and one type, so a
/// second of either, or any `OR`, is a run the gate refuses, which says so
/// where a pattern quietly dropped would not.
pub(crate) fn joined(conjunction: Conjunction, left: &str, predicate: &str) -> String {
    match conjunction {
        Conjunction::And => format!("{left} {predicate}"),
        Conjunction::Or => format!("{left} OR {predicate}"),
    }
}

/// Whether `text` is a preview or a count this module writes: written again
/// from the words it splits into and compared with the text, so a pattern
/// holding a newline or a quote cannot have added a command or an option.
pub(crate) fn is_generated_read(text: &str) -> bool {
    if text == count("") {
        return true;
    }
    let Ok(commands) = parse(text) else {
        return false;
    };
    let [command] = commands.as_slice() else {
        return false;
    };
    let Ok(words) = command
        .words
        .iter()
        .map(|word| std::str::from_utf8(word))
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    let ["SCAN", "0", options @ .., "COUNT", COUNT] = words.as_slice() else {
        return false;
    };
    let options = match options {
        ["MATCH", pattern] => raw_filter(pattern),
        ["TYPE", kind] => format!("TYPE {}", quoted(kind)),
        ["MATCH", pattern, "TYPE", kind] => {
            format!("{} TYPE {}", raw_filter(pattern), quoted(kind))
        }
        ["TYPE", kind, "MATCH", pattern] => {
            format!("TYPE {} {}", quoted(kind), raw_filter(pattern))
        }
        _ => return false,
    };
    preview(&options) == text
}

/// The command a key of `kind` opens with: its first `rows` entries, in the
/// shape each type reads back as a grid, and never in one read that holds the
/// server for as long as the key is big. A hash, set and sorted set are
/// scanned, so the fetch's limit stops the cursor; a list and a stream say
/// how many to read. `None` for a type there is no read for.
///
/// `key` is as the keys grid shows it, which for a name that is not UTF-8 is
/// already redis-cli's quoted spelling of its bytes, and is sent as written.
/// A UTF-8 name that happens to spell such a word the same way reads as those
/// bytes; the grid cannot tell the two apart.
pub(crate) fn read(key: &str, kind: &str, rows: usize) -> Option<String> {
    let key = match super::split(key).as_deref() {
        Ok([bytes]) if std::str::from_utf8(bytes).is_err() => key.to_string(),
        _ => quoted(key),
    };
    Some(match kind {
        "string" => format!("GET {key}"),
        "hash" => format!("HSCAN {key} 0 COUNT {COUNT}"),
        "list" => format!("LRANGE {key} 0 {}", rows.max(1) - 1),
        "set" => format!("SSCAN {key} 0 COUNT {COUNT}"),
        "zset" => format!("ZSCAN {key} 0 COUNT {COUNT}"),
        "stream" => format!("XRANGE {key} - + COUNT {rows}"),
        "ReJSON-RL" => format!("JSON.GET {key} $"),
        "TSDB-TYPE" => format!("TS.RANGE {key} - + COUNT {rows}"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_cmd::split;

    fn pattern(predicate: &str) -> Vec<u8> {
        let words = split(predicate).unwrap();
        assert_eq!(words[0], b"MATCH");
        words[1].clone()
    }

    #[test]
    fn a_key_bar_is_a_glob_matching_its_value_literally() {
        let value = r#"a*b?c[d]e\f g"#;
        assert_eq!(
            pattern(&filter_predicate("key", Operator::Equals, value).unwrap()),
            br#"a\*b\?c\[d\]e\\f g"#
        );
        assert_eq!(
            pattern(&filter_predicate("key", Operator::StartsWith, "user:").unwrap()),
            b"user:*"
        );
        assert_eq!(
            pattern(&filter_predicate("key", Operator::Contains, "*").unwrap()),
            br"*\**"
        );
        assert_eq!(
            filter_predicate("type", Operator::Equals, "hash").as_deref(),
            Some("TYPE hash")
        );
        assert_eq!(filter_predicate("type", Operator::Contains, "h"), None);
        assert_eq!(filter_predicate("ttl", Operator::Equals, "1"), None);
    }

    #[test]
    fn only_a_keys_name_and_type_are_offered_and_only_as_scan_can_narrow_them() {
        assert!(filters_on("key") && filters_on("type") && !filters_on("value"));
        assert!(offers(Some("key"), Operator::EndsWith));
        assert!(offers(None, Operator::Contains));
        assert!(!offers(Some("key"), Operator::NotContains));
        assert!(!offers(Some("type"), Operator::StartsWith));
        assert!(!offers(Some("ttl"), Operator::Equals));
    }

    #[test]
    fn the_preview_and_the_count_pass_the_gate_and_nothing_else_does() {
        let filters = [
            String::new(),
            raw_filter("user:*"),
            filter_predicate("type", Operator::Equals, "hash").unwrap(),
            joined(
                Conjunction::And,
                &raw_filter("a b\"c\n"),
                &filter_predicate("type", Operator::Equals, "zset").unwrap(),
            ),
            raw_filter("x COUNT 1\nFLUSHDB"),
            joined(
                Conjunction::And,
                &filter_predicate("type", Operator::Equals, "hash").unwrap(),
                &raw_filter("user:*"),
            ),
        ];
        for filter in &filters {
            assert!(is_generated_read(&preview(filter)), "{filter}");
        }
        assert!(is_generated_read(&count("")));
        assert!(!is_generated_read(&count("MATCH a*")));

        let refused = [
            joined(Conjunction::Or, &raw_filter("a*"), &raw_filter("b*")),
            joined(Conjunction::And, &raw_filter("a*"), &raw_filter("b*")),
            format!("TYPE hash TYPE zset {}", raw_filter("a*")),
            "MATCH a* COUNT 5".to_string(),
        ];
        for filter in &refused {
            assert!(!is_generated_read(&preview(filter)), "{filter}");
        }
        for text in [
            "SCAN 0 MATCH * COUNT 1000\nFLUSHDB",
            "SCAN 0 MATCH * COUNT 1000\n# a comment",
            "scan 0 MATCH * COUNT 1000",
            "SCAN 5 MATCH * COUNT 1000",
            "SCAN 0 MATCH \"*\" COUNT 1000",
            "DBSIZE ",
            "FLUSHDB",
            "",
        ] {
            assert!(!is_generated_read(text), "{text}");
        }
    }

    #[test]
    fn a_key_opens_with_its_types_read_and_its_name_quoted() {
        let read = |key, kind| read(key, kind, 100);
        assert_eq!(
            read("user:1", "hash").as_deref(),
            Some("HSCAN user:1 0 COUNT 1000")
        );
        assert_eq!(
            read("a key \"q\"", "list").as_deref(),
            Some(r#"LRANGE "a key \"q\"" 0 99"#)
        );
        assert_eq!(
            read("tags", "set").as_deref(),
            Some("SSCAN tags 0 COUNT 1000")
        );
        assert_eq!(
            read("leaderboard", "zset").as_deref(),
            Some("ZSCAN leaderboard 0 COUNT 1000")
        );
        assert_eq!(
            read("events", "stream").as_deref(),
            Some("XRANGE events - + COUNT 100")
        );
        assert_eq!(
            read("doc:1", "ReJSON-RL").as_deref(),
            Some("JSON.GET doc:1 $")
        );
        // A name that is not UTF-8 is shown spelled as a word already.
        assert_eq!(
            read(r#""\x00\xff""#, "string").as_deref(),
            Some(r#"GET "\x00\xff""#)
        );
        assert_eq!(read("k", "none"), None);
        assert_eq!(read("k", "vectorset"), None);
    }
}

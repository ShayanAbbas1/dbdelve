//! The keys grid: the `SCAN` its preview runs, the `DBSIZE` its count runs,
//! the `SCAN` options its filter bar writes, and the gate the
//! preview and the count pass.
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

pub(crate) fn preview(filter: &str) -> String {
    match filter.trim() {
        "" => format!("SCAN 0 MATCH * COUNT {COUNT}"),
        options => format!("SCAN 0 {options} COUNT {COUNT}"),
    }
}

/// The status bar's Count. `DBSIZE` counts the whole keyspace and nothing
/// counts a pattern's keys but walking them, which is what the preview already
/// did, so under a filter there is no count to run: the empty text is one the
/// gate refuses.
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
        _ => return false,
    };
    preview(&options) == text
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
        ];
        for filter in &filters {
            assert!(is_generated_read(&preview(filter)), "{filter}");
        }
        assert!(is_generated_read(&count("")));
        assert!(!is_generated_read(&count("MATCH a*")));

        let refused = [
            joined(Conjunction::Or, &raw_filter("a*"), &raw_filter("b*")),
            joined(Conjunction::And, &raw_filter("a*"), &raw_filter("b*")),
            format!("TYPE hash {}", raw_filter("a*")),
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
}

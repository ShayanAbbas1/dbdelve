//! The keys grid: the `SCAN` its preview runs, the `DBSIZE` its count runs,
//! and the gate both pass.
//!
//! A tab's filter is the run of `SCAN` options its bars add up to (`MATCH
//! user:* TYPE hash`), so the preview is that run between the cursor and the
//! page size. `SCAN` has no `LIMIT`: the tab's row limit is the fetch's, which
//! stops following the cursor once the rows reach it.

use super::{parse, quoted};

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

    #[test]
    fn the_preview_and_the_count_pass_the_gate_and_nothing_else_does() {
        let filters = [
            String::new(),
            raw_filter("user:*"),
            raw_filter("a b\"c\n"),
            raw_filter("x COUNT 1\nFLUSHDB"),
        ];
        for filter in &filters {
            assert!(is_generated_read(&preview(filter)), "{filter}");
        }
        assert!(is_generated_read(&count("")));
        assert!(!is_generated_read(&count("MATCH a*")));

        let refused = [
            format!("{} {}", raw_filter("a*"), raw_filter("b*")),
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

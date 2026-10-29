//! The one matcher every list that narrows as it is typed into goes through:
//! the palette, the explorer's filter, the settings search and the editor's
//! completions.
//!
//! One matcher rather than one per surface, because a query that finds a table
//! in the palette and not in the sidebar reads as the sidebar being broken.
//! nucleo's fzf scoring: a subsequence match, with bonuses for word starts,
//! runs of consecutive characters and the start of the text, and penalties for
//! gaps. Space-separated words must each match, in any order, and fzf's
//! `^prefix`, `suffix$`, `'exact` and `!not` atoms work as they do there.

use std::{cmp::Reverse, ops::Range};

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use unicode_segmentation::UnicodeSegmentation;

/// A query, parsed once per keystroke rather than once per candidate.
pub struct Query {
    pattern: Pattern,
    empty: bool,
}

impl Query {
    pub fn new(text: &str) -> Self {
        Self {
            pattern: Pattern::parse(text, CaseMatching::Ignore, Normalization::Smart),
            empty: text.trim().is_empty(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

pub struct Fuzzy {
    matcher: Matcher,
    chars: Vec<char>,
    indices: Vec<u32>,
}

impl Default for Fuzzy {
    fn default() -> Self {
        Self::new()
    }
}

impl Fuzzy {
    pub fn new() -> Self {
        let mut config = Config::DEFAULT;
        // Every list here is read top-down and typed from the front of a name,
        // so of two otherwise equal matches the one nearer the start wins.
        config.prefer_prefix = true;
        Self {
            matcher: Matcher::new(config),
            chars: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// `None` when `text` does not match. An empty query matches everything,
    /// at zero.
    pub fn score(&mut self, query: &Query, text: &str) -> Option<u32> {
        if query.empty {
            return Some(0);
        }
        let haystack = Utf32Str::new(text, &mut self.chars);
        query.pattern.score(haystack, &mut self.matcher)
    }

    pub fn matches(&mut self, query: &Query, text: &str) -> bool {
        self.score(query, text).is_some()
    }

    /// The candidates that match, best first, each with its score.
    ///
    /// A candidate is found by its `text` and by each of the texts in `also`,
    /// scored on its own with the best counting, so no match runs from the
    /// end of one into the next. Equal scores go to the shorter `text`, which
    /// is the one more of whose characters the query accounted for, and then
    /// to the order the candidates came in. An empty query keeps every
    /// candidate in that order.
    pub fn rank<T>(
        &mut self,
        query: &Query,
        candidates: impl IntoIterator<Item = T>,
        text: impl Fn(&T) -> &str,
        also: impl Fn(&T) -> &[String],
    ) -> Vec<(T, u32)> {
        if query.empty {
            return candidates.into_iter().map(|item| (item, 0)).collect();
        }
        let mut scored = candidates
            .into_iter()
            .filter_map(|item| {
                let score = std::iter::once(text(&item))
                    .chain(also(&item).iter().map(String::as_str))
                    .filter_map(|field| self.score(query, field))
                    .max()?;
                Some((item, score))
            })
            .collect::<Vec<_>>();
        scored.sort_by_key(|(item, score)| (Reverse(*score), text(item).len()));
        scored
    }

    /// The byte ranges of `text` the query matched, in order and merged where
    /// they touch, for drawing the matched characters. Empty when nothing
    /// matched or the query is empty.
    ///
    /// Costlier than [`Self::score`], so a list asks only for the rows it
    /// draws, not for every candidate it ranked.
    pub fn highlights(&mut self, query: &Query, text: &str) -> Vec<Range<usize>> {
        if query.empty {
            return Vec::new();
        }
        self.indices.clear();
        let haystack = Utf32Str::new(text, &mut self.chars);
        let by_byte = matches!(haystack, Utf32Str::Ascii(_));
        if query
            .pattern
            .indices(haystack, &mut self.matcher, &mut self.indices)
            .is_none()
        {
            return Vec::new();
        }
        self.indices.sort_unstable();
        self.indices.dedup();
        grapheme_ranges(text, &self.indices, by_byte)
    }
}

/// Match indices, sorted and unique, as merged byte ranges of `text`, each
/// covering the whole grapheme cluster its index lands in.
///
/// nucleo indexes text by grapheme cluster, matching the first char of each,
/// so an index is a cluster's position -- except where every cluster starts
/// with an ASCII char (`e` and a combining accent, say, or `\r\n`), when it
/// matches the text's bytes and an index is a byte offset.
fn grapheme_ranges(text: &str, indices: &[u32], by_byte: bool) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut wanted = indices.iter().map(|&index| index as usize).peekable();
    for (position, (start, grapheme)) in text.grapheme_indices(true).enumerate() {
        if wanted.peek().is_none() {
            break;
        }
        let end = start + grapheme.len();
        let lands = |index: &usize| {
            if by_byte {
                *index < end
            } else {
                *index == position
            }
        };
        if wanted.next_if(lands).is_none() {
            continue;
        }
        while wanted.next_if(lands).is_some() {}
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(labels: &[&str], query: &str) -> Vec<String> {
        let mut fuzzy = Fuzzy::new();
        fuzzy
            .rank(
                &Query::new(query),
                labels.iter().copied(),
                |label| label,
                |_| &[],
            )
            .into_iter()
            .map(|(label, _)| label.to_string())
            .collect()
    }

    fn marked(text: &str, query: &str) -> String {
        let ranges = Fuzzy::new().highlights(&Query::new(query), text);
        let mut out = String::new();
        let mut at = 0;
        for range in ranges {
            out.push_str(&text[at..range.start]);
            out.push('[');
            out.push_str(&text[range.clone()]);
            out.push(']');
            at = range.end;
        }
        out.push_str(&text[at..]);
        out
    }

    #[test]
    fn an_empty_query_keeps_every_candidate_in_order() {
        let labels = ["public.events", "analytics.events", "public.accounts"];
        assert_eq!(ranked(&labels, ""), labels);
        assert_eq!(ranked(&labels, "   "), labels);
    }

    #[test]
    fn a_subsequence_matches_and_anything_else_is_dropped() {
        let labels = ["Run query", "Format query", "New connection"];
        assert_eq!(ranked(&labels, "rq"), ["Run query", "Format query"]);
        assert!(ranked(&labels, "zzz").is_empty());
    }

    #[test]
    fn a_word_start_outranks_the_same_letters_mid_word() {
        let labels = ["Metabase", "Close tab"];
        assert_eq!(ranked(&labels, "tab"), ["Close tab", "Metabase"]);
    }

    #[test]
    fn of_two_equal_matches_the_shorter_name_comes_first() {
        let labels = ["public.account_log", "public.accounts"];
        assert_eq!(ranked(&labels, "account")[0], "public.accounts");
    }

    #[test]
    fn every_word_has_to_match_in_any_order() {
        let labels = ["analytics.events", "public.events", "public.accounts"];
        assert_eq!(ranked(&labels, "ev pub"), ["public.events"]);
    }

    #[test]
    fn case_and_accents_do_not_matter() {
        assert_eq!(ranked(&["Café orders"], "CAFE"), ["Café orders"]);
    }

    #[test]
    fn the_highlights_are_the_matched_characters_merged_into_runs() {
        assert_eq!(marked("Run query", "rq"), "[R]un [q]uery");
        assert_eq!(marked("public.events", "events"), "public.[events]");
        assert_eq!(marked("Run query", ""), "Run query");
        assert_eq!(marked("Run query", "zz"), "Run query");
    }

    #[test]
    fn the_highlights_are_byte_ranges_on_text_that_is_not_ascii() {
        assert_eq!(marked("Café orders", "fe o"), "Ca[fé] [o]rders");
    }

    #[test]
    fn a_letter_and_its_combining_accent_are_marked_as_one() {
        assert_eq!(
            marked("Cafe\u{301} orders", "fe o"),
            "Ca[fe\u{301}] [o]rders"
        );
    }

    #[test]
    fn an_emoji_sequence_counts_as_one_character() {
        assert_eq!(
            marked("👩\u{200d}💻 dev notes", "dev"),
            "👩\u{200d}💻 [dev] notes"
        );
        assert_eq!(
            marked("👩\u{200d}💻 dev notes", "👩"),
            "[👩\u{200d}💻] dev notes"
        );
    }

    #[test]
    fn a_crlf_counts_as_one_character() {
        assert_eq!(
            marked("café\r\nfrom accounts", "from"),
            "café\r\n[from] accounts"
        );
        assert_eq!(
            marked("SELECT 1;\r\nFROM t", "from"),
            "SELECT 1;\r\n[FROM] t"
        );
    }
}

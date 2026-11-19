//! What an agent reads back for rows: plain strings in, one string out.
//!
//! The text is read by a model, so structure is only ever written where this
//! module puts it, as `explain::compact` does for a plan. The first line is
//! `truncated:`; each table then writes its label and the JSON array of its
//! columns on one line, and every other line is one row as a JSON array of
//! strings and `null`. JSON escapes every newline in a cell or a name, so no
//! value from the database can start a line of its own, and nothing that is not
//! a row, a header or a count can be taken for one.
//!
//! Db-free and gpui-free: the window turns a `QueryResult` into a [`Table`].

use serde_json::json;

/// One column of a table: its name and the server's own name for its type, when
/// it gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) name: String,
    pub(crate) data_type: Option<String>,
}

/// How many rows a table has, as far as is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Total {
    /// The statement returned exactly this many.
    Exact(usize),
    /// It was stopped at the limit, so it returned at least this many.
    AtLeast(usize),
}

/// One result set, its rows already rendered to strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table {
    /// The word its header line starts with.
    pub(crate) label: &'static str,
    pub(crate) heads: Vec<Head>,
    pub(crate) rows: Vec<Vec<Option<String>>>,
    pub(crate) total: Total,
}

// ponytail: Claude Code saves a successful result to a file once it passes a
// token cap or 50,000 characters, and an agent that is handed a file path
// instead of rows has lost the read. 24,000 bytes sits under both for any
// tokenizer ratio above one byte per token. The bytes stand in for tokens
// until 05-10 measures tokens per byte on a worst-case result; the upgrade is
// the measured number.
pub(crate) const RESULT_BUDGET: usize = 24_000;

// ponytail: a cell past 2,000 characters is a document, not a value. The clip
// stops one cell of a JSON column from taking the whole budget and leaves the
// agent a marker that says how much was left out; the upgrade is a tool that
// reads one cell whole.
pub(crate) const CELL_CAP: usize = 2_000;

/// One table as it will be written: its leading line and its rows as lines.
struct Written<'a> {
    table: &'a Table,
    /// The `columns:` line, or the count that replaces it.
    lead: String,
    /// A row's line, newline included, and whether a cell of it was clipped.
    rows: Vec<(String, bool)>,
    /// How many of `rows` are written.
    kept: usize,
    /// The names were too many to list, and the rows went with them.
    unlisted: bool,
}

impl Written<'_> {
    fn clipped(&self) -> bool {
        self.rows[..self.kept].iter().any(|(_, clipped)| *clipped)
    }

    fn cut(&self, further: usize) -> Option<String> {
        cut(
            Cut {
                shown: self.kept,
                listed: self.table.rows.len(),
                total: self.table.total,
                budget: self.unlisted || self.kept < self.table.rows.len(),
                clipped: self.clipped(),
            },
            further,
        )
    }
}

/// The reply text for `tables`, with `further` result sets that were not
/// included. The first line says whether rows were left out, so a reader knows
/// what it has before it reads a row. The whole text is at most
/// [`RESULT_BUDGET`] bytes: rows are dropped from the end, never cut through.
///
/// `note` is a sentence of DBDelve's own, written as one `note: ` line after
/// the header. It is a constant of the caller's, never a value from the
/// database, so it can hold no newline of its own.
// ponytail: callers pass one table, or a handful of catalog tables. The
// header is part of the budget and every table's leading line is written even
// when no row follows it, so the budget holds for a few tables and not for
// thousands.
pub(crate) fn text(tables: &[Table], further: usize, note: Option<&str>) -> String {
    let mut written: Vec<Written> = tables.iter().map(write).collect();
    let note = note
        .map(|note| format!("note: {note}\n"))
        .unwrap_or_default();
    let mut body: usize = note.len()
        + written
            .iter()
            .map(|w| w.lead.len() + w.rows.iter().map(|(line, _)| line.len()).sum::<usize>())
            .sum::<usize>();
    loop {
        let head = header(&written, further);
        // `truncated: ` and the newline are the line's own.
        if "truncated: ".len() + head.len() + 1 + body <= RESULT_BUDGET {
            return assemble(&written, &head, &note);
        }
        let Some(last) = written.iter_mut().rev().find(|w| w.kept > 0) else {
            // Nothing is left to drop: the leading lines are all there is.
            return assemble(&written, &head, &note);
        };
        last.kept -= 1;
        body -= last.rows[last.kept].0.len();
    }
}

fn assemble(written: &[Written], head: &str, note: &str) -> String {
    let mut out = format!("truncated: {head}\n{note}");
    for w in written {
        out.push_str(&w.lead);
        for (line, _) in &w.rows[..w.kept] {
            out.push_str(line);
        }
    }
    out
}

/// A table as lines, with every cell clipped to [`CELL_CAP`] characters.
fn write(table: &Table) -> Written<'_> {
    let heads: Vec<_> = table
        .heads
        .iter()
        .map(|head| json!({ "name": head.name, "type": head.data_type }))
        .collect();
    let lead = format!("{}: {}\n", table.label, json_line(&heads));
    // A list of names that alone takes half the budget would leave no room
    // for what the agent asked about.
    if lead.len() > RESULT_BUDGET / 2 {
        return Written {
            table,
            lead: format!(
                "{}: {} columns (names not listed: they exceed the reply budget)\n",
                table.label,
                table.heads.len()
            ),
            rows: Vec::new(),
            kept: 0,
            unlisted: true,
        };
    }
    let rows: Vec<(String, bool)> = table
        .rows
        .iter()
        .map(|row| {
            let mut clipped = false;
            let cells: Vec<Option<String>> = row
                .iter()
                .map(|cell| {
                    cell.as_ref().map(|cell| match clip(cell) {
                        Some(shorter) => {
                            clipped = true;
                            shorter
                        }
                        None => cell.clone(),
                    })
                })
                .collect();
            (format!("{}\n", json_line(&cells)), clipped)
        })
        .collect();
    Written {
        table,
        lead,
        kept: rows.len(),
        rows,
        unlisted: false,
    }
}

/// The cell cut to [`CELL_CAP`] characters and marked, or `None` when it is
/// short enough. The marker is inside the string, so it is JSON like the rest.
fn clip(cell: &str) -> Option<String> {
    let (at, _) = cell.char_indices().nth(CELL_CAP)?;
    Some(format!(
        "{} [clipped from {} characters]",
        &cell[..at],
        cell.chars().count()
    ))
}

/// What the first line says after `truncated: `. One table reads `none (N
/// rows)` when nothing was cut, and otherwise `S of T rows shown (reasons)`,
/// `T` being a number or `at least` one. Several tables read `none` or name
/// each cut table by its label. The result sets that were not included are a
/// reason on the last table, since they would have come after it.
// ponytail: the wording of a cut is the least a reader needs to know that rows
// are missing and how many there were. The upgrade is a second line that says
// what to ask for next, if agents turn out to need it.
fn header(written: &[Written], further: usize) -> String {
    match written {
        [] if further == 0 => "none (0 rows)".to_string(),
        [] => cut(
            Cut {
                shown: 0,
                listed: 0,
                total: Total::Exact(0),
                budget: false,
                clipped: false,
            },
            further,
        )
        .unwrap_or_default(),
        [one] => one
            .cut(further)
            .unwrap_or_else(|| format!("none ({})", rows(one.kept))),
        _ => {
            let cuts: Vec<String> = written
                .iter()
                .enumerate()
                .filter_map(|(at, w)| {
                    let further = if at + 1 == written.len() { further } else { 0 };
                    Some(format!("{}: {}", w.table.label, w.cut(further)?))
                })
                .collect();
            if cuts.is_empty() {
                "none".to_string()
            } else {
                cuts.join("; ")
            }
        }
    }
}

/// What was done to one table's rows.
struct Cut {
    /// Rows written.
    shown: usize,
    /// Rows the table was given.
    listed: usize,
    total: Total,
    /// Rows or names were left out to stay inside the byte budget.
    budget: bool,
    /// A written cell was clipped.
    clipped: bool,
}

/// How a table was cut, or `None` when it was not. The reasons are always in
/// one order: `row cap`, `byte budget`, `clipped cells`, then the result sets
/// not included.
fn cut(cut: Cut, further: usize) -> Option<String> {
    let mut reasons = Vec::new();
    let of = match cut.total {
        Total::Exact(total) => {
            if total > cut.listed {
                reasons.push("row cap".to_string());
            }
            total.to_string()
        }
        // A statement that was stopped at the limit has more behind it.
        Total::AtLeast(total) => {
            reasons.push("row cap".to_string());
            format!("at least {total}")
        }
    };
    if cut.budget {
        reasons.push("byte budget".to_string());
    }
    if cut.clipped {
        reasons.push("clipped cells".to_string());
    }
    if further > 0 {
        reasons.push(match further {
            1 => "1 further result set not shown".to_string(),
            _ => format!("{further} further result sets not shown"),
        });
    }
    if reasons.is_empty() {
        return None;
    }
    Some(format!(
        "{} of {of} rows shown ({})",
        cut.shown,
        reasons.join(", ")
    ))
}

fn rows(count: usize) -> String {
    match count {
        1 => "1 row".to_string(),
        _ => format!("{count} rows"),
    }
}

fn json_line(value: &impl serde::Serialize) -> String {
    // Strings, nulls and objects of them always serialize; the empty line is
    // what the impossible case would write, and no row is taken for one.
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, data_type: Option<&str>) -> Head {
        Head {
            name: name.to_string(),
            data_type: data_type.map(str::to_string),
        }
    }

    #[test]
    fn rows_follow_a_header_and_a_columns_line_as_json_arrays() {
        let table = Table {
            label: "columns",
            heads: vec![head("id", Some("int4")), head("email", None)],
            rows: vec![
                vec![Some("1".to_string()), Some("a@x".to_string())],
                vec![Some("2".to_string()), None],
            ],
            total: Total::Exact(2),
        };
        assert_eq!(
            text(&[table], 0, None),
            "truncated: none (2 rows)\n\
             columns: [{\"name\":\"id\",\"type\":\"int4\"},{\"name\":\"email\",\"type\":null}]\n\
             [\"1\",\"a@x\"]\n\
             [\"2\",null]\n"
        );
    }

    #[test]
    fn a_note_is_one_line_after_the_header_and_counts_toward_the_budget() {
        let table = || wide(12, &"a".repeat(1_900), Total::Exact(12));
        let note = "n".repeat(300);
        let out = text(&[wide(1, "a", Total::Exact(1))], 0, Some(&note));
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("truncated: none (1 row)"));
        assert_eq!(lines.next(), Some(format!("note: {note}").as_str()));
        assert!(
            lines
                .next()
                .is_some_and(|line| line.starts_with("columns: "))
        );
        // Twelve of these rows fit alone, and a note that takes the room of
        // one of them costs it.
        assert!(text(&[table()], 0, None).starts_with("truncated: none (12 rows)\n"));
        let crowded = text(&[table()], 0, Some(&"n".repeat(1_300)));
        assert!(crowded.len() <= RESULT_BUDGET, "{}", crowded.len());
        assert!(
            crowded.starts_with("truncated: 11 of 12 rows shown (byte budget)\nnote: "),
            "{}",
            &crowded[..70]
        );
    }

    #[test]
    fn a_cell_that_looks_like_a_header_never_starts_a_line() {
        let table = Table {
            label: "columns",
            heads: vec![head("x\ncolumns: y", None)],
            rows: vec![
                vec![Some("\ntruncated: none".to_string())],
                vec![Some("a\r\n[1]\ncolumns: z".to_string())],
            ],
            total: Total::Exact(2),
        };
        let out = text(&[table], 0, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out:?}");
        assert!(lines[0].starts_with("truncated: "));
        assert!(lines[1].starts_with("columns: "));
        for line in &lines[2..] {
            assert!(line.starts_with('['), "{line:?}");
        }
    }

    #[test]
    fn the_header_says_how_many_rows_were_shown_of_what_total() {
        let table = |count: usize, total| Table {
            label: "columns",
            heads: vec![head("n", None)],
            rows: (0..count).map(|n| vec![Some(n.to_string())]).collect(),
            total,
        };
        let first = |count, total, further| {
            text(&[table(count, total)], further, None)
                .lines()
                .next()
                .expect("a text has a first line")
                .to_string()
        };
        assert_eq!(
            first(100, Total::Exact(100), 0),
            "truncated: none (100 rows)"
        );
        assert_eq!(
            first(100, Total::Exact(150), 0),
            "truncated: 100 of 150 rows shown (row cap)"
        );
        assert_eq!(
            first(100, Total::AtLeast(100), 0),
            "truncated: 100 of at least 100 rows shown (row cap)"
        );
        assert_eq!(first(1, Total::Exact(1), 0), "truncated: none (1 row)");
        assert_eq!(
            first(3, Total::Exact(3), 2),
            "truncated: 3 of 3 rows shown (2 further result sets not shown)"
        );
        assert_eq!(
            first(100, Total::AtLeast(100), 2),
            "truncated: 100 of at least 100 rows shown (row cap, 2 further result sets not shown)"
        );
        assert_eq!(
            text(&[table(0, Total::Exact(0))], 0, None),
            "truncated: none (0 rows)\ncolumns: [{\"name\":\"n\",\"type\":null}]\n"
        );
    }

    #[test]
    fn several_tables_name_each_one_that_was_cut() {
        let table = |label, count: usize, total| Table {
            label,
            heads: vec![head("n", None)],
            rows: (0..count).map(|n| vec![Some(n.to_string())]).collect(),
            total,
        };
        let first = |tables: &[Table], further| {
            text(tables, further, None)
                .lines()
                .next()
                .expect("a text has a first line")
                .to_string()
        };
        assert_eq!(
            first(
                &[
                    table("columns", 2, Total::Exact(2)),
                    table("indexes", 1, Total::Exact(1))
                ],
                0
            ),
            "truncated: none"
        );
        assert_eq!(
            first(
                &[
                    table("columns", 2, Total::Exact(2)),
                    table("indexes", 1, Total::Exact(4))
                ],
                0
            ),
            "truncated: indexes: 1 of 4 rows shown (row cap)"
        );
        assert_eq!(
            first(
                &[
                    table("columns", 1, Total::Exact(3)),
                    table("indexes", 1, Total::Exact(4))
                ],
                0
            ),
            "truncated: columns: 1 of 3 rows shown (row cap); indexes: 1 of 4 rows shown (row cap)"
        );
    }

    /// A one-column table of `count` rows of `cell`.
    fn wide(count: usize, cell: &str, total: Total) -> Table {
        Table {
            label: "columns",
            heads: vec![head("c", None)],
            rows: (0..count).map(|_| vec![Some(cell.to_string())]).collect(),
            total,
        }
    }

    #[test]
    fn a_result_is_cut_at_a_row_boundary_under_the_budget_with_its_header_first() {
        let out = text(&[wide(100, &"a".repeat(500), Total::Exact(100))], 0, None);
        assert!(out.len() <= RESULT_BUDGET, "{}", out.len());
        assert!(out.ends_with("\"]\n"), "the text ends after a whole row");
        let mut lines = out.lines();
        let shown = out.lines().filter(|line| line.starts_with("[\"")).count();
        assert!((1..100).contains(&shown), "{shown}");
        assert_eq!(
            lines.next(),
            Some(format!("truncated: {shown} of 100 rows shown (byte budget)").as_str())
        );
        assert!(
            lines
                .next()
                .is_some_and(|line| line.starts_with("columns: "))
        );
        // Every row line that is there is a whole one.
        assert!(lines.all(|line| line.len() == 504 && line.ends_with("\"]")));
    }

    #[test]
    fn a_result_exactly_at_the_budget_is_kept_whole() {
        // Every character added to the last cell is one byte of text, so the
        // empty one says how many are left.
        let table = |last: usize| {
            let mut table = wide(13, &"a".repeat(1_900), Total::Exact(13));
            table.rows[12][0] = Some("a".repeat(last));
            table
        };
        let last = RESULT_BUDGET - text(&[table(0)], 0, None).len();
        let kept = text(&[table(last)], 0, None);
        assert_eq!(kept.len(), RESULT_BUDGET);
        assert!(
            kept.starts_with("truncated: none (13 rows)\n"),
            "{}",
            &kept[..60]
        );
        let over = text(&[table(last + 1)], 0, None);
        assert!(over.len() <= RESULT_BUDGET);
        assert!(
            over.starts_with("truncated: 12 of 13 rows shown (byte budget)\n"),
            "{}",
            &over[..60]
        );
        assert_eq!(over.lines().count(), 14);
    }

    #[test]
    fn an_oversized_cell_is_clipped_on_a_character_boundary() {
        let cell = "\u{20ac}".repeat(CELL_CAP + 10);
        let out = text(&[wide(1, &cell, Total::Exact(1))], 0, None);
        let row: Vec<String> = serde_json::from_str(out.lines().nth(2).expect("a row line"))
            .expect("the row is a JSON array");
        assert_eq!(
            row[0],
            format!(
                "{} [clipped from 2010 characters]",
                "\u{20ac}".repeat(CELL_CAP)
            )
        );
        assert_eq!(
            out.lines().next(),
            Some("truncated: 1 of 1 rows shown (clipped cells)")
        );
        // A cell of exactly the cap is left alone.
        let whole = text(&[wide(1, &"x".repeat(CELL_CAP), Total::Exact(1))], 0, None);
        assert!(whole.starts_with("truncated: none (1 row)\n"));
    }

    #[test]
    fn too_many_columns_to_list_are_counted_instead() {
        let mut table = wide(3, "x", Total::Exact(3));
        table.heads = (0..5_000)
            .map(|n| head(&format!("column_{n:04}"), None))
            .collect();
        let out = text(&[table], 0, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out:?}");
        assert!(lines[0].contains("(byte budget)"), "{}", lines[0]);
        assert_eq!(
            lines[1],
            "columns: 5000 columns (names not listed: they exceed the reply budget)"
        );
        assert!(out.len() <= RESULT_BUDGET);
    }

    #[test]
    fn the_reasons_are_listed_in_one_order() {
        let mut table = wide(60, &"a".repeat(500), Total::AtLeast(60));
        table.rows[0][0] = Some("b".repeat(CELL_CAP + 1));
        let out = text(&[table], 2, None);
        let shown = out.lines().filter(|line| line.starts_with("[\"")).count();
        assert_eq!(
            out.lines().next(),
            Some(
                format!(
                    "truncated: {shown} of at least 60 rows shown (row cap, byte budget, \
                     clipped cells, 2 further result sets not shown)"
                )
                .as_str()
            )
        );
    }

    /// Not a check: writes the widest reply this module allows, once per kind of
    /// value that costs the most tokens per byte, to the directory in
    /// `DBDELVE_RESULT_OUT`. Plan 05-10 has the tokens of those files counted
    /// and sets [`RESULT_BUDGET`] from the highest count.
    #[test]
    #[ignore = "measurement aid: writes DBDELVE_RESULT_OUT"]
    fn the_worst_case_replies_are_written_for_measurement() {
        // CI runs every ignored test, this one included; with no directory named
        // there is nothing to write.
        let Ok(dir) = std::env::var("DBDELVE_RESULT_OUT") else {
            return;
        };
        // A fixed generator, so a second run writes the same bytes.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let hex = |next: &mut dyn FnMut() -> u64, len: usize| -> String {
            (0..len)
                .map(|_| char::from(b"0123456789abcdef"[(next() % 16) as usize]))
                .collect()
        };
        let uuid = |next: &mut dyn FnMut() -> u64| {
            format!(
                "{}-{}-{}-{}-{}",
                hex(next, 8),
                hex(next, 4),
                hex(next, 4),
                hex(next, 4),
                hex(next, 12)
            )
        };
        let base64 = |next: &mut dyn FnMut() -> u64| -> String {
            let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let body: String = (0..62)
                .map(|_| char::from(alphabet[(next() % 64) as usize]))
                .collect();
            format!("{body}==")
        };
        let json = |next: &mut dyn FnMut() -> u64| {
            format!(
                "{{\"id\":\"{}\",\"tags\":[\"{}\",\"{}\"],\"n\":{},\"ok\":true,\"at\":null}}",
                uuid(next),
                hex(next, 6),
                hex(next, 6),
                next() % 100_000
            )
        };
        let cjk = |next: &mut dyn FnMut() -> u64| -> String {
            (0..40)
                .map(|_| {
                    char::from_u32(0x4E00 + (next() % 0x51A5) as u32).expect("a CJK ideograph")
                })
                .collect()
        };
        // Enough rows that every text is cut by the budget: the cheapest cells
        // are 36 bytes, so a few thousand rows is several times the budget.
        let rows = 3_000;
        type Cell<'a> = &'a dyn Fn(&mut dyn FnMut() -> u64) -> String;
        let hex64 = |next: &mut dyn FnMut() -> u64| hex(next, 64);
        let families: [(&str, Vec<Cell>); 6] = [
            ("uuid", vec![&uuid; 4]),
            ("hex", vec![&hex64; 4]),
            ("base64", vec![&base64; 4]),
            ("json", vec![&json; 4]),
            ("cjk", vec![&cjk; 4]),
            ("mixed", vec![&uuid, &base64, &json, &cjk]),
        ];
        for (name, cells) in families {
            let table = Table {
                label: "columns",
                heads: (0..4).map(|i| head(&format!("c{i}"), None)).collect(),
                rows: (0..rows)
                    .map(|_| cells.iter().map(|cell| Some(cell(&mut next))).collect())
                    .collect(),
                total: Total::AtLeast(rows),
            };
            let out = text(&[table], 0, None);
            assert!(out.starts_with("truncated: "), "{name}");
            assert!(out.len() <= RESULT_BUDGET, "{name}: {}", out.len());
            let shown = out.lines().filter(|line| line.starts_with("[\"")).count();
            assert!(shown < rows, "{name}: the text was not cut");
            std::fs::write(format!("{dir}/{name}.txt"), &out).expect("the output writes");
            println!("{name}: bytes={} rows_shown={shown}", out.len());
        }
    }
}

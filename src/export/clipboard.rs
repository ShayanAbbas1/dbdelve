use std::fmt::{self, Write};

use unicode_width::UnicodeWidthStr;

use super::{COPIED_ROWS_TEXT_LIMIT, Format, render};
use crate::db::{Engine, QueryResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardFormat {
    Tsv,
    Csv,
    Markdown,
    UnicodeTable,
    AsciiTable,
}

impl ClipboardFormat {
    pub const ALL: [Self; 5] = [
        Self::Tsv,
        Self::Csv,
        Self::Markdown,
        Self::UnicodeTable,
        Self::AsciiTable,
    ];

    /// The format name shared by the menu, palette and copy confirmation.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tsv => "TSV",
            Self::Csv => "CSV",
            Self::Markdown => "Markdown",
            Self::UnicodeTable => "Unicode Table",
            Self::AsciiTable => "ASCII Table",
        }
    }

    /// Render clipboard text, limiting table formats to 16 MiB.
    /// TSV and CSV retain the file renderer's unbounded output behavior.
    pub fn render(self, engine: Engine, result: &QueryResult) -> Result<String, &'static str> {
        match self {
            Self::Tsv => Ok(render(Format::Tsv, result)),
            Self::Csv => Ok(render(Format::Csv, result)),
            _ => self
                .table(engine, result)
                .map_err(|_| "Table output exceeds 16 MiB. Copy as TSV/CSV or export to a file."),
        }
    }

    /// Measure escaped display widths before padding, so Unicode cells align
    /// without truncating values. Engine type metadata determines numeric alignment.
    fn table(self, engine: Engine, result: &QueryResult) -> Result<String, fmt::Error> {
        if result.columns.is_empty() {
            return Ok(String::new());
        }
        let numeric: Vec<_> = result
            .columns
            .iter()
            .map(|column| {
                column
                    .data_type
                    .as_deref()
                    .is_some_and(|kind| engine.is_numeric_type(kind))
            })
            .collect();
        let mut widths = Vec::with_capacity(result.columns.len());
        for (ix, column) in result.columns.iter().enumerate() {
            let header = self.cell(&column.name)?;
            let minimum = if self == Self::Markdown {
                if numeric[ix] { 4 } else { 3 }
            } else {
                0
            };
            widths.push(UnicodeWidthStr::width(header.as_str()).max(minimum));
        }
        for row in &result.rows {
            for (ix, width) in widths.iter_mut().enumerate() {
                let cell = self.cell(row.get(ix).unwrap_or("NULL"))?;
                *width = (*width).max(UnicodeWidthStr::width(cell.as_str()));
            }
        }

        let mut out = TableText::default();
        if self != Self::Markdown {
            self.border(&mut out, &widths, false, false)?;
            out.write_char('\n')?;
        }
        self.line(
            &mut out,
            &widths,
            &numeric,
            true,
            result.columns.iter().map(|c| c.name.as_str()),
        )?;
        out.write_char('\n')?;
        if self == Self::Markdown {
            out.write_char('|')?;
            for (width, numeric) in widths.iter().zip(&numeric) {
                out.write_char(' ')?;
                out.repeat('-', width - usize::from(*numeric))?;
                if *numeric {
                    out.write_char(':')?;
                }
                out.write_str(" |")?;
            }
        } else {
            self.border(&mut out, &widths, true, false)?;
        }
        for row in &result.rows {
            out.write_char('\n')?;
            self.line(
                &mut out,
                &widths,
                &numeric,
                false,
                (0..widths.len()).map(|ix| row.get(ix).unwrap_or("NULL")),
            )?;
        }
        if self != Self::Markdown {
            out.write_char('\n')?;
            self.border(&mut out, &widths, false, true)?;
        }
        Ok(out.0)
    }

    /// Write one padded row, keeping headers left-aligned even in numeric columns.
    fn line<'a>(
        self,
        out: &mut TableText,
        widths: &[usize],
        numeric: &[bool],
        header: bool,
        cells: impl Iterator<Item = &'a str>,
    ) -> fmt::Result {
        let edge = if self == Self::UnicodeTable {
            '│'
        } else {
            '|'
        };
        out.write_char(edge)?;
        for ((value, width), numeric) in cells.zip(widths).zip(numeric) {
            let cell = self.cell(value)?;
            let padding = width - UnicodeWidthStr::width(cell.as_str());
            out.write_char(' ')?;
            if *numeric && !header {
                out.repeat(' ', padding)?;
            }
            out.write_str(&cell)?;
            if !numeric || header {
                out.repeat(' ', padding)?;
            }
            out.write_char(' ')?;
            out.write_char(edge)?;
        }
        Ok(())
    }

    /// Write a table rule with junctions for its top, middle or bottom position.
    fn border(
        self,
        out: &mut TableText,
        widths: &[usize],
        middle: bool,
        bottom: bool,
    ) -> fmt::Result {
        let (left, join, right, rule) = match self {
            Self::UnicodeTable if middle => ('├', '┼', '┤', '─'),
            Self::UnicodeTable if bottom => ('└', '┴', '┘', '─'),
            Self::UnicodeTable => ('┌', '┬', '┐', '─'),
            _ => ('+', '+', '+', '-'),
        };
        out.write_char(left)?;
        for (ix, width) in widths.iter().enumerate() {
            out.repeat(rule, width + 2)?;
            out.write_char(if ix + 1 == widths.len() { right } else { join })?;
        }
        Ok(())
    }

    /// Escape markup and controls so each value stays in one physical table row.
    /// Normalize CRLF as one break and distinguish literal backslashes from escapes.
    fn cell(self, value: &str) -> Result<String, fmt::Error> {
        let markdown = self == Self::Markdown;
        let mut out = TableText::default();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' | '\n' => {
                    if ch == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.write_str(if markdown { "<br>" } else { "\\n" })?;
                }
                '\t' => out.write_str(if markdown { "\\\\t" } else { "\\t" })?,
                '\\' => out.write_str("\\\\")?,
                '&' if markdown => out.write_str("&amp;")?,
                '<' if markdown => out.write_str("&lt;")?,
                '>' if markdown => out.write_str("&gt;")?,
                '|' | '`' | '*' | '_' | '[' | ']' | '~' if markdown => {
                    out.write_char('\\')?;
                    out.write_char(ch)?;
                }
                ch if ch.is_control()
                    || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
                {
                    if markdown {
                        out.write_char('\\')?;
                    }
                    write!(out, "\\u{{{:x}}}", ch as u32)?;
                }
                ch => out.write_char(ch)?,
            }
        }
        Ok(out.0)
    }
}

/// Both escaped cells and the final padded table must fit before clipboard
/// metadata can safely carry their original rows.
#[derive(Default)]
struct TableText(String);

impl TableText {
    /// Reject padding that would exceed the UTF-8 byte limit before allocating it.
    fn repeat(&mut self, ch: char, count: usize) -> fmt::Result {
        if count > (COPIED_ROWS_TEXT_LIMIT - self.0.len()) / ch.len_utf8() {
            return Err(fmt::Error);
        }
        self.0.extend(std::iter::repeat_n(ch, count));
        Ok(())
    }
}

impl Write for TableText {
    /// Append text only when the complete slice fits, leaving output intact on error.
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > COPIED_ROWS_TEXT_LIMIT - self.0.len() {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Column, rows};

    /// Include numeric metadata, numeric-looking text, NULL, empty text and literal NULL.
    fn sample() -> QueryResult {
        QueryResult {
            columns: vec![
                Column {
                    name: "n".into(),
                    data_type: Some("numeric".into()),
                },
                Column {
                    name: "text".into(),
                    data_type: Some("text".into()),
                },
            ],
            rows: rows(vec![
                vec![Some("-1.5".into()), Some("00123".into())],
                vec![None, Some(String::new())],
                vec![Some("7".into()), Some("NULL".into())],
            ]),
            ..QueryResult::default()
        }
    }

    /// Lock down complete table layouts and retain the existing TSV/CSV rendering.
    #[test]
    fn table_formats_preserve_headers_values_and_numeric_alignment() {
        let result = sample();
        for (format, expected) in [
            (
                ClipboardFormat::Markdown,
                concat!(
                    "| n    | text  |\n",
                    "| ---: | ----- |\n",
                    "| -1.5 | 00123 |\n",
                    "| NULL |       |\n",
                    "|    7 | NULL  |"
                ),
            ),
            (
                ClipboardFormat::AsciiTable,
                concat!(
                    "+------+-------+\n",
                    "| n    | text  |\n",
                    "+------+-------+\n",
                    "| -1.5 | 00123 |\n",
                    "| NULL |       |\n",
                    "|    7 | NULL  |\n",
                    "+------+-------+"
                ),
            ),
            (
                ClipboardFormat::UnicodeTable,
                concat!(
                    "┌──────┬───────┐\n",
                    "│ n    │ text  │\n",
                    "├──────┼───────┤\n",
                    "│ -1.5 │ 00123 │\n",
                    "│ NULL │       │\n",
                    "│    7 │ NULL  │\n",
                    "└──────┴───────┘"
                ),
            ),
        ] {
            assert_eq!(format.render(Engine::Postgres, &result).unwrap(), expected);
        }
        for (clipboard, file) in [
            (ClipboardFormat::Tsv, Format::Tsv),
            (ClipboardFormat::Csv, Format::Csv),
        ] {
            assert_eq!(
                clipboard.render(Engine::Postgres, &result).unwrap(),
                render(file, &result)
            );
        }
    }

    /// Exercise the same escaping for headers and values without adding physical rows.
    #[test]
    fn escaping_keeps_markup_and_control_characters_inside_one_physical_line() {
        let value = "|\\<b>&*x*_`[a]~\r\nnext\rtab\t\0\u{1b}\u{2028}\u{202e}";
        assert_eq!(
            ClipboardFormat::Markdown.cell(value).unwrap(),
            "\\|\\\\&lt;b&gt;&amp;\\*x\\*\\_\\`\\[a\\]\\~<br>next<br>tab\\\\t\\\\u{0}\\\\u{1b}\\\\u{2028}\\\\u{202e}"
        );
        for format in [ClipboardFormat::AsciiTable, ClipboardFormat::UnicodeTable] {
            assert_eq!(
                format.cell(value).unwrap(),
                "|\\\\<b>&*x*_`[a]~\\nnext\\ntab\\t\\u{0}\\u{1b}\\u{2028}\\u{202e}"
            );
        }
        let mut result = sample();
        result.columns[1].name = value.into();
        result.rows = rows(vec![vec![Some("2".into()), Some(value.into())]]);
        for format in ClipboardFormat::ALL.into_iter().skip(2) {
            let text = format.render(Engine::Postgres, &result).unwrap();
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(
                lines.len(),
                if format == ClipboardFormat::Markdown {
                    3
                } else {
                    5
                }
            );
            let width = UnicodeWidthStr::width(lines[0]);
            assert!(
                lines
                    .iter()
                    .all(|line| UnicodeWidthStr::width(*line) == width)
            );
        }
    }

    /// Catch byte- or character-count padding that misaligns wide and combining text.
    #[test]
    fn padding_uses_display_width_for_cjk_emoji_and_combining_marks() {
        let mut result = sample();
        result.rows = rows(vec![
            vec![Some("1".into()), Some("漢字".into())],
            vec![Some("2".into()), Some("👩‍💻".into())],
            vec![Some("3".into()), Some("e\u{301}".into())],
        ]);
        assert_eq!(
            ClipboardFormat::AsciiTable
                .render(Engine::Postgres, &result)
                .unwrap(),
            "+---+------+\n| n | text |\n+---+------+\n| 1 | 漢字 |\n| 2 | 👩‍💻   |\n| 3 | e\u{301}    |\n+---+------+"
        );
    }

    /// Preserve column positions when names cannot identify them and there are no rows.
    #[test]
    fn empty_and_duplicate_headers_keep_their_positions_even_without_rows() {
        let mut result = sample();
        result.columns[0].name.clear();
        result.columns[1].name.clear();
        result.rows.clear();
        assert_eq!(
            ClipboardFormat::AsciiTable
                .render(Engine::Postgres, &result)
                .unwrap(),
            "+--+--+\n|  |  |\n+--+--+\n+--+--+"
        );
        assert_eq!(
            ClipboardFormat::Markdown
                .render(Engine::Postgres, &result)
                .unwrap(),
            "|      |     |\n| ---: | --- |"
        );
        for format in ClipboardFormat::ALL.into_iter().skip(2) {
            assert_eq!(
                format
                    .render(Engine::Postgres, &QueryResult::default())
                    .unwrap(),
                ""
            );
        }
    }

    /// Check the inclusive byte boundary, including escaping growth and multibyte text.
    #[test]
    fn table_output_accepts_exactly_sixteen_mib_and_refuses_one_more_byte() {
        let width = (COPIED_ROWS_TEXT_LIMIT - 24) / 5;
        let mut value = "x".repeat(width);
        value.push('\u{301}'); // Two UTF-8 bytes without another display cell.
        let mut result = QueryResult {
            columns: vec![Column {
                name: "h".into(),
                data_type: None,
            }],
            rows: rows(vec![vec![Some(value.clone())]]),
            ..QueryResult::default()
        };
        let text = ClipboardFormat::AsciiTable
            .render(Engine::Postgres, &result)
            .unwrap();
        assert_eq!(text.len(), COPIED_ROWS_TEXT_LIMIT);
        assert!(text.contains(&value), "long values must not be truncated");
        let mut out = TableText(text);
        assert!(out.write_char('x').is_err());
        assert_eq!(out.0.len(), COPIED_ROWS_TEXT_LIMIT);
        result.columns[0].name = "é".into();
        result.rows = rows(vec![vec![Some(value)]]);
        assert!(
            ClipboardFormat::AsciiTable
                .render(Engine::Postgres, &result)
                .is_err()
        );
        result.rows = rows(vec![vec![Some("&".repeat(COPIED_ROWS_TEXT_LIMIT / 5 + 1))]]);
        assert!(
            ClipboardFormat::Markdown
                .render(Engine::Postgres, &result)
                .is_err()
        );
        assert!(
            ClipboardFormat::UnicodeTable
                .render(Engine::Postgres, &result)
                .is_err()
        );
    }
}

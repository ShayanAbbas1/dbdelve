//! Turns a file's bytes into a [`Table`]: decoded text, rows of cells, and the
//! problems found on the way. It decides nothing about what a cell means.

use std::path::Path;

use std::collections::HashMap;
use std::fmt;

use encoding_rs::{DecoderResult, UTF_16BE, UTF_16LE, WINDOWS_1252};
use serde::de::{Deserialize, Deserializer, Error as _, MapAccess, Visitor};
use serde_json::value::RawValue;

use super::{Delimiter, Encoding, Format, Options, Problem};

pub(crate) fn detect(path: &Path, bytes: &[u8]) -> Format {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("tsv") => return Format::Tsv,
        Some("json" | "jsonl" | "ndjson") => return Format::Json,
        Some("csv") => return Format::Csv,
        _ => {}
    }
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    match body.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'[' | b'{') => Format::Json,
        _ => Format::Csv,
    }
}

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16BE_BOM: &[u8] = &[0xFE, 0xFF];

impl Encoding {
    pub(crate) fn sniff(bytes: &[u8]) -> Self {
        if bytes.starts_with(UTF16LE_BOM) {
            Self::Utf16Le
        } else if bytes.starts_with(UTF16BE_BOM) {
            Self::Utf16Be
        } else if bytes.contains(&0) {
            // UTF-8 text never holds a zero byte, and UTF-16 without a BOM is
            // full of them: in the high half of each ASCII character.
            let at = |parity| {
                bytes
                    .iter()
                    .skip(parity)
                    .step_by(2)
                    .filter(|&&b| b == 0)
                    .count()
            };
            if at(1) >= at(0) {
                Self::Utf16Le
            } else {
                Self::Utf16Be
            }
        } else {
            Self::Utf8
        }
    }
}

pub(crate) fn decode(bytes: &[u8], encoding: Encoding) -> Result<String, String> {
    let (decoder, label, bom) = match encoding {
        Encoding::Utf8 => (None, "UTF-8", UTF8_BOM),
        Encoding::Utf16Le => (Some(UTF_16LE), "UTF-16 LE", UTF16LE_BOM),
        Encoding::Utf16Be => (Some(UTF_16BE), "UTF-16 BE", UTF16BE_BOM),
        Encoding::Windows1252 => (Some(WINDOWS_1252), "Windows-1252", &[][..]),
    };
    let body = bytes.strip_prefix(bom).unwrap_or(bytes);
    let stripped = bytes.len() - body.len();
    let invalid = |at: usize| format!("The file is not valid {label} at byte {}.", stripped + at);

    let text = match decoder {
        None => std::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|e| invalid(e.valid_up_to()))?,
        Some(decoder) => decode_with(decoder, body).map_err(invalid)?,
    };
    // No text field holds a NUL, and one is the mark of a file read in the wrong encoding.
    if let Some(at) = text.find('\0') {
        let before = &text[..at];
        let width = match encoding {
            Encoding::Utf8 => before.len(),
            Encoding::Utf16Le | Encoding::Utf16Be => before.encode_utf16().count() * 2,
            Encoding::Windows1252 => before.chars().count(),
        };
        return Err(format!(
            "The file holds a NUL character at byte {}.",
            stripped + width
        ));
    }
    Ok(text)
}

/// The decoded text, or the offset into `body` of the first malformed sequence.
fn decode_with(encoding: &'static encoding_rs::Encoding, body: &[u8]) -> Result<String, usize> {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut out = String::new();
    let mut offset = 0;
    loop {
        out.reserve(body.len() - offset + 4);
        let (result, read) =
            decoder.decode_to_string_without_replacement(&body[offset..], &mut out, true);
        offset += read;
        match result {
            DecoderResult::InputEmpty => return Ok(out),
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(bad, after) => {
                return Err(offset - bad as usize - after as usize);
            }
        }
    }
}

pub(crate) struct Field {
    pub(crate) text: String,
    pub(crate) quoted: bool,
}

pub(crate) struct Record {
    pub(crate) line: usize,
    pub(crate) fields: Vec<Field>,
}

impl Record {
    fn is_blank(&self) -> bool {
        matches!(self.fields.as_slice(), [f] if f.text.is_empty() && !f.quoted)
    }
}

pub(crate) fn records(
    text: &str,
    delimiter: char,
    quote: char,
    first_line: usize,
) -> Result<Vec<Record>, String> {
    parse(text, delimiter, quote, first_line, usize::MAX)
}

/// Stops once `filled` records that are not blank have been read, which is all
/// the sniffer needs of a large file.
fn parse(
    text: &str,
    delimiter: char,
    quote: char,
    first_line: usize,
    filled: usize,
) -> Result<Vec<Record>, String> {
    let mut chars = text.chars().peekable();
    let mut line = first_line;
    let mut out: Vec<Record> = Vec::new();
    let mut seen = 0;
    while chars.peek().is_some() && seen < filled {
        let start = line;
        let mut fields = Vec::new();
        loop {
            let mut field = Field {
                text: String::new(),
                quoted: false,
            };
            if chars.peek() == Some(&quote) {
                chars.next();
                field.quoted = true;
                let opened = line;
                loop {
                    match chars.next() {
                        None => {
                            return Err(format!(
                                "A quoted field opened on line {opened} is never closed."
                            ));
                        }
                        Some(c) if c == quote => {
                            if chars.peek() == Some(&quote) {
                                chars.next();
                                field.text.push(quote);
                            } else {
                                break;
                            }
                        }
                        Some(c) => {
                            field.text.push(c);
                            if c == '\r' {
                                if chars.peek() == Some(&'\n') {
                                    field.text.push(chars.next().unwrap());
                                }
                                line += 1;
                            } else if c == '\n' {
                                line += 1;
                            }
                        }
                    }
                }
                if !matches!(chars.peek(), None | Some('\n' | '\r'))
                    && chars.peek() != Some(&delimiter)
                {
                    return Err(format!("Line {line} has text after a closing quote."));
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == delimiter || c == '\n' || c == '\r' {
                        break;
                    }
                    field.text.push(c);
                    chars.next();
                }
            }
            fields.push(field);
            match chars.next() {
                Some(c) if c == delimiter => {}
                Some('\r') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    line += 1;
                    break;
                }
                Some(_) => {
                    line += 1;
                    break;
                }
                None => break,
            }
        }
        let record = Record {
            line: start,
            fields,
        };
        seen += usize::from(!record.is_blank());
        out.push(record);
    }
    Ok(out)
}

pub(crate) fn sniff(text: &str, quote: char) -> char {
    let mut best = (',', 1);
    for candidate in [',', ';', '\t', '|'] {
        let Ok(sample) = parse(text, candidate, quote, 1, 10) else {
            continue;
        };
        let mut counts = sample
            .iter()
            .filter(|r| !r.is_blank())
            .map(|r| r.fields.len());
        let Some(first) = counts.next() else {
            continue;
        };
        if first > best.1 && counts.all(|n| n == first) {
            best = (candidate, first);
        }
    }
    best.0
}

/// A JSON value as the file wrote it. `serde_json::Value` would read every
/// number through an `f64` or a 64-bit integer, turning
/// `123456789012345678901234567890` into `1.2345678901234568e29` and `1.50`
/// into `1.5`; this keeps a number's own text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    /// In file order; a repeated key keeps its first place and its last value, as `serde_json` does.
    Object(Vec<(String, Json)>),
}

impl<'de> Deserialize<'de> for Json {
    // ponytail: each level re-parses its own raw text, so nesting d deep reads
    // those bytes d times; a hand-rolled parser if deep files ever lag.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <&RawValue>::deserialize(deserializer)?;
        let text = raw.get();
        let parsed = match text.as_bytes().first() {
            Some(b'{') => serde_json::from_str::<Fields>(text).map(|f| Json::Object(f.0)),
            Some(b'[') => serde_json::from_str(text).map(Json::Array),
            Some(b'"') => serde_json::from_str(text).map(Json::String),
            Some(b't') => Ok(Json::Bool(true)),
            Some(b'f') => Ok(Json::Bool(false)),
            Some(b'n') => Ok(Json::Null),
            _ => Ok(Json::Number(text.to_owned())),
        };
        parsed.map_err(D::Error::custom)
    }
}

struct Fields(Vec<(String, Json)>);

impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Fields;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
                let mut fields: Vec<(String, Json)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Json>()? {
                    match fields.iter_mut().find(|(k, _)| *k == key) {
                        Some(field) => field.1 = value,
                        None => fields.push((key, value)),
                    }
                }
                Ok(Fields(fields))
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

/// Compact JSON, numbers as the file spelled them.
impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let string = |f: &mut fmt::Formatter, s: &str| {
            f.write_str(&serde_json::to_string(s).map_err(|_| fmt::Error)?)
        };
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => f.write_str(n),
            Self::String(s) => string(f, s),
            Self::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Self::Object(fields) => {
                f.write_str("{")?;
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cell {
    Text { text: String, quoted: bool },
    Json(Json),
    Missing,
}

pub(crate) struct FileRow {
    pub(crate) line: usize,
    pub(crate) cells: Vec<Cell>,
}

pub(crate) struct Table {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<FileRow>,
    pub(crate) problems: Vec<Problem>,
    /// The delimiter actually used, for "Auto (semicolon)". None for JSON.
    pub(crate) delimiter: Option<char>,
}

// ponytail: whole file in memory; stream rows if multi-GB imports matter
pub(crate) fn read(bytes: &[u8], options: &Options) -> Result<Table, String> {
    let text = decode(bytes, options.encoding)?;
    match options.format {
        Format::Json => read_json(&text),
        Format::Csv | Format::Tsv => read_delimited(&text, options),
    }
}

/// The text after `count` physical lines, counting a `\r\n` pair once.
fn after_lines(text: &str, count: usize) -> &str {
    let bytes = text.as_bytes();
    let mut at = 0;
    for _ in 0..count {
        while at < bytes.len() && bytes[at] != b'\n' && bytes[at] != b'\r' {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        at += if bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
    }
    &text[at..]
}

fn plural(count: usize) -> String {
    format!("{count} field{}", if count == 1 { "" } else { "s" })
}

fn read_delimited(text: &str, options: &Options) -> Result<Table, String> {
    let body = after_lines(text, options.skip_lines);
    let delimiter = match (options.format, options.delimiter) {
        (Format::Tsv, _) => '\t',
        (_, Delimiter::Auto) => sniff(body, options.quote),
        (_, Delimiter::Comma) => ',',
        (_, Delimiter::Semicolon) => ';',
        (_, Delimiter::Tab) => '\t',
        (_, Delimiter::Pipe) => '|',
        (_, Delimiter::Custom(c)) => c,
    };
    let mut records = records(body, delimiter, options.quote, options.skip_lines + 1)?
        .into_iter()
        .peekable();
    let Some(first) = records.peek() else {
        return Err("The file is empty.".into());
    };
    let width = first.fields.len();
    let columns = if options.header {
        let header = records.next().unwrap();
        header
            .fields
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                if f.text.is_empty() {
                    format!("Column {}", i + 1)
                } else {
                    f.text
                }
            })
            .collect()
    } else {
        (1..=width).map(|i| format!("Column {i}")).collect()
    };
    let anchor = if options.header {
        "header"
    } else {
        "first row"
    };

    let mut table = Table {
        columns,
        rows: Vec::new(),
        problems: Vec::new(),
        delimiter: Some(delimiter),
    };
    for record in records {
        // In a one-column file a blank line, even the last, is a record: it is
        // how export writes a NULL there, so a trailing NULL must round-trip.
        if width > 1 && record.is_blank() {
            continue;
        }
        let found = record.fields.len();
        if found != width {
            table.problems.push(Problem {
                row: Some(table.rows.len()),
                line: Some(record.line),
                column: None,
                message: format!(
                    "Line {} has {} where the {anchor} has {width}.",
                    record.line,
                    plural(found)
                ),
            });
        }
        let mut cells: Vec<Cell> = record
            .fields
            .into_iter()
            .map(|f| Cell::Text {
                text: f.text,
                quoted: f.quoted,
            })
            .collect();
        cells.resize(width, Cell::Missing);
        table.rows.push(FileRow {
            line: record.line,
            cells,
        });
    }
    Ok(table)
}

fn read_json(text: &str) -> Result<Table, String> {
    let bad =
        |line: usize, column: usize| format!("Line {line}, column {column} is not valid JSON.");
    let records: Vec<(usize, Json)> = match text.trim_start().chars().next() {
        Some('[') => {
            let values: Vec<Json> =
                serde_json::from_str(text).map_err(|e| bad(e.line(), e.column()))?;
            let lines = element_lines(text);
            values
                .into_iter()
                .enumerate()
                .map(|(i, v)| (lines.get(i).copied().unwrap_or(1), v))
                .collect()
        }
        Some('{') => {
            let mut out = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let value = serde_json::from_str(line).map_err(|e| bad(i + 1, e.column()))?;
                out.push((i + 1, value));
            }
            out
        }
        _ => return Err("The file is not a JSON array or JSON Lines.".into()),
    };

    let mut columns: Vec<String> = Vec::new();
    for (_, value) in &records {
        if let Json::Object(fields) = value {
            for (key, _) in fields {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }

    let mut table = Table {
        rows: Vec::new(),
        problems: Vec::new(),
        delimiter: None,
        columns,
    };
    for (n, (line, value)) in records.into_iter().enumerate() {
        let cells = match value {
            Json::Object(fields) => {
                let mut fields: HashMap<String, Json> = fields.into_iter().collect();
                table
                    .columns
                    .iter()
                    .map(|c| fields.remove(c).map_or(Cell::Missing, Cell::Json))
                    .collect()
            }
            _ => {
                table.problems.push(Problem {
                    row: Some(table.rows.len()),
                    line: Some(line),
                    column: None,
                    message: format!("Record {} is not an object.", n + 1),
                });
                vec![Cell::Missing; table.columns.len()]
            }
        };
        table.rows.push(FileRow { line, cells });
    }
    Ok(table)
}

/// The line each top-level element of a JSON array begins on, skipping over
/// strings so a bracket or comma inside one is not read as structure.
fn element_lines(text: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let (mut line, mut depth) = (1, 0);
    let (mut in_string, mut escaped, mut in_element) = (false, false, false);
    for c in text.chars() {
        if c == '\n' {
            line += 1;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        let begins = depth == 1 && !in_element && !c.is_whitespace() && c != ',' && c != ']';
        if begins {
            lines.push(line);
            in_element = true;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                if depth == 1 {
                    in_element = false;
                }
            }
            ',' if depth == 1 => in_element = false,
            _ => {}
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::*;
    use super::*;
    use crate::db::Column;

    fn csv(text: &str) -> Table {
        csv_bytes(text.as_bytes())
    }

    fn csv_bytes(bytes: &[u8]) -> Table {
        read(bytes, &Options::detected(Path::new("f.csv"), bytes)).unwrap()
    }

    fn text(t: &str) -> Cell {
        Cell::Text {
            text: t.into(),
            quoted: false,
        }
    }

    fn pairs(fields: &[Field]) -> Vec<(&str, bool)> {
        fields.iter().map(|f| (f.text.as_str(), f.quoted)).collect()
    }

    #[test]
    fn a_quoted_field_keeps_its_delimiters_newlines_and_doubled_quotes() {
        let r = records("a,\"b,c\",\"d\ne\",\"f\"\"g\"\r\n", ',', '"', 1).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(
            pairs(&r[0].fields),
            [("a", false), ("b,c", true), ("d\ne", true), ("f\"g", true)]
        );
    }

    #[test]
    fn crlf_lf_and_a_lone_cr_each_end_a_record_and_lines_are_physical() {
        let lines = |t| {
            records(t, ',', '"', 1)
                .unwrap()
                .iter()
                .map(|r| r.line)
                .collect::<Vec<_>>()
        };
        assert_eq!(lines("a\r\nb\nc\rd"), [1, 2, 3, 4]);
        assert_eq!(lines("\"x\ny\",1\nz,2"), [1, 3]);
    }

    #[test]
    fn an_empty_field_and_a_quoted_empty_field_are_told_apart() {
        let r = records(",\"\"\n", ',', '"', 1).unwrap();
        assert_eq!(pairs(&r[0].fields), [("", false), ("", true)]);
    }

    #[test]
    fn a_quote_never_closed_or_text_after_a_closing_quote_says_which_line() {
        assert_eq!(
            records("a\n\"b,c\nd", ',', '"', 1).err().unwrap(),
            "A quoted field opened on line 2 is never closed."
        );
        assert_eq!(
            records("x\n\"ab\"c,d", ',', '"', 1).err().unwrap(),
            "Line 2 has text after a closing quote."
        );
    }

    #[test]
    fn the_sniffer_picks_the_delimiter_every_line_agrees_on() {
        assert_eq!(sniff("a;b;c\n1;2;3\n4;5;6\n", '"'), ';');
        assert_eq!(sniff("a\tb\n1\t2\n", '"'), '\t');
        assert_eq!(sniff("a|b\n1|2\n", '"'), '|');
        assert_eq!(sniff("\"x,y\";z\n1;2\n", '"'), ';');
        assert_eq!(sniff("only\none\n", '"'), ',');
    }

    #[test]
    fn skip_lines_drops_a_preamble_and_lines_still_count_from_the_file() {
        let text = "Exported by X\n\nid,note\n1,\"two\nlines\"\n2,b\n";
        let t = read(
            text.as_bytes(),
            &Options {
                skip_lines: 2,
                ..Options::detected(Path::new("f.csv"), text.as_bytes())
            },
        )
        .unwrap();
        assert_eq!(t.columns, ["id", "note"]);
        assert_eq!(t.rows.iter().map(|r| r.line).collect::<Vec<_>>(), [4, 6]);
    }

    #[test]
    fn without_a_header_columns_are_numbered() {
        let text = "1,ann\n2,bo\n";
        let t = read(
            text.as_bytes(),
            &Options {
                header: false,
                ..Options::detected(Path::new("f.csv"), text.as_bytes())
            },
        )
        .unwrap();
        assert_eq!(t.columns, ["Column 1", "Column 2"]);
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn an_empty_header_name_is_numbered() {
        assert_eq!(csv("id,,name\n1,2,3\n").columns, ["id", "Column 2", "name"]);
    }

    #[test]
    fn a_ragged_row_is_a_problem_on_its_line_not_a_crash() {
        let t = csv("id,name\n1,ann\n2\n3,bo,extra\n");
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[1].cells[1], Cell::Missing);
        assert_eq!(
            t.problems
                .iter()
                .map(|p| (p.row, p.line, p.message.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    Some(1),
                    Some(3),
                    "Line 3 has 1 field where the header has 2."
                ),
                (
                    Some(2),
                    Some(4),
                    "Line 4 has 3 fields where the header has 2."
                ),
            ]
        );
    }

    #[test]
    fn a_blank_line_is_a_row_only_in_a_one_column_file() {
        assert_eq!(csv("a,b\n1,2\n\n3,4\n").rows.len(), 2);
        let one = csv("a\n1\n\n3\n");
        assert_eq!(one.rows.len(), 3);
        assert_eq!(
            one.rows[1].cells,
            [Cell::Text {
                text: String::new(),
                quoted: false
            }]
        );
        assert_eq!(csv("a\n1\n").rows.len(), 1);
        let trailing = csv("a\n1\n\n");
        assert_eq!(trailing.rows.len(), 2);
        assert_eq!(trailing.rows[1].cells, one.rows[1].cells);
        assert_eq!(csv("a,b\n1,2\n\n").rows.len(), 1);
        assert_eq!(csv("a,b\r\n1,2\r\n\r\n\r\n").rows.len(), 1);
    }

    #[test]
    fn utf_16_without_a_bom_is_sniffed_and_a_nul_is_refused() {
        let le: Vec<u8> = "id\n1\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let be: Vec<u8> = "id\n1\n"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(Encoding::sniff(&le), Encoding::Utf16Le);
        assert_eq!(Encoding::sniff(&be), Encoding::Utf16Be);
        assert_eq!(csv_bytes(&le).rows[0].cells, [text("1")]);
        assert_eq!(
            decode(&le, Encoding::Utf8).err().unwrap(),
            "The file holds a NUL character at byte 1."
        );
        assert_eq!(
            decode(b"\xEF\xBB\xBFa\0", Encoding::Utf8).err().unwrap(),
            "The file holds a NUL character at byte 4."
        );
        assert_eq!(
            decode(&[0xFF, 0xFE, b'a', 0, 0, 0], Encoding::Utf16Le)
                .err()
                .unwrap(),
            "The file holds a NUL character at byte 4."
        );
    }

    #[test]
    fn an_exported_file_reads_back_null_and_empty_string_apart() {
        let columns = [
            Column {
                name: "id".into(),
                data_type: None,
            },
            Column {
                name: "note".into(),
                data_type: None,
            },
        ];
        let rows = crate::db::rows(vec![
            vec![Some("1".into()), None],
            vec![Some("2".into()), Some(String::new())],
            vec![Some("3".into()), Some("a,\"b\"\nc".into())],
        ]);
        let t = csv(&crate::export::render_delimited(',', &columns, &rows));
        assert_eq!(
            t.rows
                .iter()
                .map(|r| r.cells[1].clone())
                .collect::<Vec<_>>(),
            [
                Cell::Text {
                    text: String::new(),
                    quoted: false
                },
                Cell::Text {
                    text: String::new(),
                    quoted: true
                },
                Cell::Text {
                    text: "a,\"b\"\nc".into(),
                    quoted: true
                },
            ]
        );
    }

    #[test]
    fn json_array_and_json_lines_read_the_same_rows() {
        let json = |name: &str, text: &str| {
            read(
                text.as_bytes(),
                &Options::detected(Path::new(name), text.as_bytes()),
            )
            .unwrap()
        };
        let array = json(
            "f.json",
            "[\n  {\"a\": 1, \"b\": \"x\"},\n  {\"b\": null, \"c\": {\"d\": [1]}}\n]",
        );
        let lines = json(
            "f.jsonl",
            "{\"a\":1,\"b\":\"x\"}\n{\"b\":null,\"c\":{\"d\":[1]}}\n",
        );
        for t in [&array, &lines] {
            assert_eq!(t.columns, ["a", "b", "c"]);
            assert_eq!(
                t.rows[1].cells,
                [
                    Cell::Missing,
                    Cell::Json(Json::Null),
                    Cell::Json(Json::Object(vec![(
                        "d".into(),
                        Json::Array(vec![Json::Number("1".into())])
                    )]))
                ]
            );
        }
        assert_eq!(
            array.rows.iter().map(|r| r.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(
            lines.rows.iter().map(|r| r.line).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn a_json_number_keeps_the_text_the_file_wrote() {
        let text = r#"[{"a": 123456789012345678901234567890, "b": 0.12345678901234567890123, "c": 1.50, "d": {"e": [1.50, "x\"y"], "e": 2}, "f": 1e400}]"#;
        let t = read(
            text.as_bytes(),
            &Options::detected(Path::new("f.json"), text.as_bytes()),
        )
        .unwrap();
        let shown: Vec<String> = t.rows[0]
            .cells
            .iter()
            .map(|c| match c {
                Cell::Json(json) => json.to_string(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "123456789012345678901234567890",
                "0.12345678901234567890123",
                "1.50",
                "{\"e\":2}",
                "1e400"
            ]
        );
        assert_eq!(
            serde_json::from_str::<Json>(r#"{"k": [1.50, "x\"y", true, null]}"#)
                .unwrap()
                .to_string(),
            r#"{"k":[1.50,"x\"y",true,null]}"#
        );
    }

    #[test]
    fn a_record_that_is_not_an_object_is_a_problem_and_bad_json_says_where() {
        let t = read(
            b"[{\"a\":1}, 2]",
            &Options::detected(Path::new("f.json"), b"["),
        )
        .unwrap();
        assert_eq!(t.problems[0].message, "Record 2 is not an object.");
        assert_eq!(
            read(
                b"{\"a\":1}\n{\"a\":}\n",
                &Options::detected(Path::new("f.jsonl"), b"{")
            )
            .err()
            .unwrap(),
            "Line 2, column 6 is not valid JSON."
        );
    }

    #[test]
    fn every_encoding_decodes_and_a_bom_is_stripped() {
        assert_eq!(decode(b"\xEF\xBB\xBFa", Encoding::Utf8).unwrap(), "a");
        assert_eq!(
            decode(&[0xFF, 0xFE, b'a', 0, b'b', 0], Encoding::Utf16Le).unwrap(),
            "ab"
        );
        assert_eq!(
            decode(&[0xFE, 0xFF, 0, b'a'], Encoding::Utf16Be).unwrap(),
            "a"
        );
        assert_eq!(
            decode(&[0x63, 0x61, 0x66, 0xE9], Encoding::Windows1252).unwrap(),
            "café"
        );
        assert_eq!(Encoding::sniff(&[0xFF, 0xFE, b'a', 0]), Encoding::Utf16Le);
        assert_eq!(Encoding::sniff(&[0xFE, 0xFF, 0, b'a']), Encoding::Utf16Be);
        assert_eq!(Encoding::sniff(b"abc"), Encoding::Utf8);
    }

    #[test]
    fn an_invalid_byte_names_its_offset_never_a_replacement_character() {
        assert_eq!(
            decode(b"ab\xFFcd", Encoding::Utf8).err().unwrap(),
            "The file is not valid UTF-8 at byte 2."
        );
        assert_eq!(
            decode(&[b'a', 0, 0x00, 0xD8, b'b', 0], Encoding::Utf16Le)
                .err()
                .unwrap(),
            "The file is not valid UTF-16 LE at byte 2."
        );
    }

    #[test]
    fn the_format_comes_from_the_extension_then_the_first_byte() {
        assert_eq!(detect(Path::new("x.tsv"), b"a,b"), Format::Tsv);
        assert_eq!(detect(Path::new("x.jsonl"), b"{"), Format::Json);
        assert_eq!(detect(Path::new("x.ndjson"), b"{"), Format::Json);
        assert_eq!(detect(Path::new("x.txt"), b"  ["), Format::Json);
        assert_eq!(detect(Path::new("x.dat"), b"\xEF\xBB\xBF{"), Format::Json);
        assert_eq!(detect(Path::new("x.txt"), b"a,b"), Format::Csv);
    }
}

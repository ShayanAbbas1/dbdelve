//! Rendering a `QueryResult` to a file format for "Export Results".
//!
//! The format is decided once, from the file extension the user picked in the
//! save dialog (`Format::for_path`), and every caller routes through it rather
//! than guessing again — a `.json` and a `.csv` button are two menu items, not
//! two code paths.

use std::{collections::HashSet, path::Path};

use crate::db::{Cell, Column, QueryResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    /// CSV's quoting with a tab between fields: what a spreadsheet splits into
    /// cells on paste, which is why the clipboard copies default to it.
    Tsv,
    Json,
}

impl Format {
    pub fn for_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => Format::Json,
            Some(ext) if ext.eq_ignore_ascii_case("tsv") => Format::Tsv,
            _ => Format::Csv,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
        }
    }
}

pub fn render(format: Format, result: &QueryResult) -> String {
    render_rows(format, &result.columns, &result.rows)
}

/// `render` over some of a result's rows, under all of its columns: a copied
/// row carries the header an exported file would.
pub fn render_rows(format: Format, columns: &[Column], rows: &[Vec<Cell>]) -> String {
    match format {
        Format::Csv => render_delimited(',', columns, rows),
        Format::Tsv => render_delimited('\t', columns, rows),
        Format::Json => render_json(columns, rows),
    }
}

fn render_delimited(delimiter: char, columns: &[Column], rows: &[Vec<Cell>]) -> String {
    let mut out = String::new();
    let separator = delimiter.to_string();

    let header = columns
        .iter()
        .map(|column| csv_field(delimiter, &column.name))
        .collect::<Vec<_>>()
        .join(&separator);
    out.push_str(&header);
    out.push('\n');

    for row in rows {
        let record = row
            .iter()
            .map(|cell| match cell {
                // Postgres's own COPY CSV convention: NULL is nothing at all,
                // an empty string is a quoted empty field. It is the only way
                // the format can tell the two apart on the way back in, so an
                // empty string is forced into quotes even though the general
                // quoting rule below would otherwise leave it bare.
                None => String::new(),
                Some(value) if value.is_empty() => "\"\"".to_string(),
                Some(value) => csv_field(delimiter, value),
            })
            .collect::<Vec<_>>()
            .join(&separator);
        out.push_str(&record);
        out.push('\n');
    }

    out
}

fn csv_field(delimiter: char, value: &str) -> String {
    if value.contains(['"', delimiter, '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Key order follows column order, which `serde_json::Map` only preserves under
/// its `preserve_order` feature — something else in the graph turns that on, and
/// we do not declare it. `json_key_order_matches_column_order` is what makes
/// losing it a test failure rather than a silently re-sorted export.
fn render_json(columns: &[Column], rows: &[Vec<Cell>]) -> String {
    let keys = json_keys(columns);

    let rows = rows
        .iter()
        .map(|row| {
            let mut object = serde_json::Map::new();
            for (key, cell) in keys.iter().zip(row) {
                object.insert(key.clone(), serde_json::Value::from(cell.clone()));
            }
            serde_json::Value::Object(object)
        })
        .collect::<Vec<_>>();

    let mut text = serde_json::to_string_pretty(&rows)
        .expect("string keys and values never fail to serialize");
    text.push('\n');
    text
}

/// One JSON key per column, in column order. `SELECT * FROM a JOIN b` routinely
/// repeats a name (two `id` columns); a plain map would let the second silently
/// overwrite the first, so repeats are suffixed `_2`, `_3`, ... until unused.
///
/// Every real name is spoken for before the walk starts, and that is the whole
/// subtlety: minting `id_2` for a repeated `id` while a column genuinely called
/// `id_2` waits further down the list gives two columns keys that describe the
/// other one. Nothing is lost, so nothing is loud — the reader just gets the
/// wrong column. Skipping the taken name costs a set and leaves a gap in the
/// numbering, which is the honest outcome.
fn json_keys(columns: &[Column]) -> Vec<String> {
    let names: HashSet<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut keys = Vec::with_capacity(columns.len());

    for column in columns {
        let mut key = column.name.clone();
        let mut suffix = 1;
        while used.contains(&key) || (suffix > 1 && names.contains(key.as_str())) {
            suffix += 1;
            key = format!("{}_{suffix}", column.name);
        }
        used.insert(key.clone());
        keys.push(key);
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> Column {
        Column {
            name: name.into(),
            data_type: None,
        }
    }

    #[test]
    fn a_comma_a_quote_and_a_newline_each_get_escaped() {
        let result = QueryResult {
            columns: vec![column("a")],
            rows: vec![
                vec![Some("has,comma".into())],
                vec![Some("has\"quote".into())],
                vec![Some("has\nnewline".into())],
            ],
            ..QueryResult::default()
        };

        let text = render(Format::Csv, &result);
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines[1], "\"has,comma\"");
        assert_eq!(lines[2], "\"has\"\"quote\"");
        assert_eq!(lines[3], "\"has");
        assert_eq!(lines[4], "newline\"");
    }

    #[test]
    fn null_is_an_empty_field_but_an_empty_string_is_quoted() {
        let result = QueryResult {
            columns: vec![column("a")],
            rows: vec![vec![None], vec![Some(String::new())]],
            ..QueryResult::default()
        };

        let text = render(Format::Csv, &result);
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "\"\"");
    }

    #[test]
    fn a_column_name_needing_quotes_is_quoted_in_the_header() {
        let result = QueryResult {
            columns: vec![column("a,b")],
            ..QueryResult::default()
        };

        assert_eq!(render(Format::Csv, &result).lines().next(), Some("\"a,b\""));
    }

    #[test]
    fn an_empty_result_set_still_renders_a_header_and_no_panic() {
        let result = QueryResult {
            columns: vec![column("a"), column("b")],
            ..QueryResult::default()
        };

        assert_eq!(render(Format::Csv, &result), "a,b\n");
    }

    #[test]
    fn zero_columns_and_zero_rows_do_not_panic() {
        assert_eq!(render(Format::Csv, &QueryResult::default()), "\n");
    }

    #[test]
    fn json_renders_null_as_null_never_as_the_string_null() {
        let result = QueryResult {
            columns: vec![column("a")],
            rows: vec![vec![None]],
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["a"], serde_json::Value::Null);
        assert!(!text.contains("\"NULL\""));
    }

    #[test]
    fn duplicate_column_names_both_survive_the_json_round_trip() {
        let result = QueryResult {
            columns: vec![column("id"), column("id")],
            rows: vec![vec![Some("7".into()), Some("8".into())]],
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["id"], "7");
        assert_eq!(parsed[0]["id_2"], "8");
    }

    #[test]
    fn a_duplicate_key_skips_over_a_suffix_that_is_already_a_real_column() {
        let result = QueryResult {
            columns: vec![column("id"), column("id_2"), column("id")],
            rows: vec![vec![Some("1".into()), Some("2".into()), Some("3".into())]],
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["id"], "1");
        assert_eq!(parsed[0]["id_2"], "2");
        assert_eq!(parsed[0]["id_3"], "3");
    }

    /// The same collision the other way round, which is the one a left-to-right
    /// walk gets wrong: the real `id_2` has not been reached yet when the
    /// repeated `id` is looking for a name.
    #[test]
    fn a_real_column_keeps_its_name_from_a_duplicate_that_comes_before_it() {
        let result = QueryResult {
            columns: vec![column("id"), column("id"), column("id_2")],
            rows: vec![vec![Some("1".into()), Some("2".into()), Some("3".into())]],
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["id"], "1");
        assert_eq!(parsed[0]["id_3"], "2");
        assert_eq!(parsed[0]["id_2"], "3");
    }

    #[test]
    fn json_key_order_matches_column_order() {
        let result = QueryResult {
            columns: vec![column("z"), column("a"), column("m")],
            rows: vec![vec![Some("1".into()), Some("2".into()), Some("3".into())]],
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let z = text.find("\"z\"").unwrap();
        let a = text.find("\"a\"").unwrap();
        let m = text.find("\"m\"").unwrap();
        assert!(z < a && a < m);
    }

    #[test]
    fn an_empty_result_set_renders_an_empty_json_array() {
        assert_eq!(render(Format::Json, &QueryResult::default()), "[]\n");
    }

    #[test]
    fn tsv_quotes_a_tab_but_leaves_a_comma_bare() {
        let result = QueryResult {
            columns: vec![column("a"), column("b")],
            rows: vec![vec![Some("has\ttab".into()), Some("has,comma".into())]],
            ..QueryResult::default()
        };

        assert_eq!(
            render(Format::Tsv, &result),
            "a\tb\n\"has\ttab\"\thas,comma\n"
        );
    }

    #[test]
    fn for_path_maps_extensions_case_insensitively_and_defaults_to_csv() {
        assert_eq!(Format::for_path(Path::new("out.json")), Format::Json);
        assert_eq!(Format::for_path(Path::new("out.JSON")), Format::Json);
        assert_eq!(Format::for_path(Path::new("out.csv")), Format::Csv);
        assert_eq!(Format::for_path(Path::new("out.TSV")), Format::Tsv);
        assert_eq!(Format::for_path(Path::new("out")), Format::Csv);
    }
}

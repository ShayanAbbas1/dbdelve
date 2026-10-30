//! Rendering a `QueryResult` to a file format for "Export Results".
//!
//! The format is decided once, from the file extension the user picked in the
//! save dialog (`Format::for_path`), and every caller routes through it rather
//! than guessing again — a `.json` and a `.csv` button are two menu items, not
//! two code paths.

use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
};

use serde::Deserialize;

use crate::db::{Column, EditTarget, Engine, MISSING, QueryResult, Row, Syntax};

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

/// How "Copy Rows As" writes the rows it copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum RowsAs {
    Text,
    Csv,
    CsvWithHeader,
    Json,
    InsertSql,
}

impl RowsAs {
    pub const ALL: [RowsAs; 5] = [
        RowsAs::Text,
        RowsAs::Csv,
        RowsAs::CsvWithHeader,
        RowsAs::Json,
        RowsAs::InsertSql,
    ];

    /// Whether the menu offers this shape for rows read from `engine`. An
    /// `INSERT` only means something to a server that reads SQL.
    pub fn offered_on(self, engine: Engine) -> bool {
        match self {
            RowsAs::Text | RowsAs::Csv | RowsAs::CsvWithHeader | RowsAs::Json => true,
            RowsAs::InsertSql => engine.syntax() == Syntax::Sql,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RowsAs::Text => "Text",
            RowsAs::Csv => "CSV",
            RowsAs::CsvWithHeader => "CSV with Header",
            RowsAs::Json => "JSON",
            RowsAs::InsertSql => "INSERT SQL",
        }
    }
}

/// The rows of `result` as clipboard text. `edit` is where the rows came
/// from, for `INSERT` to name; without one it says `table_name` and leaves
/// the naming to whoever pastes it, writing every result column under its own
/// (possibly aliased) name for lack of a mapping back to real ones. Nothing
/// here runs: it is text, and the statement is the user's to run or not.
pub fn render_rows_as(
    kind: RowsAs,
    engine: Engine,
    edit: Option<&EditTarget>,
    result: &QueryResult,
) -> String {
    match kind {
        RowsAs::Text => result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.unwrap_or("NULL"))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        RowsAs::CsvWithHeader => render_delimited(',', &result.columns, &result.rows),
        RowsAs::Csv => {
            let header = render_delimited(',', &result.columns, &[]);
            render_delimited(',', &result.columns, &result.rows)[header.len()..].to_string()
        }
        RowsAs::Json => render(Format::Json, result),
        RowsAs::InsertSql => {
            let target = edit.map_or_else(
                || "table_name".to_string(),
                |edit| engine.qualified(&edit.schema, &edit.table),
            );
            // `edit.columns` is the result column's real source, positionally
            // -- `None` for a computed one (`now() AS t`), which has nowhere
            // to write back to and is dropped rather than named by its alias.
            let sources: Vec<Option<&str>> = match edit {
                Some(edit) => edit.columns.iter().map(|c| c.as_deref()).collect(),
                None => result
                    .columns
                    .iter()
                    .map(|c| Some(c.name.as_str()))
                    .collect(),
            };
            let columns = result
                .columns
                .iter()
                .zip(&sources)
                .filter_map(|(_, source)| source.map(|name| engine.quote_identifier(name)))
                .collect::<Vec<_>>()
                .join(", ");
            result
                .rows
                .iter()
                .map(|row| {
                    let values = row
                        .iter()
                        .zip(&result.columns)
                        .zip(&sources)
                        .filter_map(|((cell, column), source)| {
                            source.map(|_| insert_literal(engine, cell, column))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("INSERT INTO {target} ({columns}) VALUES ({values});")
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// One value of an `INSERT … VALUES` row, as `render_rows_as`'s `InsertSql`
/// writes it: a number bare, a fetched binary value in the engine's own
/// literal (`0xAB`, `x'AB'`) bare, and everything else through
/// [`Engine::quote_value`].
///
/// Binary is handled here rather than in `quote_value` because the two have
/// different callers to answer for: `quote_value` also spells a value typed
/// into a filter bar or an edit, free text that must stay a quoted string
/// unless it is `SqlServer`'s own hex shape (its one unquoted path in).
/// `insert_literal`'s value, by contrast, is never anything but what dbdelve
/// itself rendered (`mssql::render`, `mysql::render`, `sqlite::render`), so a
/// shape check alone is enough.
fn insert_literal(engine: Engine, cell: Option<&str>, column: &Column) -> String {
    let data_type = column.data_type.as_deref();
    match cell {
        None => "NULL".to_string(),
        Some(value)
            if data_type.is_some_and(|data_type| engine.is_numeric_type(data_type))
                && value.parse::<f64>().is_ok_and(f64::is_finite) =>
        {
            value.to_string()
        }
        Some(value)
            if data_type.is_some_and(|data_type| engine.is_binary_type(data_type))
                && is_bare_binary_literal(value) =>
        {
            value.to_string()
        }
        Some(value) => engine.quote_value(value, data_type),
    }
}

/// Whether `value` is already a bare binary literal -- SQL Server and
/// MySQL's `0xAB`, SQLite's `x'AB'` -- and not the text either would be if
/// quoted as a string, which is what running the `INSERT` as text would
/// store.
fn is_bare_binary_literal(value: &str) -> bool {
    value
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
        || (value
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("x'"))
            && value.len() > 2
            && value.ends_with('\''))
}

pub fn render(format: Format, result: &QueryResult) -> String {
    render_rows(format, &result.columns, &result.rows, &result.cell_types)
}

/// `render` over some of a result's rows, under all of its columns: a copied
/// row carries the header an exported file would. `cell_types` is
/// [`QueryResult::cell_types`] for those rows.
pub fn render_rows(
    format: Format,
    columns: &[Column],
    rows: &[Row],
    cell_types: &[Vec<&str>],
) -> String {
    rendered(Writer::new(Vec::new(), format, columns), rows, cell_types)
}

pub(crate) fn render_delimited(delimiter: char, columns: &[Column], rows: &[Row]) -> String {
    let writer = Writer::with_shape(Vec::new(), Shape::Delimited(delimiter), columns);
    rendered(writer, rows, &[])
}

fn rendered(writer: io::Result<Writer<Vec<u8>>>, rows: &[Row], cell_types: &[Vec<&str>]) -> String {
    let written = writer
        .and_then(|mut writer| {
            writer.rows(rows, cell_types)?;
            writer.finish()
        })
        .expect("writing to memory never fails");
    String::from_utf8(written).expect("every piece written is a str")
}

pub(crate) fn csv_field(delimiter: char, value: &str) -> String {
    if value.contains(['"', delimiter, '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// A file written as its rows come: the header when it is made, each batch of
/// rows as it is handed over, and whatever closes the format at `finish`.
/// [`render`] is this over a `Vec`, so the file an export streams to disk and
/// the text a copy puts on the clipboard are one spelling.
///
/// JSON is spelled out by hand in `serde_json`'s pretty layout, one value at a
/// time, rather than built as a `Value` and printed whole: that would hold
/// every row twice over before the first byte left, and it puts each object's
/// keys out in column order with no map between to re-sort them.
/// `the_json_writer_matches_serde_jsons_own_pretty_print` is what holds the
/// two spellings together.
pub struct Writer<W: Write> {
    out: W,
    shape: Shape,
    written: usize,
}

enum Shape {
    Delimited(char),
    /// Each column's key, already spelled as a JSON string.
    Json(Vec<String>),
}

impl Shape {
    fn json(columns: &[Column]) -> Self {
        Shape::Json(
            json_keys(columns)
                .iter()
                .map(|key| serde_json::to_string(key).expect("a string always serializes"))
                .collect(),
        )
    }
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, format: Format, columns: &[Column]) -> io::Result<Self> {
        let shape = match format {
            Format::Csv => Shape::Delimited(','),
            Format::Tsv => Shape::Delimited('\t'),
            Format::Json => Shape::json(columns),
        };
        Self::with_shape(out, shape, columns)
    }

    fn with_shape(mut out: W, shape: Shape, columns: &[Column]) -> io::Result<Self> {
        if let Shape::Delimited(delimiter) = shape {
            for (index, column) in columns.iter().enumerate() {
                if index > 0 {
                    write_char(&mut out, delimiter)?;
                }
                write_field(&mut out, delimiter, &column.name)?;
            }
            out.write_all(b"\n")?;
        }
        Ok(Self {
            out,
            shape,
            written: 0,
        })
    }

    /// `cell_types` is [`QueryResult::cell_types`] for these rows, and empty
    /// where every column is typed whole.
    pub fn rows(&mut self, rows: &[Row], cell_types: &[Vec<&str>]) -> io::Result<()> {
        let out = &mut self.out;
        match &self.shape {
            Shape::Delimited(delimiter) => {
                for row in rows {
                    for (index, cell) in row.iter().enumerate() {
                        if index > 0 {
                            write_char(out, *delimiter)?;
                        }
                        match cell {
                            // Postgres's own COPY CSV convention: NULL is
                            // nothing at all, an empty string is a quoted empty
                            // field. It is the only way the format can tell the
                            // two apart on the way back in, so an empty string
                            // is forced into quotes even though the general
                            // quoting rule would otherwise leave it bare.
                            None => {}
                            Some("") => out.write_all(b"\"\"")?,
                            Some(value) => write_field(out, *delimiter, value)?,
                        }
                    }
                    out.write_all(b"\n")?;
                }
            }
            // A document's missing field is left out of its object rather
            // than written as the `null` it does not hold.
            Shape::Json(keys) => {
                for (row_ix, row) in rows.iter().enumerate() {
                    out.write_all(match self.written {
                        0 => b"[\n  ",
                        _ => b",\n  ",
                    })?;
                    let types = cell_types.get(row_ix);
                    let mut fields = 0;
                    for (col_ix, (key, cell)) in keys.iter().zip(row.iter()).enumerate() {
                        if types.and_then(|types| types.get(col_ix)) == Some(&MISSING) {
                            continue;
                        }
                        out.write_all(match fields {
                            0 => b"{\n    ",
                            _ => b",\n    ",
                        })?;
                        out.write_all(key.as_bytes())?;
                        out.write_all(b": ")?;
                        match cell {
                            None => out.write_all(b"null")?,
                            Some(value) => serde_json::to_writer(&mut *out, value)?,
                        }
                        fields += 1;
                    }
                    out.write_all(match fields {
                        0 => b"{}",
                        _ => b"\n  }",
                    })?;
                    self.written += 1;
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        if let Shape::Json(_) = self.shape {
            self.out.write_all(match self.written {
                0 => b"[]\n",
                _ => b"\n]\n",
            })?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

fn write_char(out: &mut impl Write, value: char) -> io::Result<()> {
    out.write_all(value.encode_utf8(&mut [0; 4]).as_bytes())
}

/// [`csv_field`], written out rather than built.
fn write_field(out: &mut impl Write, delimiter: char, value: &str) -> io::Result<()> {
    if !value.contains(['"', delimiter, '\n', '\r']) {
        return out.write_all(value.as_bytes());
    }
    out.write_all(b"\"")?;
    for (index, part) in value.split('"').enumerate() {
        if index > 0 {
            out.write_all(b"\"\"")?;
        }
        out.write_all(part.as_bytes())?;
    }
    out.write_all(b"\"")
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
    use crate::db::rows;

    fn column(name: &str) -> Column {
        Column {
            name: name.into(),
            data_type: None,
        }
    }

    fn edit_target(columns: &[&str]) -> EditTarget {
        EditTarget {
            schema: "public".into(),
            table: "notes".into(),
            columns: columns.iter().map(|name| Some(name.to_string())).collect(),
            keys: vec![0],
            ..Default::default()
        }
    }

    #[test]
    fn a_row_copies_in_each_of_its_shapes() {
        let result = QueryResult {
            columns: vec![
                Column {
                    name: "id".into(),
                    data_type: Some("int4".into()),
                },
                Column {
                    name: "note".into(),
                    data_type: Some("text".into()),
                },
            ],
            rows: rows(vec![
                vec![Some("7".into()), Some("it's".into())],
                vec![Some("8".into()), None],
            ]),
            ..QueryResult::default()
        };
        let edit = edit_target(&["id", "note"]);
        let as_ = |kind| render_rows_as(kind, Engine::Postgres, Some(&edit), &result);

        assert_eq!(as_(RowsAs::Text), "7\tit's\n8\tNULL");
        assert_eq!(as_(RowsAs::Csv), "7,it's\n8,\n");
        assert_eq!(as_(RowsAs::CsvWithHeader), "id,note\n7,it's\n8,\n");
        assert_eq!(
            as_(RowsAs::InsertSql),
            "INSERT INTO \"public\".\"notes\" (\"id\", \"note\") VALUES (7, 'it''s');\n\
             INSERT INTO \"public\".\"notes\" (\"id\", \"note\") VALUES (8, NULL);"
        );
        assert!(
            render_rows_as(RowsAs::InsertSql, Engine::Postgres, None, &result)
                .contains("INTO table_name ")
        );
    }

    #[test]
    fn insert_sql_names_each_columns_real_source_and_drops_a_computed_one() {
        // `SELECT id AS ident, now() AS t FROM accounts` -- `ident` has a real
        // column behind it, `t` does not, and the statement must write into
        // the first under its own name and leave the second out entirely
        // rather than into a column literally called "t".
        let result = QueryResult {
            columns: vec![
                Column {
                    name: "ident".into(),
                    data_type: Some("int4".into()),
                },
                column("t"),
            ],
            rows: rows(vec![vec![Some("7".into()), Some("2024-01-01".into())]]),
            ..QueryResult::default()
        };
        let edit = EditTarget {
            schema: "public".into(),
            table: "accounts".into(),
            columns: vec![Some("id".into()), None],
            keys: vec![0],
            ..Default::default()
        };

        assert_eq!(
            render_rows_as(RowsAs::InsertSql, Engine::Postgres, Some(&edit), &result),
            "INSERT INTO \"public\".\"accounts\" (\"id\") VALUES (7);"
        );
    }

    #[test]
    fn insert_sql_writes_a_fetched_binary_value_as_its_engines_own_literal() {
        // MySQL renders a fetched blob as `0xABCD` and SQLite as `x'ABCD'`
        // (see `mysql::render`, `sqlite::render`); quoting either back as a
        // string is what makes the statement store ASCII text instead of
        // bytes. SQL Server's `0x…` already round-trips through `quote_value`.
        let result = QueryResult {
            columns: vec![Column {
                name: "data".into(),
                data_type: Some("blob".into()),
            }],
            rows: rows(vec![vec![Some("0xABCD".into())]]),
            ..QueryResult::default()
        };
        let edit = edit_target(&["data"]);

        assert_eq!(
            render_rows_as(RowsAs::InsertSql, Engine::MySql, Some(&edit), &result),
            "INSERT INTO `public`.`notes` (`data`) VALUES (0xABCD);"
        );

        let result = QueryResult {
            columns: vec![Column {
                name: "data".into(),
                data_type: Some("blob".into()),
            }],
            rows: rows(vec![vec![Some("x'ABCD'".into())]]),
            ..QueryResult::default()
        };
        assert_eq!(
            render_rows_as(RowsAs::InsertSql, Engine::Sqlite, Some(&edit), &result),
            "INSERT INTO \"public\".\"notes\" (\"data\") VALUES (x'ABCD');"
        );
    }

    #[test]
    fn a_comma_a_quote_and_a_newline_each_get_escaped() {
        let result = QueryResult {
            columns: vec![column("a")],
            rows: rows(vec![
                vec![Some("has,comma".into())],
                vec![Some("has\"quote".into())],
                vec![Some("has\nnewline".into())],
            ]),
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
            rows: rows(vec![vec![None], vec![Some(String::new())]]),
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
            rows: rows(vec![vec![None]]),
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["a"], serde_json::Value::Null);
        assert!(!text.contains("\"NULL\""));
    }

    #[test]
    fn a_missing_field_is_left_out_of_the_json_and_a_null_is_kept() {
        let result = QueryResult {
            columns: vec![column("email"), column("phone")],
            rows: rows(vec![vec![None, None]]),
            cell_types: vec![vec![MISSING, "null"]],
            ..QueryResult::default()
        };
        let expected = serde_json::json!([{ "phone": null }]);
        for text in [
            render(Format::Json, &result),
            render_rows_as(RowsAs::Json, Engine::MongoDb, None, &result),
        ] {
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed, expected, "{text}");
        }
    }

    #[test]
    fn insert_sql_is_offered_only_where_the_server_reads_sql() {
        assert!(RowsAs::InsertSql.offered_on(Engine::Postgres));
        assert!(!RowsAs::InsertSql.offered_on(Engine::MongoDb));
        assert!(RowsAs::Json.offered_on(Engine::MongoDb));
    }

    #[test]
    fn duplicate_column_names_both_survive_the_json_round_trip() {
        let result = QueryResult {
            columns: vec![column("id"), column("id")],
            rows: rows(vec![vec![Some("7".into()), Some("8".into())]]),
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
            rows: rows(vec![vec![
                Some("1".into()),
                Some("2".into()),
                Some("3".into()),
            ]]),
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
            rows: rows(vec![vec![
                Some("1".into()),
                Some("2".into()),
                Some("3".into()),
            ]]),
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
            rows: rows(vec![vec![
                Some("1".into()),
                Some("2".into()),
                Some("3".into()),
            ]]),
            ..QueryResult::default()
        };

        let text = render(Format::Json, &result);
        let z = text.find("\"z\"").unwrap();
        let a = text.find("\"a\"").unwrap();
        let m = text.find("\"m\"").unwrap();
        assert!(z < a && a < m);
    }

    /// What JSON export wrote when it built a `Value` and pretty-printed it
    /// whole, kept as the reference the streamed spelling has to match.
    fn pretty_printed(columns: &[Column], rows: &[Row], cell_types: &[Vec<&str>]) -> String {
        let keys = json_keys(columns);
        let rows = rows
            .iter()
            .enumerate()
            .map(|(row_ix, row)| {
                let types = cell_types.get(row_ix);
                let mut object = serde_json::Map::new();
                for (col_ix, (key, cell)) in keys.iter().zip(row.iter()).enumerate() {
                    if types.and_then(|types| types.get(col_ix)) == Some(&MISSING) {
                        continue;
                    }
                    object.insert(key.clone(), serde_json::Value::from(cell));
                }
                serde_json::Value::Object(object)
            })
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&rows).unwrap() + "\n"
    }

    #[test]
    fn the_json_writer_matches_serde_jsons_own_pretty_print() {
        let cases = vec![
            (Vec::new(), Vec::new(), Vec::new()),
            (vec![column("a")], Vec::new(), Vec::new()),
            (Vec::new(), rows(vec![vec![], vec![]]), Vec::new()),
            (
                vec![
                    column("id"),
                    column("id"),
                    column("id_2"),
                    column("q\"uote"),
                ],
                rows(vec![
                    vec![
                        Some("1".into()),
                        None,
                        Some("tab\tline\nend\u{1}".into()),
                        Some("\\ \"é\" 🙂 </script>".into()),
                    ],
                    vec![Some(String::new()), Some("2".into()), None, None],
                ]),
                Vec::new(),
            ),
            (
                vec![column("email"), column("phone")],
                rows(vec![
                    vec![None, None],
                    vec![Some("a@b".into()), None],
                    vec![None, Some("5".into())],
                ]),
                vec![
                    vec![MISSING, MISSING],
                    vec!["string", "null"],
                    vec![MISSING, "int"],
                ],
            ),
        ];
        for (columns, rows, cell_types) in cases {
            let expected = pretty_printed(&columns, &rows, &cell_types);
            assert_eq!(
                render_rows(Format::Json, &columns, &rows, &cell_types),
                expected
            );
            // Handed over one row at a time, as an export streams them.
            let mut writer = Writer::new(Vec::new(), Format::Json, &columns).unwrap();
            for (row_ix, row) in rows.iter().enumerate() {
                writer
                    .rows(
                        std::slice::from_ref(row),
                        cell_types.get(row_ix..=row_ix).unwrap_or_default(),
                    )
                    .unwrap();
            }
            assert_eq!(
                String::from_utf8(writer.finish().unwrap()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn csv_written_in_batches_is_the_csv_rendered_at_once() {
        let columns = vec![column("a,b"), column("c")];
        let rows = rows(vec![
            vec![Some("x\"y".into()), None],
            vec![Some(String::new()), Some("line\r\nbreak".into())],
        ]);
        let mut writer = Writer::new(Vec::new(), Format::Csv, &columns).unwrap();
        writer.rows(&rows[..1], &[]).unwrap();
        writer.rows(&rows[1..], &[]).unwrap();
        assert_eq!(
            String::from_utf8(writer.finish().unwrap()).unwrap(),
            "\"a,b\",c\n\"x\"\"y\",\n\"\",\"line\r\nbreak\"\n"
        );
    }

    #[test]
    fn an_empty_result_set_renders_an_empty_json_array() {
        assert_eq!(render(Format::Json, &QueryResult::default()), "[]\n");
    }

    #[test]
    fn tsv_quotes_a_tab_but_leaves_a_comma_bare() {
        let result = QueryResult {
            columns: vec![column("a"), column("b")],
            rows: rows(vec![vec![
                Some("has\ttab".into()),
                Some("has,comma".into()),
            ]]),
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

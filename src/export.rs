//! Rendering a `QueryResult` to a file format for "Export Results".
//!
//! The format is decided once, from the file extension the user picked in the
//! save dialog (`FileAs::for_path`), and every caller routes through it rather
//! than guessing again — a `.json` and a `.csv` button are two menu items, not
//! two code paths.

use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
};

use serde::Deserialize;

use crate::db::{
    Column, ColumnDefinition, EditTarget, Engine, IdentityInsert, MISSING, QueryResult, Row, Syntax,
};

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

/// What an export's file holds. Apart from `Format` because `INSERT`s need
/// an engine and a table to name, which the clipboard's formats never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAs {
    Rows(Format),
    Inserts,
}

impl FileAs {
    /// `.sql` on an engine that does not read SQL falls back as any other
    /// unknown extension does, and the notice says what was written.
    pub fn for_path(path: &Path, engine: Engine) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("sql") && engine.syntax() == Syntax::Sql => {
                FileAs::Inserts
            }
            _ => FileAs::Rows(Format::for_path(path)),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            FileAs::Rows(format) => format.extension(),
            FileAs::Inserts => "sql",
        }
    }

    pub fn writer<W: Write>(
        self,
        out: W,
        engine: Engine,
        target: &InsertTarget,
        columns: &[Column],
    ) -> io::Result<Writer<W>> {
        match self {
            FileAs::Rows(format) => Writer::new(out, format, columns),
            FileAs::Inserts => {
                let inserts = Inserts::new(engine, target, columns);
                Writer::with_shape(out, Shape::Inserts(inserts), columns)
            }
        }
    }
}

/// Where a result's `INSERT`s write. With no relation they say `table_name`
/// and leave the naming to whoever runs them; with no columns they write each
/// result column under its own (possibly aliased) name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InsertTarget {
    /// Schema and table.
    pub relation: Option<(String, String)>,
    /// Each column of the result this was read from, by its own name, and its
    /// real source -- `None` for a computed one (`now() AS t`), which has
    /// nowhere to write back to.
    pub columns: Option<Vec<(String, Option<String>)>>,
    /// The table's columns its server works out for itself, by their real
    /// names: left out wherever the result holds them.
    pub computed: Vec<String>,
    /// The table's columns its server numbers, by their real names.
    pub numbered: Vec<String>,
}

impl InsertTarget {
    /// Where the `columns` of a result read from `edit` write back to.
    pub fn read_from(edit: &EditTarget, columns: &[Column]) -> Self {
        let names = columns.iter().map(|column| column.name.clone());
        Self {
            relation: Some((edit.schema.clone(), edit.table.clone())),
            columns: Some(names.zip(edit.columns.iter().cloned()).collect()),
            ..Self::default()
        }
    }

    /// This target, knowing what the server fills of the table of `columns`.
    pub fn knowing(self, engine: Engine, columns: &[ColumnDefinition]) -> Self {
        let spec = engine.spec();
        let names = |fills: &dyn Fn(&ColumnDefinition) -> bool| {
            let filled = columns.iter().filter(|column| fills(column));
            filled.map(|column| column.name.clone()).collect()
        };
        Self {
            computed: names(&|column| spec.computes(column)),
            numbered: names(&|column| spec.numbers(column)),
            ..self
        }
    }
}

/// The statement shape of one result, spelled once and filled per row.
struct Inserts {
    engine: Engine,
    opening: String,
    /// The result columns that are written, by position.
    written: Vec<(usize, Column)>,
    /// The statements a file holds before its first row and after its last.
    around: Option<(String, String)>,
}

impl Inserts {
    fn new(engine: Engine, target: &InsertTarget, columns: &[Column]) -> Self {
        let into = target.relation.as_ref().map_or_else(
            || "table_name".to_string(),
            |(schema, table)| engine.qualified(schema, table),
        );
        // A statement fetched again may have come back a different shape from
        // the result its target was read from, and a source named for the
        // wrong column would write values under it.
        let sources = target.columns.as_ref().filter(|sources| {
            sources.len() == columns.len()
                && sources
                    .iter()
                    .zip(columns)
                    .all(|((name, _), column)| *name == column.name)
        });
        // SQL Server clears the source of a column no `UPDATE` can set, and an
        // identity is one, so it goes by the name the result gave it. On the
        // others no source means no column of the table.
        let identity_named = |column: &Column| {
            target
                .numbered
                .iter()
                .find(|numbered| numbered.eq_ignore_ascii_case(&column.name))
                .filter(|_| engine.spec().sql.identity_insert == IdentityInsert::SessionSwitch)
                .map(String::as_str)
        };
        let written: Vec<(usize, &str, &Column)> = columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| match sources {
                Some(sources) => sources[index]
                    .1
                    .as_deref()
                    .or_else(|| identity_named(column))
                    .map(|name| (index, name, column)),
                None => Some((index, column.name.as_str(), column)),
            })
            .filter(|(_, name, _)| !target.computed.iter().any(|computed| computed == name))
            .collect();
        let numbered = written
            .iter()
            .any(|(_, name, _)| target.numbered.iter().any(|numbered| numbered == name));
        let names = written
            .iter()
            .map(|(_, name, _)| engine.quote_identifier(name))
            .collect::<Vec<_>>()
            .join(", ");
        let identity_insert = engine.spec().sql.identity_insert;
        let overriding = match (numbered, identity_insert) {
            (true, IdentityInsert::Overriding) => " OVERRIDING SYSTEM VALUE",
            _ => "",
        };
        let around = (numbered && identity_insert == IdentityInsert::SessionSwitch).then(|| {
            (
                format!("SET IDENTITY_INSERT {into} ON;\n"),
                format!("SET IDENTITY_INSERT {into} OFF;\n"),
            )
        });
        Self {
            engine,
            opening: format!("INSERT INTO {into} ({names}){overriding} VALUES ("),
            written: written
                .into_iter()
                .map(|(index, _, column)| (index, column.clone()))
                .collect(),
            around,
        }
    }

    fn statement(&self, row: &Row) -> String {
        let values = self
            .written
            .iter()
            .map(|(index, column)| insert_literal(self.engine, row.get(*index), column))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}{values});", self.opening)
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
/// from, for `INSERT` to name. Nothing here runs: it is text, and the
/// statement is the user's to run or not.
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
            let target = edit
                .map(|edit| InsertTarget::read_from(edit, &result.columns))
                .unwrap_or_default();
            let inserts = Inserts::new(engine, &target, &result.columns);
            result
                .rows
                .iter()
                .map(|row| inserts.statement(row))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// One value of an `INSERT … VALUES` row, as `render_rows_as`'s `InsertSql`
/// writes it: what the engine says of a value its driver rendered
/// ([`Engine::fetched_literal`]), a number bare, a fetched binary value in the
/// engine's own literal (`0xAB`, `x'AB'`) bare, and everything else through
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
    let fetched = cell
        .zip(data_type)
        .and_then(|(value, data_type)| engine.fetched_literal(value, data_type));
    if let Some(literal) = fetched {
        return literal;
    }
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
        // Spelled out in either arm below, since the grammar that cuts a file
        // into statements stops reading at a raw NUL.
        Some(value) if engine.spec().sql.backslash_escapes => {
            engine.quote_value(value, data_type).replace('\0', r"\0")
        }
        Some(value) => engine
            .spec()
            .text_around_nul(value, data_type)
            .unwrap_or_else(|| engine.quote_value(value, data_type)),
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
    Inserts(Inserts),
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
            Shape::Inserts(inserts) => {
                for row in rows {
                    if let (0, Some((before, _))) = (self.written, &inserts.around) {
                        out.write_all(before.as_bytes())?;
                    }
                    out.write_all(inserts.statement(row).as_bytes())?;
                    out.write_all(b"\n")?;
                    self.written += 1;
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        match &self.shape {
            Shape::Json(_) => self.out.write_all(match self.written {
                0 => b"[]\n",
                _ => b"\n]\n",
            })?,
            Shape::Inserts(Inserts {
                around: Some((_, after)),
                ..
            }) if self.written > 0 => self.out.write_all(after.as_bytes())?,
            Shape::Delimited(_) | Shape::Inserts(_) => {}
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
    fn a_literal_says_what_the_engines_driver_fetched() {
        let literal = |engine, value, data_type: &str| {
            let column = Column {
                name: "c".into(),
                data_type: Some(data_type.into()),
            };
            insert_literal(engine, Some(value), &column)
        };

        assert_eq!(literal(Engine::MySql, "0xA5", "bit"), "0xA5");
        assert_eq!(literal(Engine::MariaDb, "0x0001", "geometry"), "0x0001");
        assert_eq!(literal(Engine::MySql, "0x", "varbinary"), "x''");
        assert_eq!(literal(Engine::SqlServer, "0x", "varbinary"), "0x");
        assert_eq!(literal(Engine::MySql, "0xA5", "varchar"), "'0xA5'");
        assert_eq!(literal(Engine::DuckDb, "00ff", "blob"), "unhex('00ff')");
        assert_eq!(literal(Engine::DuckDb, "00ff", "varchar"), "'00ff'");
    }

    #[test]
    fn a_nul_is_escaped_where_the_engine_reads_escapes() {
        let column = Column {
            name: "c".into(),
            data_type: Some("char".into()),
        };
        assert_eq!(
            insert_literal(Engine::MySql, Some("a\0b\\0"), &column),
            r"'a\0b\\0'"
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

    fn inserts_file(target: &InsertTarget, result: &QueryResult) -> String {
        let mut writer = FileAs::Inserts
            .writer(Vec::new(), Engine::Postgres, target, &result.columns)
            .unwrap();
        writer.rows(&result.rows, &[]).unwrap();
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    #[test]
    fn a_file_of_inserts_holds_one_terminated_statement_a_row() {
        let result = QueryResult {
            columns: vec![column("id"), column("note")],
            rows: rows(vec![
                vec![Some("7".into()), Some("it's".into())],
                vec![Some("8".into()), None],
            ]),
            ..QueryResult::default()
        };
        let target = InsertTarget {
            relation: Some(("public".into(), "notes".into())),
            ..InsertTarget::default()
        };
        assert_eq!(
            inserts_file(&target, &result),
            "INSERT INTO \"public\".\"notes\" (\"id\", \"note\") VALUES ('7', 'it''s');\n\
             INSERT INTO \"public\".\"notes\" (\"id\", \"note\") VALUES ('8', NULL);\n"
        );
    }

    #[test]
    fn sources_that_no_longer_line_up_fall_back_to_the_columns_own_names() {
        let result = QueryResult {
            columns: vec![column("a"), column("b")],
            rows: rows(vec![vec![Some("1".into()), Some("2".into())]]),
            ..QueryResult::default()
        };
        let target = InsertTarget {
            columns: Some(vec![("only".into(), Some("only".into()))]),
            ..InsertTarget::default()
        };
        assert_eq!(
            inserts_file(&target, &result),
            "INSERT INTO table_name (\"a\", \"b\") VALUES ('1', '2');\n"
        );
    }

    fn file_of(engine: Engine, target: &InsertTarget, rows_written: &[Row]) -> String {
        let columns = [column("id"), column("note"), column("twice")];
        let mut writer = FileAs::Inserts
            .writer(Vec::new(), engine, target, &columns)
            .unwrap();
        for row in rows_written {
            writer.rows(std::slice::from_ref(row), &[]).unwrap();
        }
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    fn definition(name: &str, data_type: &str, default: &str) -> ColumnDefinition {
        ColumnDefinition {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            default: Some(default.into()),
        }
    }

    fn sources(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        pairs
            .iter()
            .map(|(name, source)| (name.to_string(), source.map(str::to_string)))
            .collect()
    }

    fn numbered_target(engine: Engine, identity: &str, computed: &str) -> InsertTarget {
        InsertTarget {
            relation: Some(("s".into(), "t".into())),
            ..InsertTarget::default()
        }
        .knowing(
            engine,
            &[
                definition("id", "int", identity),
                definition("note", "text", ""),
                definition("twice", "int", computed),
            ],
        )
    }

    #[test]
    fn a_sql_server_file_switches_identity_insert_around_its_rows() {
        let target = numbered_target(Engine::SqlServer, "IDENTITY(1,1)", "AS ([id]*(2))");
        let two = rows(vec![
            vec![Some("3".into()), Some("a".into()), Some("6".into())],
            vec![Some("5".into()), None, Some("10".into())],
        ]);
        assert_eq!(
            file_of(Engine::SqlServer, &target, &two),
            "SET IDENTITY_INSERT \"s\".\"t\" ON;\n\
             INSERT INTO \"s\".\"t\" (\"id\", \"note\") VALUES (N'3', N'a');\n\
             INSERT INTO \"s\".\"t\" (\"id\", \"note\") VALUES (N'5', NULL);\n\
             SET IDENTITY_INSERT \"s\".\"t\" OFF;\n"
        );
        assert_eq!(file_of(Engine::SqlServer, &target, &[]), "");

        let plain = numbered_target(Engine::SqlServer, "", "");
        assert!(!file_of(Engine::SqlServer, &plain, &two).contains("IDENTITY_INSERT"));
    }

    #[test]
    fn a_rowversion_is_left_out_as_a_computed_column_is() {
        let target = InsertTarget::default()
            .knowing(Engine::SqlServer, &[definition("twice", "timestamp", "")]);
        assert_eq!(target.computed, ["twice"]);
        assert!(target.numbered.is_empty());
    }

    #[test]
    fn postgres_overrides_only_an_identity_it_writes_and_that_refuses_values() {
        let row = rows(vec![vec![Some("3".into()), None, Some("6".into())]]);
        let target = numbered_target(
            Engine::Postgres,
            "GENERATED ALWAYS AS IDENTITY",
            "GENERATED ALWAYS AS ((id * 2)) STORED",
        );
        assert_eq!(
            file_of(Engine::Postgres, &target, &row),
            "INSERT INTO \"s\".\"t\" (\"id\", \"note\") OVERRIDING SYSTEM VALUE \
             VALUES ('3', NULL);\n"
        );
        let by_default = numbered_target(Engine::Postgres, "GENERATED BY DEFAULT AS IDENTITY", "");
        assert_eq!(
            file_of(Engine::Postgres, &by_default, &row),
            "INSERT INTO \"s\".\"t\" (\"id\", \"note\", \"twice\") \
             VALUES ('3', NULL, '6');\n"
        );
        let elsewhere = InsertTarget {
            columns: Some(sources(&[
                ("id", None),
                ("note", Some("note")),
                ("twice", Some("twice")),
            ])),
            computed: vec!["twice".into()],
            numbered: vec!["id".into()],
            ..target
        };
        assert_eq!(
            file_of(Engine::Postgres, &elsewhere, &row),
            "INSERT INTO \"s\".\"t\" (\"note\") VALUES (NULL);\n"
        );
    }

    #[test]
    fn an_auto_increment_keeps_its_value_with_nothing_said_and_a_computed_column_goes() {
        let target = numbered_target(
            Engine::MySql,
            "AUTO_INCREMENT",
            "GENERATED ALWAYS AS (`id` * 2)",
        );
        let row = rows(vec![vec![Some("3".into()), None, Some("6".into())]]);
        assert_eq!(
            file_of(Engine::MySql, &target, &row),
            "INSERT INTO `s`.`t` (`id`, `note`) VALUES ('3', NULL);\n"
        );
    }

    #[test]
    fn sources_of_a_result_reordered_since_fall_back_to_the_columns_own_names() {
        let result = QueryResult {
            columns: vec![column("id"), column("b"), column("a")],
            rows: rows(vec![vec![
                Some("1".into()),
                Some("beta".into()),
                Some("alpha".into()),
            ]]),
            ..QueryResult::default()
        };
        let target = InsertTarget {
            columns: Some(sources(&[
                ("id", Some("id")),
                ("a", Some("a")),
                ("b", Some("b")),
            ])),
            ..InsertTarget::default()
        };
        assert_eq!(
            inserts_file(&target, &result),
            "INSERT INTO table_name (\"id\", \"b\", \"a\") VALUES ('1', 'beta', 'alpha');\n"
        );
    }

    #[test]
    fn a_sql_server_identity_with_no_source_goes_by_its_own_name() {
        let result = QueryResult {
            columns: vec![column("code"), column("SEQ"), column("doubled")],
            rows: rows(vec![vec![
                Some("1".into()),
                Some("7".into()),
                Some("2".into()),
            ]]),
            ..QueryResult::default()
        };
        let target = |engine, numbered: &str| {
            let names = result.columns.iter().map(|column| column.name.as_str());
            InsertTarget {
                relation: Some(("s".into(), "t".into())),
                columns: Some(sources(
                    &names
                        .map(|name| (name, (name == "code").then_some("code")))
                        .collect::<Vec<_>>(),
                )),
                ..InsertTarget::default()
            }
            .knowing(engine, &[definition("seq", "int", numbered)])
        };
        let file = |engine, numbered| {
            let mut writer = FileAs::Inserts
                .writer(
                    Vec::new(),
                    engine,
                    &target(engine, numbered),
                    &result.columns,
                )
                .unwrap();
            writer.rows(&result.rows, &[]).unwrap();
            String::from_utf8(writer.finish().unwrap()).unwrap()
        };

        assert_eq!(
            file(Engine::SqlServer, "IDENTITY(1,1)"),
            "SET IDENTITY_INSERT \"s\".\"t\" ON;\n\
             INSERT INTO \"s\".\"t\" (\"code\", \"seq\") VALUES (N'1', N'7');\n\
             SET IDENTITY_INSERT \"s\".\"t\" OFF;\n"
        );
        assert_eq!(
            file(Engine::Postgres, "GENERATED ALWAYS AS IDENTITY"),
            "INSERT INTO \"s\".\"t\" (\"code\") VALUES ('1');\n"
        );
    }

    #[test]
    fn a_result_is_matched_to_its_table_by_the_source_of_each_column() {
        let target = InsertTarget {
            columns: Some(sources(&[
                ("id", Some("twice")),
                ("note", Some("note")),
                ("twice", Some("id")),
            ])),
            ..numbered_target(
                Engine::Postgres,
                "GENERATED ALWAYS AS IDENTITY",
                "GENERATED ALWAYS AS ((id * 2)) STORED",
            )
        };
        let row = rows(vec![vec![Some("6".into()), None, Some("3".into())]]);
        assert_eq!(
            file_of(Engine::Postgres, &target, &row),
            "INSERT INTO \"s\".\"t\" (\"note\", \"id\") OVERRIDING SYSTEM VALUE \
             VALUES (NULL, '3');\n"
        );
    }

    #[test]
    fn a_nul_in_text_is_pieced_together_where_a_literal_cannot_hold_one() {
        let spelled = |engine: Engine| engine.spec().text_around_nul("a\0b", None);
        assert_eq!(
            spelled(Engine::SqlServer).as_deref(),
            Some("CAST(N'a' AS nvarchar(max)) + NCHAR(0) + N'b'")
        );
        assert_eq!(
            spelled(Engine::Sqlite).as_deref(),
            Some("'a' || char(0) || 'b'")
        );
        assert_eq!(
            spelled(Engine::DuckDb).as_deref(),
            Some("'a' || chr(0) || 'b'")
        );
        assert_eq!(spelled(Engine::Postgres), None);
        assert_eq!(Engine::Sqlite.spec().text_around_nul("plain", None), None);
    }

    #[test]
    fn dot_sql_writes_inserts_only_where_the_server_reads_sql() {
        let path = Path::new("rows.SQL");
        assert_eq!(FileAs::for_path(path, Engine::Postgres), FileAs::Inserts);
        for engine in [Engine::MongoDb, Engine::Redis] {
            assert_eq!(FileAs::for_path(path, engine), FileAs::Rows(Format::Csv));
        }
        assert_eq!(
            FileAs::for_path(Path::new("rows.json"), Engine::Postgres),
            FileAs::Rows(Format::Json)
        );
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

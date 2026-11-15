//! The catalog through ODBC's catalog functions. Every value, numbers
//! included, is read as text through `raw::get_data`, since `KEY_SEQ`,
//! `NULLABLE` and every NULL indicator cross the same SQLLEN a query's do.

use std::collections::{BTreeMap, HashMap, HashSet};

use dbdelve_plugin_protocol::{
    Column, ForeignKey, Graph, GraphKey, GraphTable, Index, Reference, Relation, RelationKind,
    Response, Schema, Structure, UniqueKey,
};
use odbc_api::handles::StatementImpl;
use odbc_api::sys::{InfoType, SqlReturn};

use crate::connect::Session;
use crate::failure;
use crate::raw::{self, DataError, Fetched};
use crate::run::{self, Control};

type Row = Vec<Option<String>>;

const SCRATCH: usize = 4 << 10;

/// No catalog name or default is near this; it only bounds what a broken
/// driver can make the bridge hold.
const MOST_VALUE: usize = 1 << 20;

const SQL_CHAR: i16 = 1;
const SQL_NUMERIC: i16 = 2;
const SQL_DECIMAL: i16 = 3;
const SQL_VARCHAR: i16 = 12;
const SQL_WCHAR: i16 = -8;
const SQL_WVARCHAR: i16 = -9;
const SQL_NO_NULLS: &str = "0";
const SQL_TABLE_STAT: &str = "0";

pub fn catalog(session: &Session, control: &Control) -> Result<Response, Response> {
    // A driver without schemas (MySQL's) may refuse any schema argument at all.
    let schema = has_schemas(session).then_some("%");
    let rows = read(session, control, "SQLTables", 4, |statement| {
        raw::tables(statement, schema, "%")
    })?;
    Ok(Response::Catalog {
        schemas: schemas_from(&rows),
    })
}

pub fn structure(
    session: &Session,
    control: &Control,
    schema: &str,
    relation: &str,
) -> Result<Response, Response> {
    let scope = Scope::of(session, schema);
    // A driver without an escape leaves `_` and `%` as wildcards; the
    // exact-name filter below still keeps only the relation asked about.
    let escape =
        raw::info_string(session.handle(), InfoType::SearchPatternEscape).unwrap_or_default();
    let pattern = scope.schema.map(|schema| escape_pattern(schema, &escape));
    let columns = read(session, control, "SQLColumns", 13, |statement| {
        let relation = escape_pattern(relation, &escape);
        raw::columns(statement, scope.catalog, pattern.as_deref(), &relation)
    })?;
    let keys = read(session, control, "SQLPrimaryKeys", 6, |statement| {
        raw::primary_keys(statement, scope.catalog, scope.schema, relation)
    })?;
    // `SQLStatistics` takes ordinary arguments, not patterns: an escaped name
    // would be looked up with its escape characters in it.
    let statistics = read(session, control, "SQLStatistics", 9, |statement| {
        raw::statistics(statement, scope.catalog, scope.schema, relation)
    })?;
    let foreign = read(session, control, "SQLForeignKeys", 12, |statement| {
        let foreign = [scope.catalog, scope.schema, Some(relation)];
        raw::foreign_keys(statement, [None, None, None], foreign)
    })?;
    let (primary_key, key_name) = primary_key_from(&keys, schema, relation);
    Ok(Response::Structure(Structure {
        columns: columns_from(&columns, schema, relation),
        indexes: indexes_from(&statistics, schema, relation, &primary_key, &key_name),
        primary_key,
        foreign_keys: foreign_keys_from(&foreign, schema, relation),
    }))
}

pub fn references(
    session: &Session,
    control: &Control,
    schema: &str,
    relation: &str,
) -> Result<Response, Response> {
    let scope = Scope::of(session, schema);
    let rows = read(session, control, "SQLForeignKeys", 12, |statement| {
        let primary = [scope.catalog, scope.schema, Some(relation)];
        raw::foreign_keys(statement, primary, [None, None, None])
    })?;
    Ok(Response::References {
        references: references_from(&rows, schema, relation),
    })
}

/// One schema's base tables and keys for the ER diagram: `SQLTables` and
/// `SQLColumns` once for the schema, then three calls a table (see
/// [`Each::table`]).
pub fn graph(session: &Session, control: &Control, schema: &str) -> Result<Response, Response> {
    let scope = Scope::of(session, schema);
    let escape =
        raw::info_string(session.handle(), InfoType::SearchPatternEscape).unwrap_or_default();
    let pattern = scope.schema.map(|schema| escape_pattern(schema, &escape));
    let listed = read(session, control, "SQLTables", 4, |statement| {
        raw::tables(statement, pattern.as_deref(), "%")
    })?;
    let tables = tables_from(&listed, schema);
    let columns = read(session, control, "SQLColumns", 13, |statement| {
        raw::columns(statement, scope.catalog, pattern.as_deref(), "%")
    })?;
    let each = Each {
        session,
        control,
        schema,
        tables: &tables,
    };
    let keys = each.table("SQLPrimaryKeys", 6, 0, |statement, table| {
        raw::primary_keys(statement, scope.catalog, scope.schema, table)
    })?;
    let statistics = each.table("SQLStatistics", 13, 0, |statement, table| {
        raw::statistics(statement, scope.catalog, scope.schema, table)
    })?;
    let foreign = each.table("SQLForeignKeys", 12, 4, |statement, table| {
        let foreign = [scope.catalog, scope.schema, Some(table)];
        raw::foreign_keys(statement, [None, None, None], foreign)
    })?;
    Ok(Response::Graph(graph_from(
        schema,
        &tables,
        &columns,
        &keys,
        &statistics,
        &foreign,
    )))
}

/// A catalog function asked about every table of one schema.
struct Each<'a> {
    session: &'a Session,
    control: &'a Control,
    schema: &'a str,
    tables: &'a [String],
}

impl Each<'_> {
    /// Columns `1..=last` of `call`'s rows for every table, one call a table,
    /// keeping only the rows naming it (its `TABLE_CAT`-shaped column at
    /// `at`). Never one call for the schema: ODBC makes the table name an
    /// ordinary argument here, and the driver manager itself refuses a NULL
    /// one with `HY009` before any driver sees it, while psqlODBC and Db2
    /// answer `""` and `%` with no rows or an error, never every table.
    /// Each call is published, so a Cancel stops the loop at the next.
    ///
    /// ponytail: three calls a table, one after another on the one session;
    /// a driver-specific catalog query would read a schema in one, at the
    /// cost of SQL the bridge would have to spell per driver.
    fn table(
        &self,
        function: &str,
        last: u16,
        at: usize,
        call: impl Fn(&StatementImpl, &str) -> SqlReturn,
    ) -> Result<Vec<Row>, Response> {
        let mut rows = Vec::new();
        for table in self.tables {
            let named = read(self.session, self.control, function, last, |statement| {
                call(statement, table)
            })?;
            rows.extend(
                named
                    .into_iter()
                    .filter(|row| names(row, at, self.schema, table)),
            );
        }
        Ok(rows)
    }
}

/// The schema's base tables in the order `SQLTables` lists them. Only
/// `TABLE`: views, Db2's materialized query and temporary tables, system
/// tables, aliases and the rest are not drawn.
fn tables_from(rows: &[Row], schema: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter(|row| schema_of(row, 0) == schema && cell(row, 3).map(trim_name) == Some("TABLE"))
        .map(|row| name(row, 2))
        .filter(|table| seen.insert(table.clone()))
        .collect()
}

/// The graph out of the catalog functions' rows, each already narrowed to
/// `schema` by the call or filtered to it here.
fn graph_from(
    schema: &str,
    tables: &[String],
    columns: &[Row],
    keys: &[Row],
    statistics: &[Row],
    foreign: &[Row],
) -> Graph {
    let mut drawn: Vec<GraphTable> = tables
        .iter()
        .map(|table| GraphTable {
            name: table.clone(),
            columns: Vec::new(),
        })
        .collect();
    let at: HashMap<&str, usize> = tables
        .iter()
        .enumerate()
        .map(|(index, table)| (table.as_str(), index))
        .collect();
    let mine = |row: &Row, catalog: usize| -> Option<usize> {
        if schema_of(row, catalog) != schema {
            return None;
        }
        at.get(name(row, catalog + 2).as_str()).copied()
    };
    for row in columns {
        if let Some(table) = mine(row, 0) {
            drawn[table].columns.push(Column {
                name: name(row, 3),
                data_type: data_type(
                    cell(row, 5).unwrap_or_default(),
                    cell(row, 4),
                    cell(row, 6),
                    cell(row, 8),
                ),
                nullable: cell(row, 10).map(str::trim) != Some(SQL_NO_NULLS),
                default: None,
            });
        }
    }

    let mut primary = Groups::default();
    for row in keys {
        if let Some(table) = mine(row, 0) {
            let key = (String::new(), name(row, 5));
            primary.add(table, key, number(row, 4), name(row, 3));
        }
    }
    let mut unique_keys: Vec<UniqueKey> = (primary.0.into_iter())
        .map(|key| {
            let (table, name, columns) = key.into_parts();
            UniqueKey {
                table: tables[table].clone(),
                name,
                columns,
                primary: true,
            }
        })
        .collect();

    // An index whose column is not one of the table's is on an expression
    // (the driver names the expression, or nothing), so it makes no column
    // unique and is left out whole, as is one with a `FILTER_CONDITION`.
    let mut indexes = Groups::default();
    for row in statistics {
        let Some(table) = mine(row, 0) else {
            continue;
        };
        if cell(row, 6).map(str::trim) == Some(SQL_TABLE_STAT)
            || cell(row, 3).map(str::trim) != Some("0")
            || cell(row, 12).is_some_and(|filter| !filter.trim().is_empty())
        {
            continue;
        }
        let column = cell(row, 8)
            .map(trim_name)
            .filter(|column| {
                drawn[table]
                    .columns
                    .iter()
                    .any(|known| known.name == *column)
            })
            .map(str::to_string);
        indexes.add(table, (name(row, 4), name(row, 5)), number(row, 7), column);
    }
    for index in indexes.0 {
        let (table, name, columns) = index.into_parts();
        let Some(columns) = columns.into_iter().collect::<Option<Vec<_>>>() else {
            continue;
        };
        unique_keys.push(UniqueKey {
            table: tables[table].clone(),
            name,
            columns,
            primary: false,
        });
    }

    Graph {
        tables: drawn,
        unique_keys,
        foreign_keys: graph_keys_from(foreign, |row| mine(row, 4), tables),
    }
}

/// Rows gathered into keys or indexes by their table, qualifier and name, in
/// the order each was first seen.
struct Groups<T>(Vec<Group<T>>, HashMap<(usize, String, String), usize>);

impl<T> Default for Groups<T> {
    fn default() -> Self {
        Self(Vec::new(), HashMap::new())
    }
}

impl<T> Groups<T> {
    fn add(&mut self, table: usize, key: (String, String), position: u32, column: T) {
        let Self(groups, by_name) = self;
        let (qualifier, name) = key;
        let at = *by_name
            .entry((table, qualifier, name.clone()))
            .or_insert_with(|| {
                groups.push(Group {
                    table,
                    name,
                    columns: Vec::new(),
                });
                groups.len() - 1
            });
        groups[at].columns.push((position, column));
    }
}

struct Group<T> {
    table: usize,
    name: String,
    /// Each with its position (`KEY_SEQ`, `ORDINAL_POSITION`).
    columns: Vec<(u32, T)>,
}

impl<T> Group<T> {
    /// Its table, its name and its columns in order.
    fn into_parts(mut self) -> (usize, String, Vec<T>) {
        self.columns.sort_by_key(|(position, _)| *position);
        let columns = self.columns.into_iter().map(|(_, column)| column);
        (self.table, self.name, columns.collect())
    }
}

/// `SQLForeignKeys` asked from the foreign-key side, grouped into keys by
/// `FK_NAME`, each key's columns in `KEY_SEQ` order. Where a driver leaves
/// `FK_NAME` NULL, each unnamed `KEY_SEQ` 1 row starts a key of its own,
/// unless its table also has an unnamed `KEY_SEQ` 2 or more, when which rows
/// pair up cannot be told and none of that table's unnamed keys is drawn.
/// A key into another catalog is left out, as Snowflake's into another
/// database is: a graph's key names a schema and a table, and drawn by them
/// it would point at a table of this catalog.
fn graph_keys_from(
    rows: &[Row],
    mine: impl Fn(&Row) -> Option<usize>,
    tables: &[String],
) -> Vec<GraphKey> {
    let rows: Vec<(usize, &Row)> = rows
        .iter()
        .filter(|row| !in_another_catalog(row))
        .filter_map(|row| mine(row).map(|table| (table, row)))
        .collect();
    let unnamed_composite: HashSet<usize> = rows
        .iter()
        .filter(|(_, row)| name(row, 11).is_empty() && number(row, 8) > 1)
        .map(|(table, _)| *table)
        .collect();
    let mut keys = Groups::default();
    for (index, &(table, row)) in rows.iter().enumerate() {
        let key_name = match name(row, 11) {
            key_name if !key_name.is_empty() => key_name,
            _ if unnamed_composite.contains(&table) => continue,
            // Not a name any driver gives, so it cannot meet a named key.
            _ => format!("\0{index}"),
        };
        keys.add(table, (String::new(), key_name), number(row, 8), row);
    }
    keys.0
        .into_iter()
        .map(|key| {
            let (table, key_name, columns) = key.into_parts();
            GraphKey {
                table: tables[table].clone(),
                name: key_name,
                columns: columns.iter().map(|row| name(row, 7)).collect(),
                referenced_schema: schema_of(columns[0], 0),
                referenced_table: name(columns[0], 2),
                referenced_columns: columns.iter().map(|row| name(row, 3)).collect(),
            }
        })
        .collect()
}

/// Whether an `SQLForeignKeys` row's key points into a catalog other than
/// its own table's, where the driver has schemas: without them the catalog
/// is the schema the key names. A catalog a driver leaves NULL or blank on
/// either side is taken to be the same one.
fn in_another_catalog(row: &Row) -> bool {
    let (referenced, own) = (name(row, 0), name(row, 4));
    !name(row, 1).is_empty() && !referenced.is_empty() && !own.is_empty() && referenced != own
}

fn has_schemas(session: &Session) -> bool {
    raw::info_u32(session.handle(), InfoType::SchemaUsage) != Some(0)
}

/// Where the name the host sends back as a schema goes. The catalog lists a
/// driver without schemas (MySQL's) by its catalogs, so there it is one.
struct Scope<'a> {
    catalog: Option<&'a str>,
    schema: Option<&'a str>,
}

impl<'a> Scope<'a> {
    fn of(session: &Session, schema: &'a str) -> Self {
        let none = Self {
            catalog: None,
            schema: None,
        };
        match has_schemas(session) {
            _ if schema.is_empty() => none,
            false => Self {
                catalog: Some(schema),
                ..none
            },
            true => Self {
                schema: Some(schema),
                ..none
            },
        }
    }
}

/// Columns `1..=last` of every row the catalog function `call` leaves, as
/// text, with the call and each fetch where `cancel` can reach them.
fn read(
    session: &Session,
    control: &Control,
    function: &str,
    last: u16,
    call: impl FnOnce(&StatementImpl) -> SqlReturn,
) -> Result<Vec<Row>, Response> {
    let statement = session.statement()?;
    let rows = rows(&statement, control, function, last, call);
    control.settle(&statement);
    rows
}

fn rows(
    statement: &StatementImpl,
    control: &Control,
    function: &str,
    last: u16,
    call: impl FnOnce(&StatementImpl) -> SqlReturn,
) -> Result<Vec<Row>, Response> {
    let Some(called) = control.published(statement, || call(statement)) else {
        return Err(run::cancelled());
    };
    if !matches!(called, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
        return Err(control.failure(statement, function));
    }
    let cancelled = || control.cancelled();
    let mut scratch = vec![0; SCRATCH];
    let mut rows = Vec::new();
    loop {
        let Some(fetched) = control.published(statement, || raw::fetch(statement)) else {
            return Err(run::cancelled());
        };
        match fetched {
            SqlReturn::NO_DATA => return Ok(rows),
            SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {}
            _ => return Err(control.failure(statement, "SQLFetch")),
        }
        let row = (1..=last)
            .map(|column| {
                let value = raw::get_data(
                    statement,
                    column,
                    Fetched::Text,
                    MOST_VALUE,
                    &mut scratch,
                    &cancelled,
                );
                match value {
                    Ok(value) => Ok(value.map(|bytes| run::wide_text(&bytes))),
                    Err(DataError::Odbc) => Err(control.failure(statement, "SQLGetData")),
                    Err(DataError::Cancelled) => Err(run::cancelled()),
                    Err(DataError::TooLarge | DataError::Unmeasured) => {
                        Err(failure::plain(&format!(
                            "{function} returned a value over {MOST_VALUE} bytes, or one whose length the driver did not give."
                        )))
                    }
                }
            })
            .collect::<Result<Row, Response>>()?;
        rows.push(row);
    }
}

/// Db2 returns catalog names as blank-padded `CHAR`. Only trailing spaces go:
/// a leading one is part of a delimited name.
///
/// ponytail: a delimited name that really ends in a space is shown trimmed,
/// and its structure lookup misses it; nobody has asked.
fn trim_name(name: &str) -> &str {
    name.trim_end_matches(' ')
}

/// `name` as a catalog pattern that matches only itself.
fn escape_pattern(name: &str, escape: &str) -> String {
    if escape.is_empty() {
        return name.to_string();
    }
    let mut pattern = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(next) = rest.chars().next() {
        if rest.starts_with(escape) {
            pattern.push_str(escape);
            pattern.push_str(escape);
            rest = &rest[escape.len()..];
            continue;
        }
        if next == '_' || next == '%' {
            pattern.push_str(escape);
        }
        pattern.push(next);
        rest = &rest[next.len_utf8()..];
    }
    pattern
}

fn relation_kind(table_type: &str) -> Option<RelationKind> {
    match trim_name(table_type) {
        kind if kind.contains("VIEW") => Some(RelationKind::View),
        "TABLE" | "MATERIALIZED QUERY TABLE" | "GLOBAL TEMPORARY TABLE" => {
            Some(RelationKind::Table)
        }
        _ => None,
    }
}

/// `TYPE_NAME`, with its size for the character and decimal types, whose
/// name alone says too little to write a value that fits.
fn data_type(
    type_name: &str,
    sql_type: Option<&str>,
    size: Option<&str>,
    digits: Option<&str>,
) -> String {
    let name = trim_name(type_name);
    let number = |value: Option<&str>| value.and_then(|value| value.trim().parse::<u64>().ok());
    let sql_type = sql_type.and_then(|value| value.trim().parse::<i16>().ok());
    let arguments = match (sql_type, number(size), number(digits)) {
        (Some(SQL_CHAR | SQL_VARCHAR | SQL_WCHAR | SQL_WVARCHAR), Some(size), _) => {
            Some(size.to_string())
        }
        (Some(SQL_DECIMAL | SQL_NUMERIC), Some(size), Some(digits)) => {
            Some(format!("{size},{digits}"))
        }
        (Some(SQL_DECIMAL | SQL_NUMERIC), Some(size), None) => Some(size.to_string()),
        _ => None,
    };
    match arguments {
        Some(arguments) => format!("{name}({arguments})"),
        None => name.to_string(),
    }
}

fn cell(row: &Row, index: usize) -> Option<&str> {
    row.get(index)?.as_deref()
}

fn name(row: &Row, index: usize) -> String {
    cell(row, index)
        .map(trim_name)
        .unwrap_or_default()
        .to_string()
}

/// The schema of the row whose `TABLE_CAT`-shaped column is at `catalog`,
/// with its `TABLE_SCHEM` beside it: the schema, or the catalog where the
/// driver has no schemas.
fn schema_of(row: &Row, catalog: usize) -> String {
    match name(row, catalog + 1) {
        schema if schema.is_empty() => name(row, catalog),
        schema => schema,
    }
}

/// The second guard behind the escaped patterns: the row names exactly the
/// relation asked about.
fn names(row: &Row, catalog: usize, schema: &str, relation: &str) -> bool {
    schema_of(row, catalog) == schema && name(row, catalog + 2) == relation
}

fn number(row: &Row, index: usize) -> u32 {
    cell(row, index)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn schemas_from(rows: &[Row]) -> Vec<Schema> {
    let mut schemas: BTreeMap<String, Vec<Relation>> = BTreeMap::new();
    for row in rows {
        let Some(kind) = cell(row, 3).and_then(relation_kind) else {
            continue;
        };
        schemas
            .entry(schema_of(row, 0))
            .or_default()
            .push(Relation {
                name: name(row, 2),
                kind,
            });
    }
    schemas
        .into_iter()
        .map(|(name, relations)| Schema { name, relations })
        .collect()
}

fn columns_from(rows: &[Row], schema: &str, relation: &str) -> Vec<Column> {
    rows.iter()
        .filter(|row| names(row, 0, schema, relation))
        .map(|row| Column {
            name: name(row, 3),
            data_type: data_type(
                cell(row, 5).unwrap_or_default(),
                cell(row, 4),
                cell(row, 6),
                cell(row, 8),
            ),
            nullable: cell(row, 10).map(str::trim) != Some(SQL_NO_NULLS),
            default: cell(row, 12).map(str::to_string),
        })
        .collect()
}

/// The key's columns in `KEY_SEQ` order, and its `PK_NAME`, empty when the
/// driver gives none.
fn primary_key_from(rows: &[Row], schema: &str, relation: &str) -> (Vec<String>, String) {
    let rows: Vec<&Row> = rows
        .iter()
        .filter(|row| names(row, 0, schema, relation))
        .collect();
    let mut key: Vec<(u32, String)> = rows
        .iter()
        .map(|row| (number(row, 4), name(row, 3)))
        .collect();
    key.sort_by_key(|(sequence, _)| *sequence);
    let key_name = rows.first().map(|row| name(row, 5)).unwrap_or_default();
    (
        key.into_iter().map(|(_, column)| column).collect(),
        key_name,
    )
}

/// The index named as the key, where a driver names the key's index after it;
/// otherwise the first unique index on exactly the key's columns, in its
/// order, since ODBC does not say which index backs it.
fn indexes_from(
    rows: &[Row],
    schema: &str,
    relation: &str,
    primary_key: &[String],
    key_name: &str,
) -> Vec<Index> {
    let mut indexes: Vec<Gathered> = Vec::new();
    for row in rows {
        if !names(row, 0, schema, relation) || cell(row, 6).map(str::trim) == Some(SQL_TABLE_STAT) {
            continue;
        }
        let Some(column) = cell(row, 8).map(trim_name) else {
            continue;
        };
        let (qualifier, index) = (name(row, 4), name(row, 5));
        let column = (number(row, 7), column.to_string());
        match indexes
            .iter_mut()
            .find(|gathered| gathered.qualifier == qualifier && gathered.name == index)
        {
            Some(gathered) => gathered.columns.push(column),
            None => indexes.push(Gathered {
                qualifier,
                name: index,
                unique: cell(row, 3).map(str::trim) == Some("0"),
                columns: vec![column],
            }),
        }
    }
    let mut indexes: Vec<Index> = indexes
        .into_iter()
        .map(|mut gathered| {
            gathered.columns.sort_by_key(|(position, _)| *position);
            Index {
                name: gathered.name,
                columns: gathered
                    .columns
                    .into_iter()
                    .map(|(_, column)| column)
                    .collect(),
                unique: gathered.unique,
                primary: false,
            }
        })
        .collect();
    let named = indexes
        .iter()
        .position(|index| !key_name.is_empty() && index.name == key_name);
    let backing = named.or_else(|| {
        indexes.iter().position(|index| {
            index.unique && !primary_key.is_empty() && index.columns == primary_key
        })
    });
    if let Some(backing) = backing {
        indexes[backing].primary = true;
    }
    indexes
}

/// One index's rows, its columns still with their positions.
struct Gathered {
    qualifier: String,
    name: String,
    unique: bool,
    columns: Vec<(u32, String)>,
}

/// `SQLForeignKeys` asked from the foreign-key side: one entry per column,
/// each key's columns in key order.
fn foreign_keys_from(rows: &[Row], schema: &str, relation: &str) -> Vec<ForeignKey> {
    let mut keys: Vec<(String, u32, ForeignKey)> = rows
        .iter()
        .filter(|row| names(row, 4, schema, relation))
        .map(|row| {
            let key = ForeignKey {
                column: name(row, 7),
                referenced_schema: schema_of(row, 0),
                referenced_table: name(row, 2),
                referenced_column: name(row, 3),
            };
            (name(row, 11), number(row, 8), key)
        })
        .collect();
    keys.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    keys.into_iter().map(|(.., key)| key).collect()
}

/// `SQLForeignKeys` asked from the primary-key side, single-column keys only:
/// a filter on one column of a composite key matches rows that do not
/// reference the row it was asked from.
///
/// A driver may leave `FK_NAME` NULL. Each unnamed `KEY_SEQ` 1 row then starts
/// a key of its own, unless its table also has an unnamed `KEY_SEQ` 2 or more,
/// when which rows pair up cannot be told and none of that table's unnamed
/// keys is offered.
fn references_from(rows: &[Row], schema: &str, relation: &str) -> Vec<Reference> {
    let rows: Vec<&Row> = rows
        .iter()
        .filter(|row| names(row, 0, schema, relation))
        .collect();
    let source = |row: &Row| (schema_of(row, 4), name(row, 6));
    let unnamed_composite: Vec<(String, String)> = rows
        .iter()
        .filter(|row| name(row, 11).is_empty() && number(row, 8) > 1)
        .map(|row| source(row))
        .collect();
    let mut keys: Vec<((String, String, String), Vec<Reference>)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let reference = Reference {
            schema: schema_of(row, 4),
            table: name(row, 6),
            column: name(row, 7),
            referenced_column: name(row, 3),
        };
        let key_name = match name(row, 11) {
            key_name if !key_name.is_empty() => key_name,
            _ if unnamed_composite.contains(&source(row)) => continue,
            // Not a name any driver gives, so it cannot meet a named key.
            _ => format!("\0{index}"),
        };
        let key = (reference.schema.clone(), reference.table.clone(), key_name);
        match keys.iter_mut().find(|(name, _)| *name == key) {
            Some((_, columns)) => columns.push(reference),
            None => keys.push((key, vec![reference])),
        }
    }
    let mut references: Vec<Reference> = keys
        .into_iter()
        .filter_map(|(_, mut columns)| (columns.len() == 1).then(|| columns.remove(0)))
        .collect();
    references.dedup();
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[Option<&str>]) -> Row {
        cells.iter().map(|cell| cell.map(str::to_string)).collect()
    }

    #[test]
    fn a_pattern_matches_only_its_own_name() {
        assert_eq!(escape_pattern("my_table", "\\"), "my\\_table");
        assert_eq!(escape_pattern("100%\\x", "\\"), "100\\%\\\\x");
        assert_eq!(escape_pattern("my_table", ""), "my_table");
    }

    #[test]
    fn table_types_map_to_kinds_and_the_rest_are_left_out() {
        assert_eq!(relation_kind("VIEW"), Some(RelationKind::View));
        assert_eq!(
            relation_kind("MATERIALIZED QUERY TABLE"),
            Some(RelationKind::Table)
        );
        assert_eq!(relation_kind("TABLE     "), Some(RelationKind::Table));
        assert_eq!(relation_kind("SYSTEM TABLE"), None);
        assert_eq!(relation_kind("NICKNAME"), None);
        assert_eq!(relation_kind("HIERARCHY TABLE"), None);
    }

    #[test]
    fn character_and_decimal_types_carry_their_size() {
        assert_eq!(
            data_type("VARCHAR", Some("12"), Some("20"), None),
            "VARCHAR(20)"
        );
        assert_eq!(
            data_type("DECIMAL", Some("3"), Some("10"), Some("2")),
            "DECIMAL(10,2)"
        );
        assert_eq!(
            data_type("INTEGER", Some("4"), Some("10"), Some("0")),
            "INTEGER"
        );
    }

    #[test]
    fn names_lose_trailing_blanks_only() {
        assert_eq!(trim_name("CONFORMANCE   "), "CONFORMANCE");
        assert_eq!(trim_name("  lead"), "  lead");
    }

    #[test]
    fn a_driver_without_schemas_lists_by_catalog() {
        let rows = [
            row(&[Some("shop"), None, Some("orders"), Some("TABLE")]),
            row(&[Some("CAT"), Some("APP   "), Some("V"), Some("VIEW")]),
            row(&[Some("CAT"), Some("APP"), Some("SYN"), Some("SYNONYM")]),
        ];
        let schemas = schemas_from(&rows);
        let names: Vec<_> = schemas
            .iter()
            .map(|schema| (schema.name.as_str(), schema.relations.len()))
            .collect();
        assert_eq!(names, [("APP", 1), ("shop", 1)]);
    }

    #[test]
    fn the_key_follows_key_seq_and_names_its_index() {
        let keys = [
            row(&[None, Some("S"), Some("T"), Some("A"), Some("2"), Some("PK")]),
            row(&[None, Some("S"), Some("T"), Some("B"), Some("1"), Some("PK")]),
            row(&[
                None,
                Some("S"),
                Some("TX"),
                Some("C"),
                Some("1"),
                Some("PX"),
            ]),
        ];
        let (key, key_name) = primary_key_from(&keys, "S", "T");
        assert_eq!(
            (key.clone(), key_name.as_str()),
            (vec!["B".to_string(), "A".to_string()], "PK")
        );
        let index = |non_unique, name, position, column| {
            let head = [None, Some("S"), Some("T")];
            let tail = [non_unique, Some("S"), name, Some("3"), position, column];
            row(&[head.as_slice(), tail.as_slice()].concat())
        };
        let statistics = [
            row(&[None, Some("S"), Some("T"), None, None, None, Some("0")]),
            index(Some("0"), Some("DUP"), Some("1"), Some("B")),
            index(Some("0"), Some("DUP"), Some("2"), Some("A")),
            index(Some("0"), Some("PK"), Some("2"), Some("A")),
            index(Some("0"), Some("PK"), Some("1"), Some("B")),
            index(Some("1"), Some("BY_A"), Some("1"), Some("A")),
        ];
        let read = |key_name: &str| -> Vec<(String, String, bool, bool)> {
            indexes_from(&statistics, "S", "T", &key, key_name)
                .into_iter()
                .map(|index| {
                    (
                        index.name,
                        index.columns.join(","),
                        index.unique,
                        index.primary,
                    )
                })
                .collect()
        };
        let owned = |name: &str, columns: &str, unique, primary| {
            (name.to_string(), columns.to_string(), unique, primary)
        };
        assert_eq!(
            read("PK"),
            [
                owned("DUP", "B,A", true, false),
                owned("PK", "B,A", true, true),
                owned("BY_A", "A", false, false),
            ]
        );
        let unnamed: Vec<_> = read("").into_iter().map(|index| index.3).collect();
        assert_eq!(unnamed, [true, false, false]);
    }

    #[test]
    fn only_the_schemas_base_tables_are_drawn_once_each() {
        let rows = [
            row(&[None, Some("S"), Some("ORDERS"), Some("TABLE")]),
            row(&[
                None,
                Some("S  "),
                Some("TOTALS"),
                Some("MATERIALIZED QUERY TABLE"),
            ]),
            row(&[None, Some("S"), Some("V"), Some("VIEW")]),
            row(&[None, Some("S"), Some("SYS"), Some("SYSTEM TABLE")]),
            row(&[None, Some("OTHER"), Some("T"), Some("TABLE")]),
            row(&[None, Some("S"), Some("ORDERS"), Some("TABLE")]),
        ];
        assert_eq!(tables_from(&rows, "S"), ["ORDERS"]);
    }

    fn column(table: &str, column: &str, type_name: &str, nullable: &str) -> Row {
        row(&[
            None,
            Some("S"),
            Some(table),
            Some(column),
            Some(if type_name == "VARCHAR" { "12" } else { "4" }),
            Some(type_name),
            Some("10"),
            None,
            Some("0"),
            None,
            Some(nullable),
            None,
            None,
        ])
    }

    fn primary(table: &str, column: &str, sequence: &str, name: &str) -> Row {
        row(&[
            None,
            Some("S"),
            Some(table),
            Some(column),
            Some(sequence),
            Some(name),
        ])
    }

    fn statistic(
        table: &str,
        non_unique: &str,
        name: &str,
        position: &str,
        column: Option<&str>,
    ) -> Row {
        row(&[
            None,
            Some("S"),
            Some(table),
            Some(non_unique),
            Some("S"),
            Some(name),
            Some("3"),
            Some(position),
            column,
        ])
    }

    fn foreign(
        table: &str,
        column: &str,
        sequence: &str,
        name: Option<&str>,
        to: (&str, &str, &str),
    ) -> Row {
        let (schema, referenced, referenced_column) = to;
        row(&[
            None,
            Some(schema),
            Some(referenced),
            Some(referenced_column),
            None,
            Some("S"),
            Some(table),
            Some(column),
            Some(sequence),
            None,
            None,
            name,
        ])
    }

    /// The canned rows Db2's catalog functions give for a composite key and
    /// a key into another schema, read into the graph `graph` answers with.
    fn graph() -> Graph {
        let tables = ["ORDERS".to_string(), "ITEMS".to_string(), "LOG".to_string()];
        let columns = [
            column("ORDERS", "ACCOUNT", "BIGINT", "0"),
            column("ORDERS", "NUMBER", "INTEGER", "0"),
            column("ORDERS", "CODE", "VARCHAR", "1"),
            column("ITEMS", "ID", "INTEGER", "0"),
            column("ITEMS", "NUMBER", "INTEGER", "1"),
            column("ITEMS", "ACCOUNT", "BIGINT", "1"),
            column("ITEMS", "OWNER", "BIGINT", "1"),
            column("ITEMS", "A", "INTEGER", "1"),
            column("ITEMS", "B", "INTEGER", "1"),
            column("A_VIEW", "ID", "INTEGER", "1"),
        ];
        let keys = [
            primary("ORDERS", "NUMBER", "2", "SQL2"),
            primary("ORDERS", "ACCOUNT", "1", "SQL2"),
            primary("ITEMS", "ID", "1", "SQL1"),
        ];
        let statistics = [
            row(&[None, Some("S"), Some("ORDERS"), None, None, None, Some("0")]),
            statistic("ORDERS", "0", "SQL1", "1", Some("CODE")),
            statistic("ORDERS", "0", "SQL9", "2", Some("NUMBER")),
            statistic("ORDERS", "0", "SQL9", "1", Some("ACCOUNT")),
            statistic("ITEMS", "1", "BY_OWNER", "1", Some("OWNER")),
            statistic("ITEMS", "0", "LOWER_A", "1", Some("LOWER(A)")),
            statistic("ITEMS", "0", "NAMELESS", "1", None),
            statistic("ITEMS", "0", "A_AND_EXPR", "1", Some("A")),
            statistic("ITEMS", "0", "A_AND_EXPR", "2", Some("")),
        ];
        let foreign_keys = [
            foreign(
                "ITEMS",
                "NUMBER",
                "2",
                Some("TO_ORDER"),
                ("S", "ORDERS", "NUMBER"),
            ),
            foreign(
                "ITEMS",
                "ACCOUNT",
                "1",
                Some("TO_ORDER"),
                ("S", "ORDERS", "ACCOUNT"),
            ),
            foreign(
                "ITEMS",
                "OWNER",
                "1",
                Some("TO_OWNER"),
                ("BILLING", "OWNERS", "ID"),
            ),
            foreign("ITEMS", "A", "1", None, ("S", "LOG", "X")),
            foreign("ITEMS", "B", "1", None, ("S", "LOG", "Y")),
            foreign("A_VIEW", "ID", "1", Some("NOT_DRAWN"), ("S", "LOG", "X")),
        ];
        graph_from("S", &tables, &columns, &keys, &statistics, &foreign_keys)
    }

    #[test]
    fn a_graph_holds_the_tables_columns_in_order_and_nothing_of_a_view() {
        let graph = graph();
        let tables: Vec<(&str, Vec<&str>)> = graph
            .tables
            .iter()
            .map(|table| {
                let columns = table.columns.iter().map(|column| column.name.as_str());
                (table.name.as_str(), columns.collect())
            })
            .collect();
        assert_eq!(
            tables,
            [
                ("ORDERS", vec!["ACCOUNT", "NUMBER", "CODE"]),
                ("ITEMS", vec!["ID", "NUMBER", "ACCOUNT", "OWNER", "A", "B"]),
                ("LOG", vec![]),
            ]
        );
        let code = &graph.tables[0].columns[2];
        assert_eq!(
            (code.data_type.as_str(), code.nullable),
            ("VARCHAR(10)", true)
        );
        assert!(!graph.tables[0].columns[0].nullable);
    }

    /// Db2 names a primary key after another column's unique index, so a
    /// name is no way to tell the key's own index; both are kept, and an
    /// index on an expression, which the driver names by it or not at all,
    /// is left out whole.
    #[test]
    fn unique_keys_follow_key_seq_and_leave_out_expressions_and_plain_indexes() {
        let graph = graph();
        let keys: Vec<(&str, &str, String, bool)> = graph
            .unique_keys
            .iter()
            .map(|key| {
                let columns = key.columns.join(",");
                (key.table.as_str(), key.name.as_str(), columns, key.primary)
            })
            .collect();
        let owned =
            |table, name, columns: &str, primary| (table, name, columns.to_string(), primary);
        assert_eq!(
            keys,
            [
                owned("ORDERS", "SQL2", "ACCOUNT,NUMBER", true),
                owned("ITEMS", "SQL1", "ID", true),
                owned("ORDERS", "SQL1", "CODE", false),
                owned("ORDERS", "SQL9", "ACCOUNT,NUMBER", false),
            ]
        );
    }

    #[test]
    fn a_filtered_index_is_left_out_and_a_qualifier_keeps_indexes_apart() {
        let tables = ["T".to_string()];
        let columns = [
            column("T", "A", "INTEGER", "1"),
            column("T", "B", "INTEGER", "1"),
        ];
        let index = |qualifier: &str, name: &str, column: &str, filter: Option<&str>| {
            let mut row = statistic("T", "0", name, "1", Some(column));
            row[4] = Some(qualifier.to_string());
            row.extend([None, None, None, filter.map(str::to_string)]);
            row
        };
        let statistics = [
            index("S", "SAME", "A", None),
            index("X", "SAME", "B", None),
            index("S", "PARTIAL", "A", Some("(a > 0)")),
        ];
        let graph = graph_from("S", &tables, &columns, &[], &statistics, &[]);
        let keys: Vec<Vec<String>> = graph
            .unique_keys
            .into_iter()
            .map(|key| key.columns)
            .collect();
        assert_eq!(keys, [["A"], ["B"]]);
    }

    #[test]
    fn foreign_keys_group_by_name_in_key_seq_order_wherever_they_point() {
        let keys: Vec<(String, String, String, String)> = graph()
            .foreign_keys
            .into_iter()
            .map(|key| {
                (
                    key.columns.join(","),
                    key.referenced_schema,
                    key.referenced_table,
                    key.referenced_columns.join(","),
                )
            })
            .collect();
        let owned = |columns: &str, schema: &str, table: &str, referenced: &str| {
            let owned = |text: &str| text.to_string();
            (
                owned(columns),
                owned(schema),
                owned(table),
                owned(referenced),
            )
        };
        assert_eq!(
            keys,
            [
                owned("ACCOUNT,NUMBER", "S", "ORDERS", "ACCOUNT,NUMBER"),
                owned("OWNER", "BILLING", "OWNERS", "ID"),
                owned("A", "S", "LOG", "X"),
                owned("B", "S", "LOG", "Y"),
            ]
        );
        let names: Vec<String> = graph()
            .foreign_keys
            .into_iter()
            .map(|key| key.name)
            .collect();
        assert_ne!(names[2], names[3], "two unnamed keys stay two");
    }

    #[test]
    fn a_key_into_another_catalog_is_left_out() {
        let tables = ["ITEMS".to_string(), "ACCOUNTS".to_string()];
        let key = |name: &str, catalogs: (Option<&str>, Option<&str>), schema: &str| {
            let mut row = foreign("ITEMS", "A", "1", Some(name), (schema, "ACCOUNTS", "ID"));
            (row[0], row[4]) = (
                catalogs.0.map(str::to_string),
                catalogs.1.map(str::to_string),
            );
            row
        };
        let rows = [
            key("ELSEWHERE", (Some("OTHER_DB"), Some("HERE")), "S"),
            key(
                "ELSEWHERE_STUB",
                (Some("OTHER_DB"), Some("HERE")),
                "BILLING",
            ),
            key("SAME", (Some("HERE  "), Some("HERE")), "S"),
            key("UNNAMED", (None, None), "S"),
            key("HALF", (Some("OTHER_DB"), None), "S"),
            key("NO_SCHEMAS", (Some("OTHER_DB"), Some("HERE")), ""),
        ];
        let graph = graph_from("S", &tables, &[], &[], &[], &rows);
        let keys: Vec<&str> = graph
            .foreign_keys
            .iter()
            .map(|key| key.name.as_str())
            .collect();
        assert_eq!(keys, ["SAME", "UNNAMED", "HALF", "NO_SCHEMAS"]);
        assert_eq!(graph.foreign_keys[3].referenced_schema, "OTHER_DB");
    }

    #[test]
    fn unnamed_keys_of_a_table_with_an_unnamed_composite_are_not_drawn() {
        let tables = ["ITEMS".to_string(), "LOG".to_string()];
        let rows = [
            foreign("ITEMS", "A", "1", None, ("S", "LOG", "X")),
            foreign("ITEMS", "B", "2", None, ("S", "LOG", "Y")),
            foreign("ITEMS", "C", "1", Some("NAMED"), ("S", "LOG", "Z")),
        ];
        let graph = graph_from("S", &tables, &[], &[], &[], &rows);
        let keys: Vec<&str> = graph
            .foreign_keys
            .iter()
            .map(|key| key.name.as_str())
            .collect();
        assert_eq!(keys, ["NAMED"]);
    }

    #[test]
    fn only_single_column_keys_are_references() {
        let fk = |table: &str, column: &str, sequence: &str, name: Option<&str>| {
            let primary = [None, Some("S"), Some("P"), Some("ID")];
            let foreign = [None, Some("S"), Some(table), Some(column), Some(sequence)];
            row(&[&primary[..], &foreign[..], &[None, None, name]].concat())
        };
        let rows = [
            fk("ONE", "P_ID", "1", Some("FK1")),
            fk("TWO", "PA", "1", Some("FK2")),
            fk("TWO", "PB", "2", Some("FK2")),
            fk("THREE", "X", "1", None),
            fk("THREE", "Y", "1", None),
            fk("FOUR", "M", "1", None),
            fk("FOUR", "N", "2", None),
        ];
        let references: Vec<_> = references_from(&rows, "S", "P")
            .into_iter()
            .map(|reference| format!("{}.{}", reference.table, reference.column))
            .collect();
        assert_eq!(references, ["ONE.P_ID", "THREE.X", "THREE.Y"]);
    }
}

//! The catalog through ODBC's catalog functions. Every value, numbers
//! included, is read as text through `raw::get_data`, since `KEY_SEQ`,
//! `NULLABLE` and every NULL indicator cross the same SQLLEN a query's do.

use std::collections::BTreeMap;

use dbdelve_plugin_protocol::{
    Column, ForeignKey, Index, Reference, Relation, RelationKind, Response, Schema, Structure,
};
use odbc_api::handles::StatementImpl;
use odbc_api::sys::{InfoType, SqlReturn};

use crate::connect::Session;
use crate::raw::{self, DataError, Fetched};
use crate::{failure, run};

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

pub fn catalog(session: &Session) -> Result<Response, Response> {
    let statement = session.statement()?;
    let called = raw::tables(&statement, "%", "%");
    let rows = rows(&statement, called, "SQLTables", 4)?;
    Ok(Response::Catalog {
        schemas: schemas_from(&rows),
    })
}

pub fn structure(session: &Session, schema: &str, relation: &str) -> Result<Response, Response> {
    let scope = Scope::of(session, schema);
    // A driver without an escape leaves `_` and `%` as wildcards; the
    // exact-name filter below still keeps only the relation asked about.
    let escape =
        raw::info_string(session.handle(), InfoType::SearchPatternEscape).unwrap_or_default();
    let columns = {
        let statement = session.statement()?;
        let schema = scope.schema.map(|schema| escape_pattern(schema, &escape));
        let called = raw::columns(
            &statement,
            scope.catalog,
            schema.as_deref(),
            &escape_pattern(relation, &escape),
        );
        rows(&statement, called, "SQLColumns", 13)?
    };
    let keys = {
        let statement = session.statement()?;
        let called = raw::primary_keys(&statement, scope.catalog, scope.schema, relation);
        rows(&statement, called, "SQLPrimaryKeys", 5)?
    };
    // `SQLStatistics` takes ordinary arguments, not patterns: an escaped name
    // would be looked up with its escape characters in it.
    let statistics = {
        let statement = session.statement()?;
        let called = raw::statistics(&statement, scope.catalog, scope.schema, relation);
        rows(&statement, called, "SQLStatistics", 9)?
    };
    let foreign = {
        let statement = session.statement()?;
        let called = raw::foreign_keys(
            &statement,
            [None, None, None],
            [scope.catalog, scope.schema, Some(relation)],
        );
        rows(&statement, called, "SQLForeignKeys", 12)?
    };
    let primary_key = primary_key_from(&keys, schema, relation);
    Ok(Response::Structure(Structure {
        columns: columns_from(&columns, schema, relation),
        indexes: indexes_from(&statistics, schema, relation, &primary_key),
        primary_key,
        foreign_keys: foreign_keys_from(&foreign, schema, relation),
    }))
}

pub fn references(session: &Session, schema: &str, relation: &str) -> Result<Response, Response> {
    let scope = Scope::of(session, schema);
    let statement = session.statement()?;
    let called = raw::foreign_keys(
        &statement,
        [scope.catalog, scope.schema, Some(relation)],
        [None, None, None],
    );
    let rows = rows(&statement, called, "SQLForeignKeys", 12)?;
    Ok(Response::References {
        references: references_from(&rows, schema, relation),
    })
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
        match raw::info_u32(session.handle(), InfoType::SchemaUsage) {
            _ if schema.is_empty() => none,
            Some(0) => Self {
                catalog: Some(schema),
                ..none
            },
            _ => Self {
                schema: Some(schema),
                ..none
            },
        }
    }
}

/// Columns `1..=last` of every row the catalog call left, as text.
fn rows(
    statement: &StatementImpl,
    called: SqlReturn,
    function: &str,
    last: u16,
) -> Result<Vec<Row>, Response> {
    if !matches!(called, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
        return Err(failure::from(statement, function));
    }
    let mut scratch = vec![0; SCRATCH];
    let mut rows = Vec::new();
    loop {
        match raw::fetch(statement) {
            SqlReturn::NO_DATA => return Ok(rows),
            SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {}
            _ => return Err(failure::from(statement, "SQLFetch")),
        }
        let row = (1..=last)
            .map(|column| {
                match raw::get_data(statement, column, Fetched::Text, MOST_VALUE, &mut scratch) {
                    Ok(value) => Ok(value.map(|bytes| run::rendered(Fetched::Text, &bytes))),
                    Err(DataError::Odbc) => Err(failure::from(statement, "SQLGetData")),
                    Err(DataError::TooLarge) => Err(failure::plain(&format!(
                        "{function} returned a value over {MOST_VALUE} bytes."
                    ))),
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

fn primary_key_from(rows: &[Row], schema: &str, relation: &str) -> Vec<String> {
    let mut key: Vec<(u32, String)> = rows
        .iter()
        .filter(|row| names(row, 0, schema, relation))
        .map(|row| (number(row, 4), name(row, 3)))
        .collect();
    key.sort_by_key(|(sequence, _)| *sequence);
    key.into_iter().map(|(_, column)| column).collect()
}

/// ODBC does not say which index backs the primary key; a unique index on
/// exactly its columns, in its order, is taken as the one.
fn indexes_from(rows: &[Row], schema: &str, relation: &str, primary_key: &[String]) -> Vec<Index> {
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
    indexes
        .into_iter()
        .map(|mut gathered| {
            gathered.columns.sort_by_key(|(position, _)| *position);
            let columns: Vec<String> = gathered
                .columns
                .into_iter()
                .map(|(_, column)| column)
                .collect();
            Index {
                primary: gathered.unique && !primary_key.is_empty() && columns == primary_key,
                name: gathered.name,
                columns,
                unique: gathered.unique,
            }
        })
        .collect()
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
fn references_from(rows: &[Row], schema: &str, relation: &str) -> Vec<Reference> {
    let mut keys: Vec<((String, String, String), Vec<Reference>)> = Vec::new();
    for row in rows.iter().filter(|row| names(row, 0, schema, relation)) {
        let reference = Reference {
            schema: schema_of(row, 4),
            table: name(row, 6),
            column: name(row, 7),
            referenced_column: name(row, 3),
        };
        let key = (
            reference.schema.clone(),
            reference.table.clone(),
            name(row, 11),
        );
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
            row(&[None, Some("S"), Some("T"), Some("A"), Some("2")]),
            row(&[None, Some("S"), Some("T"), Some("B"), Some("1")]),
            row(&[None, Some("S"), Some("TX"), Some("C"), Some("1")]),
        ];
        let key = primary_key_from(&keys, "S", "T");
        assert_eq!(key, ["B", "A"]);
        let index = |non_unique, name, position, column| {
            let head = [None, Some("S"), Some("T")];
            let tail = [non_unique, Some("S"), name, Some("3"), position, column];
            row(&[head.as_slice(), tail.as_slice()].concat())
        };
        let statistics = [
            row(&[None, Some("S"), Some("T"), None, None, None, Some("0")]),
            index(Some("0"), Some("PK"), Some("2"), Some("A")),
            index(Some("0"), Some("PK"), Some("1"), Some("B")),
            index(Some("1"), Some("BY_A"), Some("1"), Some("A")),
        ];
        let indexes = indexes_from(&statistics, "S", "T", &key);
        let read: Vec<_> = indexes
            .iter()
            .map(|index| {
                (
                    index.name.as_str(),
                    index.columns.join(","),
                    index.unique,
                    index.primary,
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("PK", "B,A".to_string(), true, true),
                ("BY_A", "A".to_string(), false, false)
            ]
        );
    }

    #[test]
    fn only_single_column_keys_are_references() {
        let fk = |table: &str, column: &str, referenced: &str, name: &str| {
            row(&[
                None,
                Some("S"),
                Some("P"),
                Some(referenced),
                None,
                Some("S"),
                Some(table),
                Some(column),
                Some("1"),
                None,
                None,
                Some(name),
            ])
        };
        let rows = [
            fk("ONE", "P_ID", "ID", "FK1"),
            fk("TWO", "PA", "A", "FK2"),
            fk("TWO", "PB", "B", "FK2"),
        ];
        let references = references_from(&rows, "S", "P");
        assert_eq!(
            references,
            [Reference {
                schema: "S".into(),
                table: "ONE".into(),
                column: "P_ID".into(),
                referenced_column: "ID".into(),
            }]
        );
    }
}

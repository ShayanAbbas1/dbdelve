//! One schema's tables and declared keys, as the ER diagram draws them.
//!
//! Read in bulk by [`super::Connection::schema_graph`], separately from the
//! catalog, which must never hold columns, and slimmer than a [`super::Structure`]
//! per table: no defaults, indexes or constraint text, only what a card and an
//! edge show. Names are [`SharedString`] because that is what shaping text
//! takes, so painting a frame bumps a reference count instead of copying.

use std::collections::HashMap;

use gpui::SharedString;

use super::{DbError, QueryResult, optional_cell, required_cell, unexpected_catalog_value};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaGraph {
    pub schema: SharedString,
    /// Base tables only, in the order the catalog lists them. Partitions are
    /// folded into their parent, and views are not drawn.
    pub tables: Vec<GraphTable>,
    /// Every distinct type label once: a thousand tables use a few dozen
    /// types, and a column holds an index into this rather than a copy.
    pub types: Vec<SharedString>,
    pub keys: Vec<GraphKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphTable {
    pub name: SharedString,
    /// In the table's own column order.
    pub columns: Vec<GraphColumn>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphColumn {
    pub name: SharedString,
    /// An index into [`SchemaGraph::types`].
    pub type_label: u32,
    pub nullable: bool,
    /// Part of the table's primary key.
    pub primary: bool,
    /// Part of a foreign key in [`SchemaGraph::keys`].
    pub foreign: bool,
    /// Unique on its own: the whole of a primary key, unique constraint or
    /// unique index. A column in a composite unique key is not, because only a
    /// key whose columns are exactly unique makes a reference one-to-one, and
    /// a single column is the case a flag can say.
    pub unique: bool,
}

/// One foreign key constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphKey {
    pub name: SharedString,
    /// An index into [`SchemaGraph::tables`].
    pub table: u32,
    /// Indexes into that table's columns, in key order.
    pub columns: Vec<u32>,
    pub referenced: KeyTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyTarget {
    /// A table of this graph. `columns` index into its columns and pair with
    /// [`GraphKey::columns`] by position.
    Table { table: u32, columns: Vec<u32> },
    /// A table in another schema, drawn as a stub: its columns are not read.
    Elsewhere {
        schema: SharedString,
        table: SharedString,
    },
}

/// Reads a schema's graph out of three results whose columns are named the way
/// dbdelve names them, so every engine's catalog SQL aliases to these:
///
/// - `columns`: `table_name`, `column_name`, `data_type`, `nullable` (`yes` or
///   `no`), in column order within each table. A table with no columns is one
///   row whose `column_name` is NULL.
/// - `unique_keys`: `table_name`, `constraint_name`, `column_name`, `key_kind`
///   (`primary` or `unique`), one row per column of each primary key, unique
///   constraint or unique index. Partial and expression indexes are left out
///   by the engine, since neither makes its columns unique.
/// - `foreign_keys`: `table_name`, `constraint_name`, `column_name`,
///   `referenced_schema`, `referenced_table`, `referenced_column`, one row per
///   column of each key, in key order within it.
///
/// A key naming a table or column the columns result does not hold is dropped:
/// the three are separate statements, and a table created between them is not
/// worth failing the diagram over.
pub(super) fn assemble_schema_graph(
    schema: &str,
    columns: &QueryResult,
    unique_keys: &QueryResult,
    foreign_keys: &QueryResult,
) -> Result<SchemaGraph, DbError> {
    let mut graph = SchemaGraph {
        schema: schema.to_string().into(),
        ..SchemaGraph::default()
    };
    let mut tables: HashMap<&str, u32> = HashMap::new();
    let mut types: HashMap<&str, u32> = HashMap::new();

    for row in &columns.rows {
        let table_name = required_cell(columns, row, "table_name")?;
        let table = *tables.entry(table_name).or_insert_with(|| {
            graph.tables.push(GraphTable {
                name: table_name.to_string().into(),
                columns: Vec::new(),
            });
            graph.tables.len() as u32 - 1
        });
        let Some(column_name) = optional_cell(columns, row, "column_name") else {
            continue;
        };
        let data_type = required_cell(columns, row, "data_type")?;
        let type_label = *types.entry(data_type).or_insert_with(|| {
            graph.types.push(data_type.to_string().into());
            graph.types.len() as u32 - 1
        });
        graph.tables[table as usize].columns.push(GraphColumn {
            name: column_name.to_string().into(),
            type_label,
            nullable: match required_cell(columns, row, "nullable")? {
                "yes" => true,
                "no" => false,
                value => return Err(unexpected_catalog_value("nullability", value)),
            },
            primary: false,
            foreign: false,
            unique: false,
        });
    }

    let mut unique: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for row in &unique_keys.rows {
        let table_name = required_cell(unique_keys, row, "table_name")?;
        let column_name = required_cell(unique_keys, row, "column_name")?;
        let primary = match required_cell(unique_keys, row, "key_kind")? {
            "primary" => true,
            "unique" => false,
            value => return Err(unexpected_catalog_value("key kind", value)),
        };
        if primary && let Some(column) = column_mut(&mut graph, &tables, table_name, column_name) {
            column.primary = true;
        }
        unique
            .entry((
                table_name,
                required_cell(unique_keys, row, "constraint_name")?,
            ))
            .or_default()
            .push(column_name);
    }
    for ((table_name, _), columns) in unique {
        if let [column_name] = columns[..]
            && let Some(column) = column_mut(&mut graph, &tables, table_name, column_name)
        {
            column.unique = true;
        }
    }

    let mut pending: Vec<PendingKey> = Vec::new();
    let mut by_name: HashMap<(&str, &str), usize> = HashMap::new();
    for row in &foreign_keys.rows {
        let table = required_cell(foreign_keys, row, "table_name")?;
        let name = required_cell(foreign_keys, row, "constraint_name")?;
        let index = *by_name.entry((table, name)).or_insert_with(|| {
            pending.push(PendingKey {
                table,
                name,
                referenced_schema: "",
                referenced_table: "",
                columns: Vec::new(),
            });
            pending.len() - 1
        });
        let key = &mut pending[index];
        key.referenced_schema = required_cell(foreign_keys, row, "referenced_schema")?;
        key.referenced_table = required_cell(foreign_keys, row, "referenced_table")?;
        key.columns.push((
            required_cell(foreign_keys, row, "column_name")?,
            required_cell(foreign_keys, row, "referenced_column")?,
        ));
    }
    for key in pending {
        if let Some(key) = resolve(&graph, &tables, schema, &key) {
            for &column in &key.columns {
                graph.tables[key.table as usize].columns[column as usize].foreign = true;
            }
            graph.keys.push(key);
        }
    }

    Ok(graph)
}

/// A foreign key's rows, gathered before its table and columns are looked up.
struct PendingKey<'a> {
    table: &'a str,
    name: &'a str,
    referenced_schema: &'a str,
    referenced_table: &'a str,
    columns: Vec<(&'a str, &'a str)>,
}

fn resolve(
    graph: &SchemaGraph,
    tables: &HashMap<&str, u32>,
    schema: &str,
    key: &PendingKey,
) -> Option<GraphKey> {
    let table = *tables.get(key.table)?;
    let columns = key
        .columns
        .iter()
        .map(|(column, _)| column_index(&graph.tables[table as usize], column))
        .collect::<Option<Vec<_>>>()?;
    let referenced = match key.referenced_schema == schema {
        true => {
            let referenced = *tables.get(key.referenced_table)?;
            KeyTarget::Table {
                table: referenced,
                columns: key
                    .columns
                    .iter()
                    .map(|(_, column)| column_index(&graph.tables[referenced as usize], column))
                    .collect::<Option<Vec<_>>>()?,
            }
        }
        false => KeyTarget::Elsewhere {
            schema: key.referenced_schema.to_string().into(),
            table: key.referenced_table.to_string().into(),
        },
    };
    Some(GraphKey {
        name: key.name.to_string().into(),
        table,
        columns,
        referenced,
    })
}

fn column_index(table: &GraphTable, name: &str) -> Option<u32> {
    table
        .columns
        .iter()
        .position(|column| column.name.as_ref() == name)
        .map(|index| index as u32)
}

fn column_mut<'a>(
    graph: &'a mut SchemaGraph,
    tables: &HashMap<&str, u32>,
    table: &str,
    column: &str,
) -> Option<&'a mut GraphColumn> {
    let table = &mut graph.tables[*tables.get(table)? as usize];
    let column = column_index(table, column)?;
    table.columns.get_mut(column as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::result;

    fn graph() -> SchemaGraph {
        let columns = result(
            &["table_name", "column_name", "data_type", "nullable"],
            &[
                &[
                    Some("orders"),
                    Some("account_id"),
                    Some("bigint"),
                    Some("no"),
                ],
                &[Some("orders"), Some("number"), Some("integer"), Some("no")],
                &[Some("order_items"), Some("id"), Some("integer"), Some("no")],
                &[
                    Some("order_items"),
                    Some("order_number"),
                    Some("integer"),
                    Some("yes"),
                ],
                &[
                    Some("order_items"),
                    Some("order_account_id"),
                    Some("bigint"),
                    Some("yes"),
                ],
                &[Some("order_items"), Some("sku"), Some("text"), Some("no")],
                &[Some("order_items"), Some("bin"), Some("text"), Some("no")],
                &[Some("staff"), Some("id"), Some("integer"), Some("no")],
                &[
                    Some("staff"),
                    Some("manager_id"),
                    Some("integer"),
                    Some("yes"),
                ],
                &[
                    Some("staff"),
                    Some("account_id"),
                    Some("bigint"),
                    Some("no"),
                ],
                &[Some("placeholder"), None, None, None],
            ],
        );
        let unique_keys = result(
            &["table_name", "constraint_name", "column_name", "key_kind"],
            &[
                &[
                    Some("orders"),
                    Some("orders_pkey"),
                    Some("account_id"),
                    Some("primary"),
                ],
                &[
                    Some("orders"),
                    Some("orders_pkey"),
                    Some("number"),
                    Some("primary"),
                ],
                &[
                    Some("order_items"),
                    Some("order_items_pkey"),
                    Some("id"),
                    Some("primary"),
                ],
                &[
                    Some("order_items"),
                    Some("order_items_sku_key"),
                    Some("sku"),
                    Some("unique"),
                ],
                &[
                    Some("order_items"),
                    Some("order_items_place"),
                    Some("sku"),
                    Some("unique"),
                ],
                &[
                    Some("order_items"),
                    Some("order_items_place"),
                    Some("bin"),
                    Some("unique"),
                ],
                &[
                    Some("staff"),
                    Some("staff_pkey"),
                    Some("id"),
                    Some("primary"),
                ],
            ],
        );
        // The composite key's columns arrive in key order, which is not the
        // table's column order, so pairing them by position is the check.
        let foreign_keys = result(
            &[
                "table_name",
                "constraint_name",
                "column_name",
                "referenced_schema",
                "referenced_table",
                "referenced_column",
            ],
            &[
                &[
                    Some("order_items"),
                    Some("order_items_order_fkey"),
                    Some("order_account_id"),
                    Some("public"),
                    Some("orders"),
                    Some("account_id"),
                ],
                &[
                    Some("order_items"),
                    Some("order_items_order_fkey"),
                    Some("order_number"),
                    Some("public"),
                    Some("orders"),
                    Some("number"),
                ],
                &[
                    Some("staff"),
                    Some("staff_account_fkey"),
                    Some("account_id"),
                    Some("billing"),
                    Some("accounts"),
                    Some("id"),
                ],
                &[
                    Some("staff"),
                    Some("staff_manager_fkey"),
                    Some("manager_id"),
                    Some("public"),
                    Some("staff"),
                    Some("id"),
                ],
                &[
                    Some("staff"),
                    Some("staff_ghost_fkey"),
                    Some("id"),
                    Some("public"),
                    Some("created_since"),
                    Some("id"),
                ],
            ],
        );
        assemble_schema_graph("public", &columns, &unique_keys, &foreign_keys).unwrap()
    }

    fn column<'a>(graph: &'a SchemaGraph, table: &str, column: &str) -> &'a GraphColumn {
        let table = graph
            .tables
            .iter()
            .find(|t| t.name.as_ref() == table)
            .unwrap();
        table
            .columns
            .iter()
            .find(|c| c.name.as_ref() == column)
            .unwrap()
    }

    #[test]
    fn a_composite_key_is_one_key_with_its_columns_paired_in_key_order() {
        let graph = graph();
        let key = &graph.keys[0];
        assert_eq!(key.name.as_ref(), "order_items_order_fkey");
        assert_eq!(
            graph.tables[key.table as usize].name.as_ref(),
            "order_items"
        );
        assert_eq!(key.columns, vec![2, 1]);
        assert_eq!(
            key.referenced,
            KeyTarget::Table {
                table: 0,
                columns: vec![0, 1]
            }
        );
    }

    #[test]
    fn a_self_reference_and_a_key_into_another_schema_keep_their_ends() {
        let graph = graph();
        assert_eq!(
            graph.keys[1].referenced,
            KeyTarget::Elsewhere {
                schema: "billing".into(),
                table: "accounts".into()
            }
        );
        assert_eq!(graph.keys[2].table, 2);
        assert_eq!(
            graph.keys[2].referenced,
            KeyTarget::Table {
                table: 2,
                columns: vec![0]
            }
        );
        assert_eq!(graph.keys.len(), 3, "a key into an unread table is dropped");
    }

    #[test]
    fn flags_say_which_columns_are_nullable_keyed_and_unique_on_their_own() {
        let graph = graph();
        let flags = |table, name| {
            let c = column(&graph, table, name);
            (c.nullable, c.primary, c.foreign, c.unique)
        };
        assert_eq!(flags("orders", "account_id"), (false, true, false, false));
        assert_eq!(flags("order_items", "id"), (false, true, false, true));
        assert_eq!(
            flags("order_items", "order_number"),
            (true, false, true, false)
        );
        assert_eq!(flags("order_items", "sku"), (false, false, false, true));
        assert_eq!(flags("order_items", "bin"), (false, false, false, false));
        assert_eq!(flags("staff", "manager_id"), (true, false, true, false));
        assert_eq!(flags("staff", "id"), (false, true, false, true));
    }

    #[test]
    fn types_are_interned_and_a_table_without_columns_is_still_drawn() {
        let graph = graph();
        assert_eq!(graph.types, vec!["bigint", "integer", "text"]);
        assert_eq!(column(&graph, "staff", "account_id").type_label, 0);
        assert_eq!(graph.tables[3].name.as_ref(), "placeholder");
        assert!(graph.tables[3].columns.is_empty());
    }
}

/// What every engine's `live_` graph test asks of the dev seed, which gives
/// each one the same `accounts`, `orders` and `order_items` tables.
#[cfg(test)]
pub(super) mod seed {
    use super::*;

    pub fn table_index(graph: &SchemaGraph, name: &str) -> u32 {
        graph
            .tables
            .iter()
            .position(|table| table.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{name} should be drawn")) as u32
    }

    pub fn column<'a>(graph: &'a SchemaGraph, table: &str, name: &str) -> &'a GraphColumn {
        graph.tables[table_index(graph, table) as usize]
            .columns
            .iter()
            .find(|column| column.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{table}.{name} should be read"))
    }

    fn names(graph: &SchemaGraph, table: u32, columns: &[u32]) -> Vec<String> {
        columns
            .iter()
            .map(|&column| {
                graph.tables[table as usize].columns[column as usize]
                    .name
                    .to_string()
            })
            .collect()
    }

    /// The composite key, the flags on its columns, and that a unique column
    /// is flagged. Case and quoting of names are the engine's own, so the
    /// seed's lower-case ones are asked for as they are.
    pub fn assert_keys_read_back(graph: &SchemaGraph) {
        let items = graph
            .keys
            .iter()
            .find(|key| key.table == table_index(graph, "order_items"))
            .expect("order_items' key should be read");
        assert_eq!(
            names(graph, items.table, &items.columns),
            ["order_account_id", "order_number"]
        );
        let KeyTarget::Table {
            table: orders,
            columns,
        } = &items.referenced
        else {
            panic!("orders is in the same schema: {:?}", items.referenced);
        };
        assert_eq!(*orders, table_index(graph, "orders"));
        assert_eq!(names(graph, *orders, columns), ["account_id", "number"]);

        let account_id = column(graph, "orders", "account_id");
        assert!(account_id.primary && account_id.foreign && !account_id.unique);
        assert!(!account_id.nullable);
        assert!(column(graph, "accounts", "external_id").unique);
    }

    pub fn assert_archive_points_at_accounts(graph: &SchemaGraph, schema: &str) {
        assert_eq!(
            graph.keys[0].referenced,
            KeyTarget::Elsewhere {
                schema: schema.to_string().into(),
                table: "accounts".into()
            }
        );
    }
}

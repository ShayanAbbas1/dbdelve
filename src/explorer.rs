use std::collections::{HashMap, HashSet};

use gpui_component::tree::TreeItem;

use crate::db::{Catalog, Engine, Relation, RelationKind, Routine, RoutineKind, Schema};

/// The row counts a preview can be asked for, and the one it opens with. Every
/// result set is capped (spec §4.3); this is the part of the cap the user gets
/// to move, and the grid shows which one is in effect.
pub const ROW_LIMITS: [usize; 4] = [100, 1_000, 10_000, 100_000];
pub const PREVIEW_ROW_LIMIT: usize = ROW_LIMITS[1];

const RELATION_CATEGORIES: [(RelationKind, &str); 5] = [
    (RelationKind::Table, "Tables"),
    (RelationKind::PartitionedTable, "Partitioned Tables"),
    (RelationKind::View, "Views"),
    (RelationKind::MaterializedView, "Materialized Views"),
    (RelationKind::ForeignTable, "Foreign Tables"),
];

const ROUTINE_CATEGORIES: [(RoutineKind, &str); 2] = [
    (RoutineKind::Function, "Functions"),
    (RoutineKind::Procedure, "Procedures"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplorerTarget {
    Relation {
        schema_index: usize,
        relation_index: usize,
    },
    Routine {
        schema_index: usize,
        routine_index: usize,
    },
}

/// What kind of object a row stands for. The sidebar draws an icon from this;
/// the kind lives here rather than an `IconName` so the tree stays comparable
/// in tests and free of the widget library's types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Relation(RelationKind),
    Routine(RoutineKind),
}

/// What the sidebar knows about one openable row: where it points, and what it
/// is. One map rather than two, so a leaf can never end up with a target and no
/// kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplorerLeaf {
    pub target: ExplorerTarget,
    pub kind: ObjectKind,
}

pub struct ExplorerTree {
    pub items: Vec<TreeItem>,
    pub leaves: HashMap<String, ExplorerLeaf>,
}

pub fn tree(catalog: &Catalog, filter: &str) -> ExplorerTree {
    let filter = filter.trim().to_lowercase();
    let mut leaves = HashMap::new();

    let items = catalog
        .by_name()
        .into_iter()
        .filter_map(|(schema_index, schema)| {
            let schema_matches = matches_filter(&schema.name, &filter);
            let mut groups = Vec::new();

            let partitions = partitions_by_parent(schema);
            let nested: HashSet<usize> = partitions.values().flatten().copied().collect();

            for (kind, label) in RELATION_CATEGORIES {
                let children = schema
                    .relations
                    .iter()
                    .enumerate()
                    // A partition is drawn under its parent instead, whatever
                    // category its own kind would put it in.
                    .filter(|(relation_index, relation)| {
                        relation.kind == kind && !nested.contains(relation_index)
                    })
                    .filter_map(|(relation_index, _)| {
                        relation_item(
                            &mut leaves,
                            schema,
                            schema_index,
                            relation_index,
                            &partitions,
                            &filter,
                            schema_matches,
                        )
                    })
                    .collect::<Vec<_>>();

                if !children.is_empty() {
                    groups.push(category(label, schema_index, children));
                }
            }

            for (kind, label) in ROUTINE_CATEGORIES {
                let children = schema
                    .routines
                    .iter()
                    .enumerate()
                    .filter(|(_, routine)| {
                        routine.kind == kind
                            && (schema_matches || routine_matches(routine, &filter))
                    })
                    .map(|(routine_index, routine)| {
                        let id = format!("routine-{schema_index}-{routine_index}");
                        leaves.insert(
                            id.clone(),
                            ExplorerLeaf {
                                target: ExplorerTarget::Routine {
                                    schema_index,
                                    routine_index,
                                },
                                kind: ObjectKind::Routine(kind),
                            },
                        );
                        TreeItem::new(
                            id,
                            format!("{}({})", routine.name, routine.identity_arguments),
                        )
                    })
                    .collect::<Vec<_>>();

                if !children.is_empty() {
                    groups.push(category(label, schema_index, children));
                }
            }

            if !schema_matches && groups.is_empty() {
                return None;
            }

            Some(
                TreeItem::new(format!("schema-{schema_index}"), schema.name.clone())
                    .expanded(true)
                    .children(groups),
            )
        })
        .collect();

    ExplorerTree { items, leaves }
}

/// Every schema's partitions, by the name of the relation they hang under.
/// Built once per schema rather than searched per relation: a table split a
/// thousand ways is the case this whole feature exists for.
fn partitions_by_parent(schema: &Schema) -> HashMap<&str, Vec<usize>> {
    let names: HashSet<&str> = schema
        .relations
        .iter()
        .map(|relation| relation.name.as_str())
        .collect();
    let mut partitions: HashMap<&str, Vec<usize>> = HashMap::new();

    for (index, relation) in schema.relations.iter().enumerate() {
        // A parent the catalog did not list -- a permission the connecting
        // role lacks is enough -- leaves the child flat rather than nowhere.
        if let Some(parent) = relation
            .partition_of
            .as_deref()
            .filter(|parent| names.contains(parent))
        {
            partitions.entry(parent).or_default().push(index);
        }
    }

    partitions
}

/// One relation row and the partitions under it, recursively: a partition can
/// itself be partitioned, and a sub-partition that fell out of the tree would
/// be a relation the sidebar simply never shows.
///
/// `inherited` is a match already made by something above — the schema, or a
/// parent whose name the filter matched — which is what makes a filter on a
/// partitioned table keep all of its partitions.
fn relation_item(
    leaves: &mut HashMap<String, ExplorerLeaf>,
    schema: &Schema,
    schema_index: usize,
    relation_index: usize,
    partitions: &HashMap<&str, Vec<usize>>,
    filter: &str,
    inherited: bool,
) -> Option<TreeItem> {
    let relation = &schema.relations[relation_index];
    let matched = inherited || relation_matches(relation, filter);

    let children: Vec<TreeItem> = partitions
        .get(relation.name.as_str())
        .into_iter()
        .flatten()
        .filter_map(|&index| {
            relation_item(
                leaves,
                schema,
                schema_index,
                index,
                partitions,
                filter,
                matched,
            )
        })
        .collect();

    if !matched && children.is_empty() {
        return None;
    }

    let id = format!("relation-{schema_index}-{relation_index}");
    leaves.insert(
        id.clone(),
        ExplorerLeaf {
            target: ExplorerTarget::Relation {
                schema_index,
                relation_index,
            },
            kind: ObjectKind::Relation(relation.kind),
        },
    );

    // Collapsed is the point: nesting a hundred partitions only helps if the
    // parent stays one row. The exception is a filter that reached past the
    // parent to match a partition, which has to show what it found.
    Some(
        TreeItem::new(id, relation.name.clone())
            .expanded(!matched)
            .children(children),
    )
}

fn category(label: &'static str, schema_index: usize, children: Vec<TreeItem>) -> TreeItem {
    TreeItem::new(format!("category-{label}-{schema_index}"), label)
        .expanded(true)
        .children(children)
}

pub fn preview_sql(
    engine: Engine,
    schema: &str,
    relation: &str,
    filter: &str,
    limit: usize,
    offset: usize,
) -> String {
    let mut sql = format!("SELECT * FROM {}", engine.qualified(schema, relation));
    // The `WHERE` is emitted here rather than spliced in later: this function
    // owns the `FROM`, so it is the one place that knows where the clause goes.
    // `sql::with_order_by` anchors on the `limit` node, so the sort still lands
    // after the filter without knowing a filter exists.
    let filter = filter.trim();
    if !filter.is_empty() {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    sql.push_str(&format!(" LIMIT {limit}"));
    // `OFFSET` after `LIMIT`: the one order all three engines accept, and the
    // one the statement grammar reads -- it nests `offset` inside the `limit`
    // node, which is what keeps a paged preview sortable. A zero offset is
    // omitted rather than written, so the first page's statement is the
    // statement previews have always run.
    if offset > 0 {
        sql.push_str(&format!(" OFFSET {offset}"));
    }
    sql
}

fn relation_matches(relation: &Relation, filter: &str) -> bool {
    matches_filter(&relation.name, filter)
}

fn routine_matches(routine: &Routine, filter: &str) -> bool {
    matches_filter(&routine.name, filter)
        || matches_filter(&routine.identity_arguments, filter)
        || matches_filter(&routine.result_type, filter)
        || matches_filter(&routine.language, filter)
}

fn matches_filter(value: &str, filter: &str) -> bool {
    filter.is_empty() || value.to_lowercase().contains(filter)
}

#[cfg(test)]
mod tests {
    use crate::db::{Catalog, Relation, RelationKind, Routine, RoutineKind, Schema};

    use super::*;

    fn catalog() -> Catalog {
        Catalog {
            schemas: vec![
                Schema {
                    name: "analytics".into(),
                    relations: vec![Relation {
                        name: "events".into(),
                        kind: RelationKind::Table,
                        partition_of: None,
                    }],
                    routines: Vec::new(),
                },
                Schema {
                    name: "public".into(),
                    relations: vec![
                        Relation {
                            name: "active_accounts".into(),
                            kind: RelationKind::View,
                            partition_of: None,
                        },
                        Relation {
                            name: "accounts".into(),
                            kind: RelationKind::Table,
                            partition_of: None,
                        },
                    ],
                    routines: vec![
                        Routine {
                            name: "reindex".into(),
                            kind: RoutineKind::Procedure,
                            identity_arguments: String::new(),
                            result_type: String::new(),
                            language: "plpgsql".into(),
                            definition: String::new(),
                        },
                        Routine {
                            name: "account_name".into(),
                            kind: RoutineKind::Function,
                            identity_arguments: "account_id bigint".into(),
                            result_type: "text".into(),
                            language: "sql".into(),
                            definition: String::new(),
                        },
                    ],
                },
            ],
        }
    }

    fn partitioned_catalog() -> Catalog {
        let partition = |name: &str, parent: &str, kind| Relation {
            name: name.into(),
            kind,
            partition_of: Some(parent.into()),
        };

        Catalog {
            schemas: vec![Schema {
                name: "public".into(),
                relations: vec![
                    Relation {
                        name: "measurements".into(),
                        kind: RelationKind::PartitionedTable,
                        partition_of: None,
                    },
                    partition("measurements_2025", "measurements", RelationKind::Table),
                    partition(
                        "measurements_2026",
                        "measurements",
                        RelationKind::PartitionedTable,
                    ),
                    partition(
                        "measurements_2026_q1",
                        "measurements_2026",
                        RelationKind::Table,
                    ),
                    Relation {
                        name: "accounts".into(),
                        kind: RelationKind::Table,
                        partition_of: None,
                    },
                ],
                routines: Vec::new(),
            }],
        }
    }

    #[test]
    fn partitions_nest_under_their_parent_instead_of_their_category() {
        let explorer = tree(&partitioned_catalog(), "");
        let categories = &explorer.items[0].children;

        assert_eq!(
            categories
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>(),
            ["Tables", "Partitioned Tables"]
        );
        // The only table left in its own category is the one that is not a
        // partition of anything.
        assert_eq!(
            categories[0]
                .children
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>(),
            ["accounts"]
        );

        let parent = &categories[1].children[0];
        assert_eq!(parent.label, "measurements");
        assert_eq!(
            parent
                .children
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>(),
            ["measurements_2025", "measurements_2026"]
        );
        assert_eq!(parent.children[1].children[0].label, "measurements_2026_q1");

        // A nested row is still openable, and still points at the relation its
        // index names.
        assert_eq!(
            explorer
                .leaves
                .get(parent.children[1].children[0].id.as_ref()),
            Some(&ExplorerLeaf {
                target: ExplorerTarget::Relation {
                    schema_index: 0,
                    relation_index: 3,
                },
                kind: ObjectKind::Relation(RelationKind::Table),
            })
        );
    }

    #[test]
    fn a_catalog_without_partitions_nests_nothing() {
        let explorer = tree(&catalog(), "");

        for schema in &explorer.items {
            for category in &schema.children {
                for object in &category.children {
                    assert!(object.children.is_empty(), "{} nested", object.label);
                }
            }
        }
    }

    #[test]
    fn a_filter_matching_a_partition_surfaces_it_under_its_parent() {
        let explorer = tree(&partitioned_catalog(), "2026_q1");
        let categories = &explorer.items[0].children;

        assert_eq!(categories.len(), 1);
        let parent = &categories[0].children[0];
        assert_eq!(parent.label, "measurements");
        assert_eq!(parent.children[0].label, "measurements_2026");
        assert_eq!(parent.children[0].children[0].label, "measurements_2026_q1");
    }

    #[test]
    fn filter_keeps_the_matching_object_hierarchy() {
        let explorer = tree(&catalog(), "account_name");
        let items = explorer.items;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "public");
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].label, "Functions");
        assert_eq!(
            items[0].children[0].children[0].label,
            "account_name(account_id bigint)"
        );
    }

    #[test]
    fn each_object_kind_gets_its_own_category() {
        let explorer = tree(&catalog(), "public");
        let categories = &explorer.items[0].children;

        assert_eq!(
            categories
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>(),
            ["Tables", "Views", "Functions", "Procedures"]
        );
        assert_eq!(categories[0].children[0].label, "accounts");
        assert_eq!(categories[1].children[0].label, "active_accounts");
        assert_eq!(categories[3].children[0].label, "reindex()");
    }

    #[test]
    fn matching_a_schema_keeps_all_of_its_objects() {
        let explorer = tree(&catalog(), "analytics");
        let items = explorer.items;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "analytics");
        assert_eq!(items[0].children[0].children[0].label, "events");
    }

    #[test]
    fn tree_targets_use_catalog_indices_not_database_names() {
        let explorer = tree(&catalog(), "account_name");

        assert_eq!(
            explorer.leaves.get("routine-1-1"),
            Some(&ExplorerLeaf {
                target: ExplorerTarget::Routine {
                    schema_index: 1,
                    routine_index: 1,
                },
                kind: ObjectKind::Routine(RoutineKind::Function),
            })
        );
    }

    #[test]
    fn a_leaf_carries_the_kind_its_icon_is_drawn_from() {
        let explorer = tree(&catalog(), "public");
        let kind = |id: &str| explorer.leaves.get(id).map(|leaf| leaf.kind);

        // public: relation 0 is a view, relation 1 is a table.
        assert_eq!(
            kind("relation-1-0"),
            Some(ObjectKind::Relation(RelationKind::View))
        );
        assert_eq!(
            kind("relation-1-1"),
            Some(ObjectKind::Relation(RelationKind::Table))
        );
    }

    #[test]
    fn preview_sql_quotes_every_identifier_and_exposes_the_limit() {
        assert_eq!(
            preview_sql(
                Engine::Postgres,
                r#"odd"schema"#,
                r#"table"name"#,
                "",
                PREVIEW_ROW_LIMIT,
                0
            ),
            r#"SELECT * FROM "odd""schema"."table""name" LIMIT 1000"#
        );
    }

    #[test]
    fn a_paged_preview_carries_its_offset_after_the_limit() {
        assert_eq!(
            preview_sql(Engine::Postgres, "public", "accounts", "", 1_000, 2_000),
            r#"SELECT * FROM "public"."accounts" LIMIT 1000 OFFSET 2000"#
        );
    }

    #[test]
    fn a_filter_lands_between_the_relation_and_the_limit() {
        // The one place the WHERE can go: after the FROM this function owns,
        // and ahead of the limit, so the filter picks the rows the page is cut
        // out of rather than being applied to a page already cut.
        assert_eq!(
            preview_sql(
                Engine::Postgres,
                "public",
                "accounts",
                r#""state" = 'ok'"#,
                100,
                0
            ),
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' LIMIT 100"#
        );
        assert_eq!(
            preview_sql(
                Engine::MySql,
                "dbdelve_dev",
                "accounts",
                "`state` = 'ok'",
                100,
                200
            ),
            "SELECT * FROM `dbdelve_dev`.`accounts` WHERE `state` = 'ok' LIMIT 100 OFFSET 200"
        );
        assert_eq!(
            preview_sql(
                Engine::Sqlite,
                "main",
                "accounts",
                r#""state" = 'ok'"#,
                1_000,
                0
            ),
            r#"SELECT * FROM "main"."accounts" WHERE "state" = 'ok' LIMIT 1000"#
        );
    }

    #[test]
    fn no_filter_generates_exactly_what_it_generated_before_there_were_filters() {
        // Byte-identical, per engine, with and without a page offset. Every
        // preview dbdelve has ever run is this statement, and a stray space or a
        // bare WHERE would change what the gate and `with_order_by` read back.
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let qualified = engine.qualified("public", "accounts");
            assert_eq!(
                preview_sql(engine, "public", "accounts", "", 1_000, 0),
                format!("SELECT * FROM {qualified} LIMIT 1000")
            );
            assert_eq!(
                preview_sql(engine, "public", "accounts", "", 1_000, 2_000),
                format!("SELECT * FROM {qualified} LIMIT 1000 OFFSET 2000")
            );
            // Whitespace is not a filter: an input the user emptied by hand
            // must not write `WHERE   ` into the statement.
            assert_eq!(
                preview_sql(engine, "public", "accounts", "   ", 1_000, 0),
                format!("SELECT * FROM {qualified} LIMIT 1000")
            );
        }
    }
}

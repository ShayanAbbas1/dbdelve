use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use gpui_component::tree::TreeItem;

use crate::db::{Catalog, Engine, RelationKind, Routine, RoutineKind, Schema};
use crate::fuzzy::{Fuzzy, Query};
use crate::{mql, redis_cmd};

/// The row counts a preview can be asked for besides none at all, and the one
/// it opens with. The grid shows which one is in effect.
pub const ROW_LIMITS: [usize; 4] = [100, 1_000, 10_000, 100_000];
pub const PREVIEW_ROW_LIMIT: usize = ROW_LIMITS[1];

/// The most rows a limit may ask for: a relation's preview writes it into its
/// `LIMIT`, and Postgres reads that as a `bigint`.
pub const MAX_ROW_LIMIT: usize = i64::MAX as usize;

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
    pub size: Option<u64>,
}

#[derive(Default)]
pub struct ExplorerTree {
    pub items: Vec<TreeItem>,
    pub leaves: HashMap<String, ExplorerLeaf>,
    /// The characters of each row's own name the filter matched, by row id.
    /// A row kept only because its schema or parent matched has none.
    pub highlights: HashMap<String, Vec<Range<usize>>>,
}

/// One filter pass over the catalog: the query parsed once, and what the rows
/// it keeps record as they are built.
struct Builder {
    query: Query,
    fuzzy: Fuzzy,
    lowercase: String,
    leaves: HashMap<String, ExplorerLeaf>,
    highlights: HashMap<String, Vec<Range<usize>>>,
}

impl Builder {
    fn new(filter: &str) -> Self {
        Self {
            query: Query::new(filter),
            fuzzy: Fuzzy::new(),
            lowercase: filter.trim().to_lowercase(),
            leaves: HashMap::new(),
            highlights: HashMap::new(),
        }
    }

    /// Whether the row `id`'s own name matches, noting which characters did.
    fn name_matches(&mut self, id: &str, name: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        if !self.fuzzy.matches(&self.query, name) {
            return false;
        }
        let ranges = self.fuzzy.highlights(&self.query, name);
        if !ranges.is_empty() {
            self.highlights.insert(id.to_string(), ranges);
        }
        true
    }

    // Substring, not fuzzy: these are searched for a word like `bigint` or
    // `plpgsql`, and as a subsequence a type signature matches nearly anything.
    fn signature_matches(&self, routine: &Routine) -> bool {
        [
            &routine.identity_arguments,
            &routine.result_type,
            &routine.language,
        ]
        .into_iter()
        .any(|field| field.to_lowercase().contains(&self.lowercase))
    }
}

pub fn tree(catalog: &Catalog, filter: &str) -> ExplorerTree {
    let mut builder = Builder::new(filter);

    let items = catalog
        .by_name()
        .into_iter()
        .filter_map(|(schema_index, schema)| {
            let id = format!("schema-{schema_index}");
            let schema_matches = builder.name_matches(&id, &schema.name);
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
                            &mut builder,
                            schema,
                            schema_index,
                            relation_index,
                            &partitions,
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
                    .filter(|(_, routine)| routine.kind == kind)
                    .filter_map(|(routine_index, routine)| {
                        let id = format!("routine-{schema_index}-{routine_index}");
                        let name_matches = builder.name_matches(&id, &routine.name);
                        if !(schema_matches || name_matches || builder.signature_matches(routine)) {
                            return None;
                        }
                        builder.leaves.insert(
                            id.clone(),
                            ExplorerLeaf {
                                target: ExplorerTarget::Routine {
                                    schema_index,
                                    routine_index,
                                },
                                kind: ObjectKind::Routine(kind),
                                size: None,
                            },
                        );
                        Some(TreeItem::new(
                            id,
                            format!("{}({})", routine.name, routine.identity_arguments),
                        ))
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
                TreeItem::new(id, schema.name.clone())
                    .expanded(true)
                    .children(groups),
            )
        })
        .collect();

    ExplorerTree {
        items,
        leaves: builder.leaves,
        highlights: builder.highlights,
    }
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
    builder: &mut Builder,
    schema: &Schema,
    schema_index: usize,
    relation_index: usize,
    partitions: &HashMap<&str, Vec<usize>>,
    inherited: bool,
) -> Option<TreeItem> {
    let relation = &schema.relations[relation_index];
    let id = format!("relation-{schema_index}-{relation_index}");
    let matched = builder.name_matches(&id, &relation.name) || inherited;

    let children: Vec<TreeItem> = partitions
        .get(relation.name.as_str())
        .into_iter()
        .flatten()
        .filter_map(|&index| {
            relation_item(builder, schema, schema_index, index, partitions, matched)
        })
        .collect();

    if !matched && children.is_empty() {
        return None;
    }

    builder.leaves.insert(
        id.clone(),
        ExplorerLeaf {
            target: ExplorerTarget::Relation {
                schema_index,
                relation_index,
            },
            kind: ObjectKind::Relation(relation.kind),
            size: relation.size,
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
    limit: Option<usize>,
    offset: usize,
) -> String {
    match engine {
        Engine::MongoDb => return mql::browse::find_preview(relation, filter, limit, offset),
        // No limit or offset to write: see `Engine::pages`.
        Engine::Redis => return redis_cmd::browse::preview(filter),
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Sqlite
        | Engine::Turso
        | Engine::Snowflake
        | Engine::SqlServer => {}
    }
    let mut sql = format!("SELECT *{}", from_where(engine, schema, relation, filter));
    let Some(limit) = limit else {
        return sql;
    };
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

/// `None` where no command reads a hundred rows: a Redis `SCAN` has no limit.
pub fn select_top_sql(engine: Engine, schema: &str, relation: &str) -> Option<String> {
    Some(match engine {
        Engine::SqlServer => format!(
            "SELECT TOP 100 * FROM {}",
            engine.qualified(schema, relation)
        ),
        Engine::MongoDb => mql::browse::find_preview(relation, "", Some(100), 0),
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Sqlite
        | Engine::Turso
        | Engine::Snowflake => preview_sql(engine, schema, relation, "", Some(100), 0),
        Engine::Redis => return None,
    })
}

/// `None` where the relation cannot be dropped apart from its database: a
/// Redis keyspace is the database's own.
pub fn drop_sql(
    engine: Engine,
    schema: &str,
    relation: &str,
    kind: RelationKind,
) -> Option<String> {
    let keyword = match (engine, kind) {
        (Engine::MongoDb, _) => return Some(format!("{}.drop()", mql::browse::handle(relation))),
        (Engine::Redis, _) => return None,
        (_, RelationKind::Table | RelationKind::PartitionedTable) => "TABLE",
        (_, RelationKind::View) => "VIEW",
        (_, RelationKind::MaterializedView) => "MATERIALIZED VIEW",
        (Engine::Snowflake, RelationKind::ForeignTable) => "EXTERNAL TABLE",
        (_, RelationKind::ForeignTable) => "FOREIGN TABLE",
    };
    Some(format!(
        "DROP {keyword} {};",
        engine.qualified(schema, relation)
    ))
}

pub fn truncate_sql(engine: Engine, schema: &str, relation: &str) -> String {
    let qualified = engine.qualified(schema, relation);
    match engine {
        Engine::MongoDb => format!("{}.deleteMany({{}})", mql::browse::handle(relation)),
        Engine::Redis => "FLUSHDB".into(),
        // SQLite has no TRUNCATE.
        Engine::Sqlite | Engine::Turso => format!("DELETE FROM {qualified};"),
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Snowflake
        | Engine::SqlServer => format!("TRUNCATE TABLE {qualified};"),
    }
}

/// The command a preview's row opens with, as a result of its own
/// ([`Engine::opened_column`]), read off the row's cells by column name: a
/// Redis key's first `rows` entries, by the read its type takes. `None` where
/// rows open nothing, and for a type with no read.
pub fn open_row_sql<'a>(
    engine: Engine,
    rows: usize,
    cell: impl Fn(&str) -> Option<&'a str>,
) -> Option<String> {
    match engine {
        Engine::Redis => redis_cmd::browse::read(cell("key")?, cell("type")?, rows),
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Sqlite
        | Engine::Turso
        | Engine::Snowflake
        | Engine::SqlServer
        | Engine::MongoDb => None,
    }
}

/// Whether a row matches `filter`, for the reference arrow: a constant rather
/// than `*`, so no column's value is read or sent back to answer it.
pub fn probe_sql(engine: Engine, schema: &str, relation: &str, filter: &str) -> String {
    format!(
        "SELECT 1{} LIMIT 1",
        from_where(engine, schema, relation, filter)
    )
}

/// The size of what `preview_sql` would page through, for the status bar's
/// Count. Never run unasked: on a large table it is a full scan.
pub fn count_sql(engine: Engine, schema: &str, relation: &str, filter: &str) -> String {
    match engine {
        Engine::MongoDb => return mql::browse::count_documents(relation, filter),
        Engine::Redis => return redis_cmd::browse::count(filter),
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Sqlite
        | Engine::Turso
        | Engine::Snowflake
        | Engine::SqlServer => {}
    }
    format!(
        "SELECT {}{}",
        engine.count_all(),
        from_where(engine, schema, relation, filter)
    )
}

/// The `WHERE` is emitted here rather than spliced in later: this is the one
/// place that knows where the clause goes. `sql::with_order_by` anchors on the
/// `limit` node, so the sort still lands after the filter without knowing a
/// filter exists.
fn from_where(engine: Engine, schema: &str, relation: &str, filter: &str) -> String {
    let mut sql = format!(" FROM {}", engine.qualified(schema, relation));
    let filter = filter.trim();
    if !filter.is_empty() {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    sql
}

#[cfg(test)]
mod tests {
    use crate::db::{Catalog, Relation, RelationKind, Routine, RoutineKind, Schema};

    use super::*;

    #[test]
    fn a_count_counts_what_the_preview_pages_through_and_passes_both_gates() {
        for (engine, aggregate) in [
            (Engine::Postgres, "COUNT(*)"),
            (Engine::MySql, "COUNT(*)"),
            (Engine::Sqlite, "COUNT(*)"),
            (Engine::Snowflake, "COUNT(*)"),
            // An `int` past 2^31 rows anywhere else.
            (Engine::SqlServer, "COUNT_BIG(*)"),
        ] {
            let all = count_sql(engine, "public", "orders", "");
            assert_eq!(
                all,
                format!(
                    "SELECT {aggregate} FROM {}",
                    engine.qualified("public", "orders")
                )
            );
            let narrowed = count_sql(engine, "public", "orders", " id > 3 ");
            assert!(narrowed.ends_with(" WHERE id > 3"));
            for sql in [&all, &narrowed] {
                assert!(crate::sql::is_generated_select(engine, sql), "{sql}");
                // Runnable in Read-only without a prompt: the Count button
                // refuses rather than asks.
                let verdict = crate::sql::classify(engine, sql);
                assert!(
                    crate::sql::gate(&verdict, crate::sql::Mode::ReadOnly, &[]).is_none(),
                    "{sql}"
                );
            }
        }

        let keys = count_sql(Engine::Redis, "db0", "keys", "");
        assert_eq!(keys, "DBSIZE");
        assert!(crate::sql::is_generated_select(Engine::Redis, &keys));
        let verdict = crate::sql::classify(Engine::Redis, &keys);
        assert!(crate::sql::gate(&verdict, crate::sql::Mode::ReadOnly, &[]).is_none());
        // `DBSIZE` counts every key, so a filtered count is refused, not wrong.
        let filtered = count_sql(Engine::Redis, "db0", "keys", "MATCH user:*");
        assert!(!crate::sql::is_generated_select(Engine::Redis, &filtered));
    }

    #[test]
    fn a_probe_passes_the_select_gate_and_pages_on_every_engine() {
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::Sqlite,
            Engine::Snowflake,
            Engine::SqlServer,
        ] {
            let probe = probe_sql(engine, "public", "orders", "account_id = 7");
            assert!(crate::sql::is_generated_select(engine, &probe), "{probe}");
            let paged = crate::sql::paged(engine, &probe, &[]).expect("a probe has a limit");
            assert!(paged.starts_with("SELECT 1 FROM "), "{paged}");
        }
        assert!(
            crate::sql::paged(
                Engine::SqlServer,
                &probe_sql(Engine::SqlServer, "dbo", "orders", ""),
                &[]
            )
            .unwrap()
            .ends_with(" ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 1 ROWS ONLY")
        );
    }

    fn catalog() -> Catalog {
        Catalog {
            schemas: vec![
                Schema {
                    name: "analytics".into(),
                    relations: vec![Relation {
                        name: "events".into(),
                        kind: RelationKind::Table,
                        partition_of: None,
                        size: None,
                        rows: None,
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
                            size: None,
                            rows: None,
                        },
                        Relation {
                            name: "accounts".into(),
                            kind: RelationKind::Table,
                            partition_of: None,
                            size: None,
                            rows: None,
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
            size: None,
            rows: None,
        };

        Catalog {
            schemas: vec![Schema {
                name: "public".into(),
                relations: vec![
                    Relation {
                        name: "measurements".into(),
                        kind: RelationKind::PartitionedTable,
                        partition_of: None,
                        size: None,
                        rows: None,
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
                        size: None,
                        rows: None,
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
                size: None,
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

    fn marked(explorer: &ExplorerTree, id: &str, label: &str) -> Option<String> {
        let ranges = explorer.highlights.get(id)?;
        Some(ranges.iter().map(|range| &label[range.clone()]).collect())
    }

    #[test]
    fn a_subsequence_of_a_name_finds_it_and_marks_the_letters_it_matched() {
        let explorer = tree(&catalog(), "acct");
        let categories = &explorer.items[0].children;

        assert_eq!(explorer.items.len(), 1);
        assert_eq!(categories[0].children[0].label, "accounts");
        assert_eq!(
            marked(&explorer, "relation-1-1", "accounts").as_deref(),
            Some("acct")
        );
        // A routine's row reads `name(args)`, and only the name was matched.
        let label = "account_name(account_id bigint)";
        assert_eq!(categories[2].children[0].label, label);
        assert_eq!(
            marked(&explorer, "routine-1-1", label).as_deref(),
            Some("acct")
        );
        assert!(
            explorer.highlights["routine-1-1"]
                .iter()
                .all(|range| range.end <= "account_name".len())
        );
    }

    #[test]
    fn a_routine_signature_matches_as_a_substring_and_is_not_marked() {
        let by_argument = tree(&catalog(), "BIGINT");
        assert_eq!(
            by_argument.items[0].children[0].children[0].label,
            "account_name(account_id bigint)"
        );
        assert!(by_argument.highlights.is_empty());

        let by_language = tree(&catalog(), "plpgsql");
        assert_eq!(
            by_language.items[0].children[0].children[0].label,
            "reindex()"
        );

        // A subsequence of `account_id bigint`, which no name holds.
        assert!(tree(&catalog(), "acbt").items.is_empty());
    }

    #[test]
    fn only_a_row_whose_own_name_matched_is_marked() {
        let explorer = tree(&catalog(), "analytics");

        assert_eq!(
            marked(&explorer, "schema-0", "analytics").as_deref(),
            Some("analytics")
        );
        // Kept because its schema matched, not because of its name.
        assert!(explorer.leaves.contains_key("relation-0-0"));
        assert!(!explorer.highlights.contains_key("relation-0-0"));

        assert!(tree(&catalog(), "").highlights.is_empty());
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
                size: None,
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
    fn copied_statements_are_spelled_for_the_engine() {
        assert_eq!(
            select_top_sql(Engine::SqlServer, "dbo", "t").unwrap(),
            r#"SELECT TOP 100 * FROM "dbo"."t""#
        );
        assert_eq!(
            select_top_sql(Engine::MySql, "db", "t").unwrap(),
            "SELECT * FROM `db`.`t` LIMIT 100"
        );
        assert_eq!(
            drop_sql(Engine::Postgres, "public", "v", RelationKind::View).unwrap(),
            r#"DROP VIEW "public"."v";"#
        );
        assert_eq!(
            drop_sql(Engine::Snowflake, "S", "e", RelationKind::ForeignTable).unwrap(),
            r#"DROP EXTERNAL TABLE "S"."e";"#
        );
        assert_eq!(
            truncate_sql(Engine::Sqlite, "main", "t"),
            r#"DELETE FROM "main"."t";"#
        );
        assert_eq!(
            drop_sql(Engine::MongoDb, "db", "c", RelationKind::Table).unwrap(),
            r#"db.getCollection("c").drop()"#
        );
        assert_eq!(
            truncate_sql(Engine::MongoDb, "db", "c"),
            r#"db.getCollection("c").deleteMany({})"#
        );
    }

    #[test]
    fn a_redis_row_opens_its_key_and_no_other_engines_row_opens_anything() {
        let row = |column: &str| match column {
            "key" => Some("queue:emails"),
            "type" => Some("list"),
            _ => None,
        };
        for engine in Engine::ALL {
            let opened = open_row_sql(engine, PREVIEW_ROW_LIMIT, row);
            assert_eq!(opened.is_some(), engine.opened_column().is_some());
        }
        assert_eq!(
            open_row_sql(Engine::Redis, PREVIEW_ROW_LIMIT, row).as_deref(),
            Some("LRANGE queue:emails 0 999")
        );
        let read = open_row_sql(Engine::Redis, PREVIEW_ROW_LIMIT, row).unwrap();
        let verdict = crate::sql::classify(Engine::Redis, &read);
        assert!(crate::sql::gate(&verdict, crate::sql::Mode::ReadOnly, &[]).is_none());
    }

    #[test]
    fn preview_sql_quotes_every_identifier_and_exposes_the_limit() {
        assert_eq!(
            preview_sql(
                Engine::Postgres,
                r#"odd"schema"#,
                r#"table"name"#,
                "",
                Some(PREVIEW_ROW_LIMIT),
                0
            ),
            r#"SELECT * FROM "odd""schema"."table""name" LIMIT 1000"#
        );
    }

    #[test]
    fn a_preview_with_no_limit_asks_for_every_row() {
        for engine in Engine::ALL {
            let sql = preview_sql(engine, "public", "accounts", "", None, 0);
            assert!(!sql.contains("LIMIT") && !sql.contains("limit("), "{sql}");
            assert!(crate::sql::is_generated_select(engine, &sql), "{sql}");
            assert_eq!(
                crate::sql::paged(engine, &sql, &["id".into()]).as_deref(),
                Some(sql.as_str()),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn a_paged_preview_carries_its_offset_after_the_limit() {
        assert_eq!(
            preview_sql(
                Engine::Postgres,
                "public",
                "accounts",
                "",
                Some(1_000),
                2_000
            ),
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
                Some(100),
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
                Some(100),
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
                Some(1_000),
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
                preview_sql(engine, "public", "accounts", "", Some(1_000), 0),
                format!("SELECT * FROM {qualified} LIMIT 1000")
            );
            assert_eq!(
                preview_sql(engine, "public", "accounts", "", Some(1_000), 2_000),
                format!("SELECT * FROM {qualified} LIMIT 1000 OFFSET 2000")
            );
            // Whitespace is not a filter: an input the user emptied by hand
            // must not write `WHERE   ` into the statement.
            assert_eq!(
                preview_sql(engine, "public", "accounts", "   ", Some(1_000), 0),
                format!("SELECT * FROM {qualified} LIMIT 1000")
            );
        }
    }
}

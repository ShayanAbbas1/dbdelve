use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use gpui_component::tree::TreeItem;

use crate::db::{
    Catalog, Engine, EngineSpec, ForeignTable, MaterializedViewDrop, RelationKind, Routine,
    RoutineKind, Schema, SelectTop, Syntax, Truncate,
};
use crate::fuzzy::{Fuzzy, Query};
use crate::{mql, redis_cmd};

/// The row counts a preview can be asked for besides none at all, and the one
/// it opens with. The grid shows which one is in effect.
pub const ROW_LIMITS: [usize; 4] = [100, 1_000, 10_000, 100_000];
pub const PREVIEW_ROW_LIMIT: usize = ROW_LIMITS[1];

/// The most rows a limit may ask for: a relation's preview writes it into its
/// `LIMIT`, and Postgres reads that as a `bigint`.
pub const MAX_ROW_LIMIT: usize = i64::MAX as usize;

const RELATION_CATEGORIES: [(RelationKind, &str); 6] = [
    (RelationKind::Table, "Tables"),
    (RelationKind::PartitionedTable, "Partitioned Tables"),
    (RelationKind::View, "Views"),
    (RelationKind::MaterializedView, "Materialized Views"),
    (RelationKind::ForeignTable, "Foreign Tables"),
    (RelationKind::Dictionary, "Dictionaries"),
];

const ROUTINE_CATEGORIES: [(RoutineKind, &str); 2] = [
    (RoutineKind::Function, "Functions"),
    (RoutineKind::Procedure, "Procedures"),
];

/// The id of the sidebar's Queries group, the profile's saved queries listed
/// above its schemas.
pub const QUERIES_GROUP: &str = "queries";

/// What a saved query's row id starts with. Its name follows: query names are
/// file names, so no other row's id can take this shape.
const QUERY_ROW: &str = "query:";

/// Whether a row is a schema's own: the only row "Set as Active Schema"
/// means anything on. Asked by id, since a schema's label can be anything,
/// "Queries" included.
pub fn is_schema_row(id: &str) -> bool {
    id.starts_with(SCHEMA_ROW)
}

const SCHEMA_ROW: &str = "schema-";

/// Whether each folder in `items` is open, by row id, to put back on a
/// rebuild with [`with_folds`].
pub fn folds(items: &[TreeItem]) -> HashMap<String, bool> {
    let mut folds = HashMap::new();
    let mut stack: Vec<&TreeItem> = items.iter().collect();
    while let Some(item) = stack.pop() {
        if item.is_folder() {
            folds.insert(item.id.to_string(), item.is_expanded());
        }
        stack.extend(&item.children);
    }
    folds
}

/// `items` with every folder `folds` names opened or closed as it says. A
/// row it does not name keeps the state it was built with.
pub fn with_folds(items: Vec<TreeItem>, folds: &HashMap<String, bool>) -> Vec<TreeItem> {
    items
        .into_iter()
        .map(|mut item| {
            item.children = with_folds(std::mem::take(&mut item.children), folds);
            match folds.get(item.id.as_ref()) {
                Some(&expanded) => item.expanded(expanded),
                None => item,
            }
        })
        .collect()
}

/// The saved query a row stands for, by its row id.
pub fn saved_query(id: &str) -> Option<&str> {
    id.strip_prefix(QUERY_ROW)
}

/// The row id of a saved query, the inverse of [`saved_query`].
pub fn query_row(name: &str) -> String {
    format!("{QUERY_ROW}{name}")
}

/// The row id of a relation. By name and not by place, so that a row is the
/// same row after a schema refresh has moved the catalog around it.
pub fn relation_row(schema: &str, relation: &str) -> String {
    format!("relation-{schema}\0{relation}")
}

fn routine_row(schema: &str, routine: &Routine) -> String {
    routine_id(routine.kind, schema, &crate::session::routine_name(routine))
}

/// `name` is a routine's name and arguments, as its tab is named.
fn routine_id(kind: RoutineKind, schema: &str, name: &str) -> String {
    format!("routine-{kind:?}-{schema}\0{name}")
}

/// The row id of the object a tab shows. A routine is found by its name and
/// arguments, as its tab is named: the name alone does not tell overloads
/// apart, and a relation of the same name is another object entirely.
pub fn object_row(catalog: &Catalog, schema: &str, name: &str, kind: ObjectKind) -> Option<String> {
    let schema = catalog
        .schemas
        .iter()
        .find(|candidate| candidate.name == schema)?;
    match kind {
        ObjectKind::Relation(_) => schema
            .relations
            .iter()
            .find(|relation| relation.name == name)
            .map(|relation| relation_row(&schema.name, &relation.name)),
        ObjectKind::Routine(kind) => schema
            .routines
            .iter()
            .find(|routine| routine.kind == kind && crate::session::routine_name(routine) == name)
            .map(|routine| routine_row(&schema.name, routine)),
    }
}

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

impl ObjectKind {
    /// How a shortcut stores the kind: by name, so a kind this build lacks
    /// costs that one shortcut rather than the profile it is in.
    pub fn slug(self) -> &'static str {
        match self {
            ObjectKind::Relation(RelationKind::Table) => "table",
            ObjectKind::Relation(RelationKind::PartitionedTable) => "partitioned_table",
            ObjectKind::Relation(RelationKind::View) => "view",
            ObjectKind::Relation(RelationKind::MaterializedView) => "materialized_view",
            ObjectKind::Relation(RelationKind::ForeignTable) => "foreign_table",
            ObjectKind::Relation(RelationKind::Dictionary) => "dictionary",
            ObjectKind::Routine(RoutineKind::Function) => "function",
            ObjectKind::Routine(RoutineKind::Procedure) => "procedure",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        RELATION_CATEGORIES
            .map(|(kind, _)| ObjectKind::Relation(kind))
            .into_iter()
            .chain(ROUTINE_CATEGORIES.map(|(kind, _)| ObjectKind::Routine(kind)))
            .find(|kind| kind.slug() == slug)
    }
}

#[derive(Default)]
pub struct ExplorerTree {
    pub items: Vec<TreeItem>,
    pub leaves: HashMap<String, ExplorerLeaf>,
    /// The characters of each row's own name the filter matched, by row id.
    /// A row kept only because its schema or parent matched has none.
    pub highlights: HashMap<String, Vec<Range<usize>>>,
    /// What each row in a folder or the Pinned group stands for, by row id.
    pub shortcuts: HashMap<String, Shortcut>,
}

impl ExplorerTree {
    /// `other`'s rows after this one's.
    pub fn extend(&mut self, other: ExplorerTree) {
        self.items.extend(other.items);
        self.leaves.extend(other.leaves);
        self.highlights.extend(other.highlights);
        self.shortcuts.extend(other.shortcuts);
    }
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
            let id = format!("{SCHEMA_ROW}{}", schema.name);
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
                    groups.push(category(label, &schema.name, children));
                }
            }

            for (kind, label) in ROUTINE_CATEGORIES {
                let children = schema
                    .routines
                    .iter()
                    .enumerate()
                    .filter(|(_, routine)| routine.kind == kind)
                    .filter_map(|(routine_index, routine)| {
                        let id = routine_row(&schema.name, routine);
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
                    groups.push(category(label, &schema.name, children));
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
        shortcuts: HashMap::new(),
    }
}

/// The Queries group, holding every saved query whose name the filter keeps,
/// or no row at all when it keeps none. Only the names are matched: a filter
/// that matched the group's own label would keep every query for typing `q`.
pub fn queries(names: &[String], filter: &str) -> ExplorerTree {
    let mut builder = Builder::new(filter);
    let children: Vec<TreeItem> = names
        .iter()
        .filter_map(|name| {
            let id = query_row(name);
            builder
                .name_matches(&id, name)
                .then(|| TreeItem::new(id, name.clone()))
        })
        .collect();
    let items = match children.is_empty() {
        true => Vec::new(),
        false => vec![
            TreeItem::new(QUERIES_GROUP, "Queries")
                .expanded(true)
                .children(children),
        ],
    };
    ExplorerTree {
        items,
        leaves: HashMap::new(),
        highlights: builder.highlights,
        shortcuts: HashMap::new(),
    }
}

/// The id of the sidebar's Pinned group, listed above everything else.
pub const PINNED_GROUP: &str = "pinned";

/// A folder's row id. The name follows its length, so no name can run on
/// into the id of a row under it: a folder is called whatever was typed, `/`
/// included.
pub fn folder_row(name: &str) -> String {
    format!("folder:{}:{name}", name.len())
}

/// The id of a row listed under `parent`. A shortcut repeats a row listed
/// elsewhere, and an id may be in the tree only once.
fn nested_row(parent: &str, row: &str) -> String {
    format!("{parent}/{row}")
}

/// What a folder or the Pinned group lists: a shortcut, so the row it
/// repeats stays where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Member {
    /// A saved query, by name alone: queries are the profile's, not a
    /// database's.
    Query(String),
    Object(ObjectRef),
}

impl Member {
    /// The id of the row it repeats, in the Queries group or the schemas.
    fn row(&self) -> String {
        match self {
            Member::Query(name) => query_row(name),
            Member::Object(object) => object.row(),
        }
    }

    /// Its id under a folder or the Pinned group: everything it is named by,
    /// so no two members are one row. The database's length keeps a name
    /// with a `/` from running into the rest.
    fn id(&self) -> String {
        match self {
            Member::Query(name) => query_row(name),
            Member::Object(object) => {
                let database = object
                    .database
                    .as_deref()
                    .map_or_else(String::new, |database| {
                        format!("database:{}:{database}/", database.len())
                    });
                format!("{database}{}-{}", object.kind.slug(), object.row())
            }
        }
    }

    /// Whether `other` names the same thing. Not `==`: a relation whose kind
    /// changed is still the same relation.
    pub fn is(&self, other: &Member) -> bool {
        let database = |member: &Member| match member {
            Member::Query(_) => None,
            Member::Object(object) => object.database.clone(),
        };
        self.row() == other.row() && database(self) == database(other)
    }
}

/// A database object by name, as a restored tab is, so a shortcut outlives
/// the catalog it was made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRef {
    /// Where the catalog is one database's (`Engine::catalog_is_per_database`),
    /// the database it is in: `public.users` is in every Postgres database.
    /// `None` where every database is a schema of the catalog already.
    pub database: Option<String>,
    pub schema: String,
    /// A routine's name and arguments, as its row and its tab are named.
    pub name: String,
    pub kind: ObjectKind,
}

/// The database a shortcut names its objects by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortcutDatabase {
    /// None: the catalog lists every database as a schema.
    Unnamed,
    /// The one the catalog is of.
    On(String),
    /// The catalog is one database's and which is not known yet. Nothing is
    /// matched or made then: a shortcut made without its database would open
    /// the same schema and name in whatever database came next.
    Unknown,
}

impl ObjectRef {
    fn row(&self) -> String {
        match self.kind {
            ObjectKind::Relation(_) => relation_row(&self.schema, &self.name),
            ObjectKind::Routine(kind) => routine_id(kind, &self.schema, &self.name),
        }
    }

    /// Its row in `catalog`, or `None` for one the catalog does not list.
    fn leaf(&self, catalog: &Catalog) -> Option<ExplorerLeaf> {
        let (schema_index, schema) = catalog
            .schemas
            .iter()
            .enumerate()
            .find(|(_, schema)| schema.name == self.schema)?;
        match self.kind {
            ObjectKind::Relation(_) => schema
                .relations
                .iter()
                .position(|relation| relation.name == self.name)
                .map(|relation_index| {
                    let relation = &schema.relations[relation_index];
                    ExplorerLeaf {
                        target: ExplorerTarget::Relation {
                            schema_index,
                            relation_index,
                        },
                        kind: ObjectKind::Relation(relation.kind),
                        size: relation.size,
                    }
                }),
            ObjectKind::Routine(kind) => schema
                .routines
                .iter()
                .position(|routine| {
                    routine.kind == kind && crate::session::routine_name(routine) == self.name
                })
                .map(|routine_index| ExplorerLeaf {
                    target: ExplorerTarget::Routine {
                        schema_index,
                        routine_index,
                    },
                    kind: self.kind,
                    size: None,
                }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pin {
    Member(Member),
    /// A folder, by name. Pinned, it is listed in the Pinned group instead of
    /// among the folders.
    Folder(String),
}

impl Pin {
    fn is(&self, other: &Pin) -> bool {
        match (self, other) {
            (Pin::Member(member), Pin::Member(other)) => member.is(other),
            (Pin::Folder(name), Pin::Folder(other)) => name == other,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    /// In the order they were added.
    pub members: Vec<Member>,
}

/// A connection's folders and pins. Shortcuts to rows the explorer lists
/// anyway, so deleting a folder loses nothing but the grouping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shortcuts {
    pub folders: Vec<Folder>,
    /// In the order they were pinned.
    pub pins: Vec<Pin>,
}

impl Shortcuts {
    pub fn folder(&self, name: &str) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.name == name)
    }

    pub fn holds(&self, folder: &str, member: &Member) -> bool {
        self.folder(folder)
            .is_some_and(|folder| folder.members.iter().any(|held| held.is(member)))
    }

    /// Puts `member` in `folder`, or takes it out if it is in already.
    pub fn toggle(&mut self, folder: &str, member: Member) {
        let Some(folder) = self.folders.iter_mut().find(|held| held.name == folder) else {
            return;
        };
        match folder.members.iter().position(|held| held.is(&member)) {
            Some(at) => _ = folder.members.remove(at),
            None => folder.members.push(member),
        }
    }

    pub fn pinned(&self, pin: &Pin) -> bool {
        self.pins.iter().any(|held| held.is(pin))
    }

    pub fn toggle_pin(&mut self, pin: Pin) {
        match self.pins.iter().position(|held| held.is(&pin)) {
            Some(at) => _ = self.pins.remove(at),
            None => self.pins.push(pin),
        }
    }

    /// A name no folder has, for one made before it is named.
    pub fn unused_name(&self) -> String {
        (1..)
            .map(|n| match n {
                1 => "New Folder".to_string(),
                n => format!("New Folder {n}"),
            })
            .find(|name| folder_name_refusal(name, None, &self.folders).is_none())
            .expect("the names run out before the folders do")
    }

    pub fn add_folder(&mut self, name: &str) -> Result<(), String> {
        if let Some(refusal) = folder_name_refusal(name, None, &self.folders) {
            return Err(refusal);
        }
        self.folders.push(Folder {
            name: name.to_string(),
            members: Vec::new(),
        });
        Ok(())
    }

    /// Renames a folder, and its pin with it.
    pub fn rename_folder(&mut self, from: &str, to: &str) -> Result<(), String> {
        if let Some(refusal) = folder_name_refusal(to, Some(from), &self.folders) {
            return Err(refusal);
        }
        for folder in self.folders.iter_mut().filter(|folder| folder.name == from) {
            folder.name = to.to_string();
        }
        for pin in &mut self.pins {
            if let Pin::Folder(name) = pin
                && name == from
            {
                *name = to.to_string();
            }
        }
        Ok(())
    }

    pub fn delete_folder(&mut self, name: &str) {
        self.folders.retain(|folder| folder.name != name);
        self.pins
            .retain(|pin| !matches!(pin, Pin::Folder(held) if held == name));
    }

    /// Follows a saved query to its new name, wherever it is listed.
    pub fn rename_query(&mut self, from: &str, to: &str) {
        let pinned = self.pins.iter_mut().filter_map(|pin| match pin {
            Pin::Member(member) => Some(member),
            Pin::Folder(_) => None,
        });
        for member in self
            .folders
            .iter_mut()
            .flat_map(|folder| &mut folder.members)
            .chain(pinned)
        {
            if let Member::Query(name) = member
                && name == from
            {
                *name = to.to_string();
            }
        }
    }

    /// Takes a deleted saved query out of every folder and the pins.
    pub fn forget_query(&mut self, name: &str) {
        let gone = Member::Query(name.to_string());
        for folder in &mut self.folders {
            folder.members.retain(|member| !member.is(&gone));
        }
        self.pins.retain(|pin| !pin.is(&Pin::Member(gone.clone())));
    }

    /// What a duplicate of the connection starts with: everything but the
    /// saved queries, which a duplicate does not copy.
    pub fn duplicated(&self) -> Self {
        let object = |member: &Member| matches!(member, Member::Object(_));
        Self {
            folders: self
                .folders
                .iter()
                .map(|folder| Folder {
                    name: folder.name.clone(),
                    members: folder
                        .members
                        .iter()
                        .filter(|m| object(m))
                        .cloned()
                        .collect(),
                })
                .collect(),
            pins: self
                .pins
                .iter()
                .filter(|pin| !matches!(pin, Pin::Member(member) if !object(member)))
                .cloned()
                .collect(),
        }
    }
}

/// Why `name` cannot be a folder's, if it cannot. `keeping` is the folder's
/// own name on a rename, which it may keep in another case.
pub fn folder_name_refusal(
    name: &str,
    keeping: Option<&str>,
    folders: &[Folder],
) -> Option<String> {
    if name.is_empty() {
        return Some("A folder needs a name.".to_string());
    }
    // Any case, as saved queries are: two folders that differ only by case
    // sort as one and read as one.
    folders
        .iter()
        .find(|folder| {
            Some(folder.name.as_str()) != keeping
                && folder.name.to_lowercase() == name.to_lowercase()
        })
        .map(|folder| format!("A folder named {} already exists.", folder.name))
}

/// What a row in a folder or the Pinned group stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Folder(String),
    Member {
        member: Member,
        /// The folder it is listed in, or `None` for a pin.
        folder: Option<String>,
        /// In neither the catalog nor the saved queries: dropped, renamed, or
        /// out of the connecting role's sight. Kept, and drawn greyed, since
        /// it may well come back.
        missing: bool,
    },
}

impl Shortcut {
    /// The saved query the row opens, which a missing one does not.
    pub fn query(&self) -> Option<&str> {
        match self {
            Shortcut::Member {
                member: Member::Query(name),
                missing: false,
                ..
            } => Some(name),
            _ => None,
        }
    }
}

/// The Pinned group, when anything is pinned, then every folder not pinned,
/// by name. The filter is the schema tree's: a folder whose name matches keeps
/// every member, otherwise it keeps those whose names match and goes when none
/// do. The group's own label is never matched, as the Queries group's is not.
///
/// Where the catalog is one database's, a member of another is left out
/// rather than greyed, since it is not missing, only elsewhere, and so is
/// every member with a database while which one is open is not known. One
/// without a database there is greyed: it cannot say which it was made in.
pub fn shortcuts(
    shortcuts: &Shortcuts,
    catalog: Option<&Catalog>,
    queries: &[String],
    database: &ShortcutDatabase,
    filter: &str,
) -> ExplorerTree {
    let mut build = ShortcutBuilder {
        builder: Builder::new(filter),
        rows: HashMap::new(),
        catalog,
        queries,
        database,
    };
    let pinned: Vec<TreeItem> = shortcuts
        .pins
        .iter()
        .filter_map(|pin| match pin {
            Pin::Member(member) => build.member(PINNED_GROUP, None, member, false),
            Pin::Folder(name) => {
                let folder = shortcuts.folder(name)?;
                build.folder(Some(PINNED_GROUP), folder)
            }
        })
        .collect();
    let mut folders: Vec<&Folder> = shortcuts
        .folders
        .iter()
        .filter(|folder| !shortcuts.pinned(&Pin::Folder(folder.name.clone())))
        .collect();
    folders.sort_by_cached_key(|folder| folder.name.to_lowercase());

    let mut items = Vec::new();
    if !pinned.is_empty() {
        items.push(
            TreeItem::new(PINNED_GROUP, "Pinned")
                .expanded(true)
                .children(pinned),
        );
    }
    items.extend(
        folders
            .into_iter()
            .filter_map(|folder| build.folder(None, folder)),
    );
    ExplorerTree {
        items,
        leaves: build.builder.leaves,
        highlights: build.builder.highlights,
        shortcuts: build.rows,
    }
}

struct ShortcutBuilder<'a> {
    builder: Builder,
    rows: HashMap<String, Shortcut>,
    catalog: Option<&'a Catalog>,
    queries: &'a [String],
    database: &'a ShortcutDatabase,
}

impl ShortcutBuilder<'_> {
    fn folder(&mut self, parent: Option<&str>, folder: &Folder) -> Option<TreeItem> {
        let id = match parent {
            Some(parent) => nested_row(parent, &folder_row(&folder.name)),
            None => folder_row(&folder.name),
        };
        let matched = self.builder.name_matches(&id, &folder.name);
        let children: Vec<TreeItem> = folder
            .members
            .iter()
            .filter_map(|member| self.member(&id, Some(&folder.name), member, matched))
            .collect();
        if !matched && children.is_empty() {
            return None;
        }
        self.rows
            .insert(id.clone(), Shortcut::Folder(folder.name.clone()));
        Some(
            TreeItem::new(id, folder.name.clone())
                .expanded(true)
                .children(children),
        )
    }

    fn member(
        &mut self,
        parent: &str,
        folder: Option<&str>,
        member: &Member,
        inherited: bool,
    ) -> Option<TreeItem> {
        let id = nested_row(parent, &member.id());
        let (label, leaf, present) = match member {
            Member::Query(name) => (name.as_str(), None, self.queries.contains(name)),
            Member::Object(object) => {
                let here = match (self.database, object.database.as_deref()) {
                    (ShortcutDatabase::Unnamed, _) => true,
                    (ShortcutDatabase::On(on), Some(of)) if on == of => true,
                    (ShortcutDatabase::On(_) | ShortcutDatabase::Unknown, Some(_)) => return None,
                    (ShortcutDatabase::On(_) | ShortcutDatabase::Unknown, None) => false,
                };
                let leaf = here
                    .then(|| self.catalog.and_then(|catalog| object.leaf(catalog)))
                    .flatten();
                (object.name.as_str(), leaf, leaf.is_some())
            }
        };
        // A routine's label carries its arguments; only its name is matched,
        // as in the schema tree.
        let name = match member {
            Member::Object(ObjectRef {
                kind: ObjectKind::Routine(_),
                ..
            }) => label.split('(').next().unwrap_or(label),
            _ => label,
        };
        if !(self.builder.name_matches(&id, name) || inherited) {
            return None;
        }
        if let Some(leaf) = leaf {
            self.builder.leaves.insert(id.clone(), leaf);
        }
        self.rows.insert(
            id.clone(),
            Shortcut::Member {
                member: member.clone(),
                folder: folder.map(str::to_string),
                missing: !present,
            },
        );
        Some(TreeItem::new(id, label.to_string()))
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
    let id = relation_row(&schema.name, &relation.name);
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

fn category(label: &'static str, schema: &str, children: Vec<TreeItem>) -> TreeItem {
    TreeItem::new(format!("category-{label}-{schema}"), label)
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
    match engine.syntax() {
        Syntax::Sql => sql_preview(engine.spec(), schema, relation, filter, limit, offset),
        Syntax::Mongo => mql::browse::find_preview(relation, filter, limit, offset),
        // No limit or offset to write: see `Engine::pages`.
        Syntax::Redis => redis_cmd::browse::preview(filter),
    }
}

pub(crate) fn sql_preview(
    spec: &EngineSpec,
    schema: &str,
    relation: &str,
    filter: &str,
    limit: Option<usize>,
    offset: usize,
) -> String {
    let mut sql = format!(
        "SELECT *{}",
        from_where(&spec.qualified(schema, relation), filter)
    );
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
    Some(match engine.syntax() {
        Syntax::Sql => {
            let spec = engine.spec();
            match spec.sql.select_top {
                SelectTop::Limit => sql_preview(spec, schema, relation, "", Some(100), 0),
                SelectTop::Top => {
                    format!("SELECT TOP 100 * FROM {}", spec.qualified(schema, relation))
                }
            }
        }
        Syntax::Mongo => mql::browse::find_preview(relation, "", Some(100), 0),
        Syntax::Redis => return None,
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
    let keyword = match (engine.syntax(), kind) {
        (Syntax::Mongo, _) => return Some(format!("{}.drop()", mql::browse::handle(relation))),
        (Syntax::Redis, _) => return None,
        (Syntax::Sql, RelationKind::Table | RelationKind::PartitionedTable) => "TABLE",
        (Syntax::Sql, RelationKind::View) => "VIEW",
        (Syntax::Sql, RelationKind::Dictionary) => "DICTIONARY",
        (Syntax::Sql, RelationKind::MaterializedView) => {
            match engine.spec().sql.materialized_view {
                MaterializedViewDrop::MaterializedView => "MATERIALIZED VIEW",
                MaterializedViewDrop::View => "VIEW",
            }
        }
        (Syntax::Sql, RelationKind::ForeignTable) => match engine.spec().sql.foreign_table {
            ForeignTable::Foreign => "FOREIGN TABLE",
            ForeignTable::External => "EXTERNAL TABLE",
            ForeignTable::Table => "TABLE",
        },
    };
    Some(format!(
        "DROP {keyword} {};",
        engine.qualified(schema, relation)
    ))
}

pub fn truncate_sql(engine: Engine, schema: &str, relation: &str) -> String {
    let qualified = engine.qualified(schema, relation);
    match engine.syntax() {
        Syntax::Sql => match engine.spec().sql.truncate {
            Truncate::Truncate => format!("TRUNCATE TABLE {qualified};"),
            Truncate::DeleteFrom => format!("DELETE FROM {qualified};"),
        },
        Syntax::Mongo => format!("{}.deleteMany({{}})", mql::browse::handle(relation)),
        Syntax::Redis => "FLUSHDB".into(),
    }
}

/// What a relation's menu copies, in its two groups: the label, and the text,
/// or `None` for the DDL, which only the server can give. The palette offers
/// the same rows for the tab in front, so the two cannot disagree on which.
pub fn relation_copies(
    engine: Engine,
    schema: &str,
    relation: &str,
    kind: RelationKind,
) -> [Vec<(&'static str, Option<String>)>; 2] {
    let mut reads = Vec::new();
    if engine.copies_ddl() {
        reads.push(("Copy DDL", None));
    }
    if let Some(sql) = select_top_sql(engine, schema, relation) {
        reads.push(("Copy SELECT (Top 100)", Some(sql)));
    }
    let mut writes = Vec::new();
    if let Some(sql) = drop_sql(engine, schema, relation, kind) {
        writes.push(("Copy DROP", Some(sql)));
    }
    if matches!(kind, RelationKind::Table | RelationKind::PartitionedTable) {
        writes.push((
            "Copy TRUNCATE",
            Some(truncate_sql(engine, schema, relation)),
        ));
    }
    [reads, writes]
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
    match engine.syntax() {
        Syntax::Redis => redis_cmd::browse::read(cell("key")?, cell("type")?, rows),
        Syntax::Sql | Syntax::Mongo => None,
    }
}

/// Whether a row matches `filter`, for the reference arrow: a constant rather
/// than `*`, so no column's value is read or sent back to answer it.
pub fn probe_sql(engine: Engine, schema: &str, relation: &str, filter: &str) -> String {
    format!(
        "SELECT 1{} LIMIT 1",
        from_where(&engine.qualified(schema, relation), filter)
    )
}

/// The size of what `preview_sql` would page through, for the status bar's
/// Count. Never run unasked: on a large table it is a full scan.
pub fn count_sql(engine: Engine, schema: &str, relation: &str, filter: &str) -> String {
    match engine.syntax() {
        Syntax::Sql => sql_count(engine.spec(), schema, relation, filter),
        Syntax::Mongo => mql::browse::count_documents(relation, filter),
        Syntax::Redis => redis_cmd::browse::count(filter),
    }
}

pub(crate) fn sql_count(spec: &EngineSpec, schema: &str, relation: &str, filter: &str) -> String {
    format!(
        "SELECT {}{}",
        spec.sql.count_all.aggregate(),
        from_where(&spec.qualified(schema, relation), filter)
    )
}

/// The `WHERE` is emitted here rather than spliced in later: this is the one
/// place that knows where the clause goes. `sql::with_order_by` anchors on the
/// `limit` node, so the sort still lands after the filter without knowing a
/// filter exists.
fn from_where(qualified: &str, filter: &str) -> String {
    let mut sql = format!(" FROM {qualified}");
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

    #[test]
    fn a_row_keeps_its_id_when_the_catalog_around_it_moves() {
        let before = catalog();
        let mut after = catalog();
        after.schemas.insert(
            0,
            Schema {
                name: "aardvark".into(),
                relations: Vec::new(),
                routines: Vec::new(),
            },
        );
        after.schemas[2].relations.insert(
            0,
            Relation {
                name: "a_new_table".into(),
                kind: RelationKind::Table,
                partition_of: None,
                size: None,
                rows: None,
            },
        );

        fn ids(items: &[TreeItem], into: &mut HashSet<String>) {
            for item in items {
                into.insert(item.id.to_string());
                ids(&item.children, into);
            }
        }
        let (mut old, mut new) = (HashSet::new(), HashSet::new());
        ids(&tree(&before, "").items, &mut old);
        ids(&tree(&after, "").items, &mut new);
        assert!(old.is_subset(&new), "moved: {:?}", old.difference(&new));
    }

    #[test]
    fn a_tab_finds_its_row_by_kind_and_a_routine_by_its_arguments() {
        let catalog = catalog();
        let row = |name: &str, kind| object_row(&catalog, "public", name, kind);
        assert_eq!(
            row("accounts", ObjectKind::Relation(RelationKind::Table)).as_deref(),
            Some("relation-public\0accounts")
        );
        assert_eq!(
            row(
                "account_name(account_id bigint)",
                ObjectKind::Routine(RoutineKind::Function)
            )
            .as_deref(),
            Some("routine-Function-public\0account_name(account_id bigint)")
        );
        assert_eq!(
            row("account_name", ObjectKind::Routine(RoutineKind::Function)),
            None
        );
        assert_eq!(
            row("reindex()", ObjectKind::Relation(RelationKind::Table)),
            None
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

    #[test]
    fn every_saved_query_is_listed_under_its_group_and_a_filter_narrows_them() {
        let names = ["daily revenue".to_string(), "signups".to_string()];

        let all = queries(&names, "");
        assert_eq!(all.items.len(), 1);
        assert_eq!(all.items[0].id.as_ref(), QUERIES_GROUP);
        let rows: Vec<_> = all.items[0]
            .children
            .iter()
            .map(|row| (saved_query(&row.id), row.label.as_ref()))
            .collect();
        assert_eq!(
            rows,
            [
                (Some("daily revenue"), "daily revenue"),
                (Some("signups"), "signups")
            ]
        );

        let narrowed = queries(&names, "rev");
        assert_eq!(narrowed.items[0].children.len(), 1);
        assert_eq!(
            marked(&narrowed, "query:daily revenue", "daily revenue").as_deref(),
            Some("rev")
        );

        // No query, or none the filter keeps, is no group rather than an
        // empty one.
        assert!(queries(&names, "zzz").items.is_empty());
        assert!(queries(&[], "").items.is_empty());
    }

    #[test]
    fn no_catalog_rows_id_reads_as_a_saved_query() {
        let explorer = tree(&catalog(), "");
        let mut ids = Vec::new();
        let mut stack: Vec<&TreeItem> = explorer.items.iter().collect();
        while let Some(item) = stack.pop() {
            ids.push(item.id.to_string());
            stack.extend(item.children.iter());
        }
        assert!(!ids.is_empty());
        assert!(
            ids.iter()
                .all(|id| saved_query(id).is_none() && id != QUERIES_GROUP)
        );
    }

    #[test]
    fn only_a_schemas_row_is_offered_as_the_active_schema() {
        // A schema named like the group is still a schema, and the group is
        // never one: picking it would set and save a schema called Queries.
        let names = ["daily".to_string()];
        let mut explorer = queries(&names, "");
        explorer.extend(tree(&catalog(), ""));
        let roots: Vec<_> = explorer
            .items
            .iter()
            .map(|item| (item.id.as_ref(), is_schema_row(&item.id)))
            .collect();
        assert_eq!(
            roots,
            [
                (QUERIES_GROUP, false),
                ("schema-analytics", true),
                ("schema-public", true)
            ]
        );
        let group = &explorer.items[0];
        assert!(!is_schema_row(&group.children[0].id));
        assert!(!is_schema_row(&explorer.items[1].children[0].id));
    }

    #[test]
    fn a_change_to_the_saved_queries_keeps_what_was_folded_and_opened() {
        let build = |names: &[String]| {
            let mut explorer = queries(names, "");
            explorer.extend(tree(&partitioned_catalog(), ""));
            explorer.items
        };
        let before = build(&["daily".to_string()]);
        let schema = before
            .iter()
            .find(|item| is_schema_row(&item.id))
            .expect("a schema row");
        let category = &schema.children[0];
        let parent = schema
            .children
            .iter()
            .flat_map(|category| &category.children)
            .find(|relation| relation.is_folder())
            .expect("a partitioned table");
        // The schema and a category folded, a partitioned table opened: the
        // opposite of how each is built. A clone shares its row's state, as
        // the tree's own entries do, so folding one folds the row.
        assert!(schema.is_expanded() && category.is_expanded() && !parent.is_expanded());
        let (schema_id, category_id, parent_id) = (
            schema.id.to_string(),
            category.id.to_string(),
            parent.id.to_string(),
        );
        let _ = schema.clone().expanded(false);
        let _ = category.clone().expanded(false);
        let _ = parent.clone().expanded(true);

        let after = with_folds(
            build(&["daily".to_string(), "signups".to_string()]),
            &folds(&before),
        );
        let state = folds(&after);
        assert_eq!(state.get(&schema_id), Some(&false));
        assert_eq!(state.get(&category_id), Some(&false));
        assert_eq!(state.get(&parent_id), Some(&true));
        // The group's new query is there, and the group is open as it was.
        assert_eq!(after[0].children.len(), 2);
        assert_eq!(state.get(QUERIES_GROUP), Some(&true));
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
            marked(&explorer, "relation-public\0accounts", "accounts").as_deref(),
            Some("acct")
        );
        // A routine's row reads `name(args)`, and only the name was matched.
        let label = "account_name(account_id bigint)";
        assert_eq!(categories[2].children[0].label, label);
        assert_eq!(
            marked(
                &explorer,
                "routine-Function-public\0account_name(account_id bigint)",
                label
            )
            .as_deref(),
            Some("acct")
        );
        assert!(
            explorer.highlights["routine-Function-public\0account_name(account_id bigint)"]
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
            marked(&explorer, "schema-analytics", "analytics").as_deref(),
            Some("analytics")
        );
        // Kept because its schema matched, not because of its name.
        assert!(explorer.leaves.contains_key("relation-analytics\0events"));
        assert!(
            !explorer
                .highlights
                .contains_key("relation-analytics\0events")
        );

        assert!(tree(&catalog(), "").highlights.is_empty());
    }

    #[test]
    fn tree_targets_use_catalog_indices_not_database_names() {
        let explorer = tree(&catalog(), "account_name");

        assert_eq!(
            explorer
                .leaves
                .get("routine-Function-public\0account_name(account_id bigint)"),
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
            kind("relation-public\0active_accounts"),
            Some(ObjectKind::Relation(RelationKind::View))
        );
        assert_eq!(
            kind("relation-public\0accounts"),
            Some(ObjectKind::Relation(RelationKind::Table))
        );
    }

    fn relation(schema: &str, name: &str) -> Member {
        Member::Object(ObjectRef {
            database: None,
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Relation(RelationKind::Table),
        })
    }

    fn folder(name: &str, members: Vec<Member>) -> Folder {
        Folder {
            name: name.into(),
            members,
        }
    }

    fn labels(items: &[TreeItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_ref()).collect()
    }

    fn every_id(items: &[TreeItem], into: &mut Vec<String>) {
        for item in items {
            into.push(item.id.to_string());
            every_id(&item.children, into);
        }
    }

    #[test]
    fn a_folder_is_shortcuts_and_the_schemas_still_list_everything() {
        let shortcuts = Shortcuts {
            folders: vec![folder(
                "Hot",
                vec![
                    relation("public", "accounts"),
                    Member::Query("daily".into()),
                ],
            )],
            pins: Vec::new(),
        };
        let names = ["daily".to_string()];
        let mut explorer = super::shortcuts(
            &shortcuts,
            Some(&catalog()),
            &names,
            &ShortcutDatabase::Unnamed,
            "",
        );
        explorer.extend(queries(&names, ""));
        explorer.extend(tree(&catalog(), ""));

        assert_eq!(
            labels(&explorer.items),
            ["Hot", "Queries", "analytics", "public"]
        );
        let hot = &explorer.items[0];
        assert_eq!(labels(&hot.children), ["accounts", "daily"]);
        // The shortcut opens what the row it repeats opens.
        assert_eq!(
            explorer.leaves.get(hot.children[0].id.as_ref()),
            explorer.leaves.get("relation-public\0accounts")
        );
        assert!(explorer.leaves.contains_key("relation-public\0accounts"));
        assert_eq!(
            explorer.shortcuts[hot.children[1].id.as_ref()].query(),
            Some("daily")
        );
        assert_eq!(
            explorer.shortcuts[hot.id.as_ref()],
            Shortcut::Folder("Hot".into())
        );
    }

    #[test]
    fn pins_come_first_in_the_order_pinned_and_a_pinned_folder_moves_into_them() {
        let shortcuts = Shortcuts {
            folders: vec![
                folder("gamma", Vec::new()),
                folder("beta", vec![relation("analytics", "events")]),
                folder("Alpha", Vec::new()),
            ],
            pins: vec![
                Pin::Member(Member::Query("daily".into())),
                Pin::Folder("beta".into()),
                Pin::Member(relation("public", "accounts")),
            ],
        };
        let explorer = super::shortcuts(
            &shortcuts,
            Some(&catalog()),
            &["daily".into()],
            &ShortcutDatabase::Unnamed,
            "",
        );

        assert_eq!(labels(&explorer.items), ["Pinned", "Alpha", "gamma"]);
        let pinned = &explorer.items[0];
        assert_eq!(labels(&pinned.children), ["daily", "beta", "accounts"]);
        assert_eq!(labels(&pinned.children[1].children), ["events"]);
        // Nothing pinned is no group at all.
        let unpinned = Shortcuts {
            pins: Vec::new(),
            ..shortcuts
        };
        let explorer = super::shortcuts(
            &unpinned,
            Some(&catalog()),
            &[],
            &ShortcutDatabase::Unnamed,
            "",
        );
        assert_eq!(labels(&explorer.items), ["Alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_missing_member_stays_greyed_and_another_databases_is_left_out() {
        let elsewhere = |database: &str, name: &str| {
            Member::Object(ObjectRef {
                database: Some(database.into()),
                schema: "public".into(),
                name: name.into(),
                kind: ObjectKind::Relation(RelationKind::Table),
            })
        };
        let shortcuts = Shortcuts {
            folders: vec![folder(
                "Mixed",
                vec![
                    elsewhere("dev", "accounts"),
                    elsewhere("dev", "dropped"),
                    elsewhere("prod", "accounts"),
                    Member::Query("gone".into()),
                ],
            )],
            pins: Vec::new(),
        };
        let explorer = super::shortcuts(
            &shortcuts,
            Some(&catalog()),
            &[],
            &ShortcutDatabase::On("dev".into()),
            "",
        );
        let rows = &explorer.items[0].children;

        assert_eq!(labels(rows), ["accounts", "dropped", "gone"]);
        let missing: Vec<_> = rows
            .iter()
            .map(|row| match &explorer.shortcuts[row.id.as_ref()] {
                Shortcut::Member { missing, .. } => *missing,
                Shortcut::Folder(_) => panic!("a member is not a folder"),
            })
            .collect();
        assert_eq!(missing, [false, true, true]);
        assert!(!explorer.leaves.contains_key(rows[1].id.as_ref()));
        assert_eq!(explorer.shortcuts[rows[2].id.as_ref()].query(), None);
    }

    #[test]
    fn a_member_matches_only_in_the_database_it_names() {
        let object = |database: Option<&str>, schema: &str, name: &str| {
            Member::Object(ObjectRef {
                database: database.map(str::to_string),
                schema: schema.into(),
                name: name.into(),
                kind: ObjectKind::Relation(RelationKind::Table),
            })
        };
        let shortcuts = Shortcuts {
            folders: vec![folder(
                "Hot",
                vec![
                    object(Some("dev"), "public", "accounts"),
                    object(None, "analytics", "events"),
                ],
            )],
            pins: Vec::new(),
        };
        let listed = |database: ShortcutDatabase| {
            let explorer = super::shortcuts(&shortcuts, Some(&catalog()), &[], &database, "");
            explorer.items[0]
                .children
                .iter()
                .map(|row| match &explorer.shortcuts[row.id.as_ref()] {
                    Shortcut::Member { missing, .. } => (row.label.to_string(), *missing),
                    Shortcut::Folder(_) => panic!("a member is not a folder"),
                })
                .collect::<Vec<_>>()
        };

        // Without a database, a member cannot say which one it was made in.
        assert_eq!(
            listed(ShortcutDatabase::On("dev".into())),
            [("accounts".into(), false), ("events".into(), true)]
        );
        assert_eq!(
            listed(ShortcutDatabase::Unknown),
            [("events".to_string(), true)]
        );
    }

    #[test]
    fn members_differing_only_by_database_or_kind_are_rows_of_their_own() {
        let object = |database: Option<&str>, kind: RelationKind| {
            Member::Object(ObjectRef {
                database: database.map(str::to_string),
                schema: "public".into(),
                name: "accounts".into(),
                kind: ObjectKind::Relation(kind),
            })
        };
        let members = vec![
            object(Some("dev"), RelationKind::Table),
            object(None, RelationKind::Table),
            object(Some("dev"), RelationKind::View),
        ];
        let shortcuts = Shortcuts {
            folders: vec![folder("Hot", members.clone())],
            pins: Vec::new(),
        };
        let database = ShortcutDatabase::On("dev".into());
        let explorer = super::shortcuts(&shortcuts, Some(&catalog()), &[], &database, "");

        let listed: Vec<_> = explorer.items[0]
            .children
            .iter()
            .map(|row| match &explorer.shortcuts[row.id.as_ref()] {
                Shortcut::Member { member, .. } => member.clone(),
                Shortcut::Folder(_) => panic!("a member is not a folder"),
            })
            .collect();
        assert_eq!(listed, members);
    }

    #[test]
    fn the_filter_reaches_into_folders_and_pins() {
        let shortcuts = Shortcuts {
            folders: vec![
                folder("Reports", vec![relation("public", "accounts")]),
                folder("Misc", vec![relation("analytics", "events")]),
                folder("Empty", Vec::new()),
            ],
            pins: vec![Pin::Member(relation("analytics", "events"))],
        };
        let build = |filter| {
            super::shortcuts(
                &shortcuts,
                Some(&catalog()),
                &[],
                &ShortcutDatabase::Unnamed,
                filter,
            )
        };

        // With no filter an empty folder still shows.
        assert_eq!(
            labels(&build("").items),
            ["Pinned", "Empty", "Misc", "Reports"]
        );
        // A folder's name keeps all of its members.
        let by_folder = build("reports");
        assert_eq!(labels(&by_folder.items), ["Reports"]);
        assert_eq!(labels(&by_folder.items[0].children), ["accounts"]);
        // A member's name keeps it and its folder, and the group it is pinned in.
        let by_member = build("events");
        assert_eq!(labels(&by_member.items), ["Pinned", "Misc"]);
        assert_eq!(labels(&by_member.items[1].children), ["events"]);
        assert!(build("zzz").items.is_empty());
    }

    #[test]
    fn every_row_id_is_unique_whatever_a_folder_is_called() {
        let shortcuts = Shortcuts {
            folders: vec![
                folder(
                    "a",
                    vec![Member::Query("x".into()), relation("public", "accounts")],
                ),
                folder("a/query:x", vec![Member::Query("x".into())]),
                folder("pinned", vec![relation("public", "accounts")]),
                folder("queries", Vec::new()),
                folder("schema-public", Vec::new()),
            ],
            pins: vec![
                Pin::Member(Member::Query("x".into())),
                Pin::Member(relation("public", "accounts")),
                Pin::Folder("a".into()),
            ],
        };
        let names = ["x".to_string()];
        let mut explorer = super::shortcuts(
            &shortcuts,
            Some(&catalog()),
            &names,
            &ShortcutDatabase::Unnamed,
            "",
        );
        explorer.extend(queries(&names, ""));
        explorer.extend(tree(&catalog(), ""));

        let mut ids = Vec::new();
        every_id(&explorer.items, &mut ids);
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        // A folder named like a schema is still no schema's row.
        assert!(
            explorer
                .shortcuts
                .keys()
                .all(|id| !is_schema_row(id) && saved_query(id).is_none())
        );
    }

    #[test]
    fn folders_follow_renames_and_refuse_a_name_taken_in_any_case() {
        let mut shortcuts = Shortcuts::default();
        shortcuts.add_folder("Reports").unwrap();
        assert_eq!(
            shortcuts.add_folder("reports"),
            Err("A folder named Reports already exists.".into())
        );
        assert!(shortcuts.add_folder("").is_err());
        assert_eq!(shortcuts.unused_name(), "New Folder");
        shortcuts.add_folder("New Folder").unwrap();
        assert_eq!(shortcuts.unused_name(), "New Folder 2");

        shortcuts.toggle("Reports", Member::Query("daily".into()));
        shortcuts.toggle("Reports", relation("public", "accounts"));
        shortcuts.toggle_pin(Pin::Folder("Reports".into()));
        shortcuts.toggle_pin(Pin::Member(Member::Query("daily".into())));
        // A rename to its own name in another case is no clash.
        shortcuts.rename_folder("Reports", "REPORTS").unwrap();
        assert!(shortcuts.rename_folder("REPORTS", "new folder").is_err());
        assert!(shortcuts.pinned(&Pin::Folder("REPORTS".into())));

        shortcuts.rename_query("daily", "nightly");
        assert!(shortcuts.holds("REPORTS", &Member::Query("nightly".into())));
        assert!(shortcuts.pinned(&Pin::Member(Member::Query("nightly".into()))));
        // A duplicate has no saved queries to point at.
        let copy = shortcuts.duplicated();
        assert_eq!(
            copy.folder("REPORTS").unwrap().members,
            [relation("public", "accounts")]
        );
        assert_eq!(copy.pins, [Pin::Folder("REPORTS".into())]);

        shortcuts.forget_query("nightly");
        assert!(!shortcuts.holds("REPORTS", &Member::Query("nightly".into())));
        assert_eq!(shortcuts.pins, [Pin::Folder("REPORTS".into())]);
        // Toggling again takes it out, whatever kind it has become since.
        let mut view = relation("public", "accounts");
        if let Member::Object(object) = &mut view {
            object.kind = ObjectKind::Relation(RelationKind::View);
        }
        shortcuts.toggle("REPORTS", view);
        assert!(shortcuts.folder("REPORTS").unwrap().members.is_empty());
        shortcuts.delete_folder("REPORTS");
        assert!(shortcuts.pins.is_empty());
    }

    #[test]
    fn an_object_kind_reads_back_from_its_slug() {
        for (kind, _) in RELATION_CATEGORIES {
            let kind = ObjectKind::Relation(kind);
            assert_eq!(ObjectKind::from_slug(kind.slug()), Some(kind));
        }
        for (kind, _) in ROUTINE_CATEGORIES {
            let kind = ObjectKind::Routine(kind);
            assert_eq!(ObjectKind::from_slug(kind.slug()), Some(kind));
        }
        assert_eq!(ObjectKind::from_slug("sequence"), None);
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
            drop_sql(Engine::ClickHouse, "db", "d", RelationKind::Dictionary).unwrap(),
            "DROP DICTIONARY `db`.`d`;"
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

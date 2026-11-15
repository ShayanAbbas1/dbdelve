//! What the connections sidebar lists, as plain data.
//!
//! Complex mode draws every connection under its project, and each
//! connection's own rows are the ones simple mode draws for it. Two rules hold
//! the model together. Ids are length-prefixed, so no name -- a profile's, a
//! project's, a table's -- can run on into the next segment and be read as
//! another row's. And [`build`] takes loaded data only: it neither connects,
//! reads a catalog nor sees a window, so what a row says is what the caller
//! had already been told.

use std::{collections::HashMap, ops::Range};

use gpui_component::tree::TreeItem;

use crate::db::Levels;
use crate::explorer::{Builder, ExplorerLeaf, ExplorerTree, Shortcut, is_schema_row, saved_query};
use crate::store::StoredProject;

/// Which sidebar the window draws: today's tree for the connection in front,
/// or every connection under its project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ViewMode {
    #[default]
    Simple,
    Complex,
}

impl ViewMode {
    /// How `profiles.toml` spells it.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            ViewMode::Simple => "simple",
            ViewMode::Complex => "complex",
        }
    }

    /// The mode a stored slug names, if it names one this build knows.
    pub(crate) fn parse(slug: Option<&str>) -> Option<Self> {
        match slug {
            Some("simple") => Some(ViewMode::Simple),
            Some("complex") => Some(ViewMode::Complex),
            _ => None,
        }
    }

    /// The mode a stored slug names. A file with none, or one this build does
    /// not know, reads as Simple: what every file written before it showed.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        Self::parse(slug).unwrap_or_default()
    }
}

/// The mode a project in front draws: its own choice, or the global
/// `global` when it has none, when its choice is one this build cannot read,
/// and when no project is in front.
pub(crate) fn effective_view_mode(global: ViewMode, project: Option<&StoredProject>) -> ViewMode {
    project
        .and_then(|project| ViewMode::parse(project.view_mode.as_deref()))
        .unwrap_or(global)
}

/// Which connections the sidebar lists: every project's, or only those of the
/// project in front.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ListingScope {
    #[default]
    All,
    Current,
}

impl ListingScope {
    /// How `profiles.toml` spells it.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            ListingScope::All => "all",
            ListingScope::Current => "current",
        }
    }

    /// The scope a stored slug names, if it names one this build knows.
    pub(crate) fn parse(slug: Option<&str>) -> Option<Self> {
        match slug {
            Some("all") => Some(ListingScope::All),
            Some("current") => Some(ListingScope::Current),
            _ => None,
        }
    }

    /// The scope a stored slug names; none, or one this build does not know,
    /// lists every project.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        Self::parse(slug).unwrap_or_default()
    }
}

/// The scope a project in front lists: its own choice, or `global` when it
/// has none, when its choice is one this build cannot read, and when no
/// project is in front.
pub(crate) fn effective_listing_scope(
    global: ListingScope,
    project: Option<&StoredProject>,
) -> ListingScope {
    project
        .and_then(|project| ListingScope::parse(project.listing_scope.as_deref()))
        .unwrap_or(global)
}

/// The row of connections that belong to no project, listed first while any
/// project exists.
pub(crate) const NO_PROJECT_ROW: &str = "no-project";

/// A project's row id. The name follows its length, as a folder's does.
pub(crate) fn project_row(name: &str) -> String {
    format!("project:{}:{name}", name.len())
}

/// The row of the group a connection is in: a project's, or `None` for the
/// connections in none.
pub(crate) fn group_row(project: Option<&str>) -> String {
    project.map_or_else(|| NO_PROJECT_ROW.to_string(), project_row)
}

/// A connection's row id, by the profile's id.
pub(crate) fn connection_row(profile: &str) -> String {
    format!("connection:{}:{profile}", profile.len())
}

/// A database's row id under its connection. The database follows its length,
/// as a profile's does, so no name can be read as another row's.
pub(crate) fn database_row(profile: &str, database: &str) -> String {
    format!(
        "{}/database:{}:{database}",
        connection_row(profile),
        database.len()
    )
}

/// The id of `row`, one of simple mode's rows, listed under `profile`, and
/// under `database` where the connection draws one over its schemas. The
/// lengths keep a `/` in any of them from moving a seam.
pub(crate) fn scoped_row(profile: &str, database: Option<&str>, row: &str) -> String {
    match database {
        Some(database) => format!("{}/{row}", database_row(profile, database)),
        None => format!("{}/{row}", connection_row(profile)),
    }
}

/// What a [`scoped_row`] was made from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Scoped<'a> {
    pub(crate) profile: &'a str,
    pub(crate) database: Option<&'a str>,
    /// The simple-mode row id.
    pub(crate) inner: &'a str,
}

/// The profile, the database if any, and the simple-mode row id a
/// [`scoped_row`] was made from. `None` for any other id, including a
/// connection's own row and a database's.
///
/// A row id that itself began `database:` would read back as under a
/// database; no row simple mode lists is named that way.
pub(crate) fn unscoped(id: &str) -> Option<Scoped<'_>> {
    let (profile, rest) = length_prefixed(id.strip_prefix("connection:")?)?;
    let rest = rest.strip_prefix('/')?;
    let scoped = match rest.strip_prefix("database:") {
        Some(rest) => {
            let (database, rest) = length_prefixed(rest)?;
            Scoped {
                profile,
                database: Some(database),
                inner: rest.strip_prefix('/')?,
            }
        }
        None => Scoped {
            profile,
            database: None,
            inner: rest,
        },
    };
    // Read back the way it was written: `+3` parses as a length, and would
    // otherwise name the same row as `3`.
    (scoped_row(scoped.profile, scoped.database, scoped.inner) == id).then_some(scoped)
}

/// `{length}:{name}` and what follows the name.
fn length_prefixed(text: &str) -> Option<(&str, &str)> {
    let (length, rest) = text.split_once(':')?;
    let name = rest.get(..length.parse().ok()?)?;
    Some((name, rest.get(name.len()..)?))
}

/// Whether `filter` keeps a connection's rows whole, because it matches the
/// connection's name or its database's: what a name matches takes its rows
/// with it, as a schema's name does in simple mode.
pub(crate) fn keeps_rows_whole(
    filter: &str,
    name: &str,
    levels: Levels,
    database: Option<&str>,
) -> bool {
    let mut builder = Builder::new(filter);
    builder.name_matches("", name)
        || catalog_database(levels, database)
            .is_some_and(|database| builder.name_matches("", database))
}

/// The database the catalog's rows sit under in the tree, if its shape draws
/// one over them.
fn catalog_database(levels: Levels, database: Option<&str>) -> Option<&str> {
    database.filter(|_| levels == Levels::DatabaseThenSchema)
}

/// The id the tree gives `row`, one of the rows simple mode lists for the tab
/// in front of a connection: an object's sits under the database where the
/// connection has one, a saved query's under the connection.
pub(crate) fn listed_row(
    profile: &str,
    levels: Levels,
    database: Option<&str>,
    row: &str,
) -> String {
    let database = match saved_query(row) {
        Some(_) => None,
        None => catalog_database(levels, database),
    };
    scoped_row(profile, database, row)
}

/// Whether a connection of this shape draws its schema rows as databases.
pub(crate) fn schema_row_is_a_database(levels: Levels) -> bool {
    matches!(
        levels,
        Levels::DatabaseIsSchema | Levels::DatabaseHoldsObjects
    )
}

/// What a connection that has nothing to list says in its one row.
pub(crate) const LOADING: &str = "Loading…";

/// How much of a connection is known.
pub(crate) enum Content {
    /// Never asked to load: it connects when first expanded.
    Unloaded,
    Loading,
    Failed(String),
    Loaded(ExplorerTree),
}

/// One connection, as the caller has it.
pub(crate) struct Listed<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) content: Content,
    /// The shape its target gives its rows.
    pub(crate) levels: Levels,
    /// The database its target's catalog is of, where the shape draws one
    /// over the schemas.
    pub(crate) database: Option<String>,
}

/// What a row stands for.
pub(crate) enum Node {
    /// A project, or the connections in none.
    Group,
    Connection {
        profile: String,
    },
    /// A connection's database, with nothing to act on yet.
    Database,
    /// Says what happened to its connection, and is never acted on.
    Notice,
    /// One of simple mode's rows, its leaf when it opens an object.
    Row {
        leaf: Option<ExplorerLeaf>,
    },
}

/// The rows, and what each stands for, by row id.
#[derive(Default)]
pub(crate) struct SidebarTree {
    pub(crate) items: Vec<TreeItem>,
    pub(crate) nodes: HashMap<String, Node>,
    pub(crate) highlights: HashMap<String, Vec<Range<usize>>>,
    pub(crate) shortcuts: HashMap<String, Shortcut>,
    /// How many rows each folder row holds, drawn in a slot of its own. A row
    /// that has not loaded has no entry: nothing is counted before it is
    /// known, and never as `0`.
    pub(crate) counts: HashMap<String, usize>,
}

/// What a loaded schema or database with nothing in it says in its one row.
pub(crate) const NO_OBJECTS: &str = "No objects";

/// The rows for `listed`, grouped under `projects` the way the palette's
/// Connections scope orders them: the connections in no project first, and
/// only while a project exists, then each project in its stored order, every
/// connection in the order `listed` has it. Nothing is sorted, so two builds
/// over the same input give the same rows.
///
/// `scope` is which groups are listed. `Current` lists the connections of the
/// group named by `current` (`None` is the connections in no project) and
/// nothing else, with no group row over them, as the palette lists the
/// current project's connections as one flat group. `All` lists every group,
/// and a group starts open only when it is `current`.
///
/// `folds` is whether each row was open in the tree this one replaces, by
/// row id, so a rebuild puts the user's folding back. It wins over every
/// default.
///
/// `filter` is what the tree is narrowed by. The rows of a loaded connection
/// arrive narrowed already; what is left to decide is which names match. A
/// connection that has not loaded is kept only by its own name, and folded,
/// since nothing here can load it; a project is kept by its name or by a
/// connection it kept. While there is text every kept row that has rows under
/// it is open, so a match is seen, and the counts are of the rows kept.
pub(crate) fn build(
    projects: &[StoredProject],
    listed: Vec<Listed<'_>>,
    folds: &HashMap<String, bool>,
    scope: ListingScope,
    current: Option<&str>,
    filter: &str,
) -> SidebarTree {
    let mut tree = SidebarTree::default();
    let mut search = Search {
        builder: Builder::new(filter),
        filtering: !filter.trim().is_empty(),
    };
    // One bucket per project, and the first for the connections in none. A
    // connection a second project also names stays under the first, as it
    // does in the palette.
    let mut buckets: Vec<Vec<TreeItem>> = vec![Vec::new(); projects.len() + 1];
    for connection in listed {
        let bucket = projects
            .iter()
            .position(|project| project.connections.iter().any(|id| id == connection.id))
            .map_or(0, |at| at + 1);
        buckets[bucket].extend(connection_item(&mut tree, connection, folds, &mut search));
    }
    if projects.is_empty() {
        tree.items = buckets.swap_remove(0);
        tree.highlights.extend(search.builder.into_highlights());
        return tree;
    }
    if scope == ListingScope::Current {
        let bucket = current
            .and_then(|name| projects.iter().position(|project| project.name == name))
            .map_or(0, |at| at + 1);
        tree.items = buckets.swap_remove(bucket);
        tree.highlights.extend(search.builder.into_highlights());
        return tree;
    }
    let current_row = group_row(current);
    let mut buckets = buckets.into_iter();
    // A group with nothing in it is a row that only says so, which is wanted
    // of a project the user made and not of the absence of one.
    let unlisted = buckets.next().unwrap_or_default();
    let groups = (!unlisted.is_empty())
        .then(|| (NO_PROJECT_ROW.to_string(), "No project", unlisted, false))
        .into_iter()
        .chain(projects.iter().zip(buckets).map(|(project, members)| {
            (
                project_row(&project.name),
                project.name.as_str(),
                members,
                true,
            )
        }));
    for (id, label, members, named_by_user) in groups {
        // "No project" is not a name anyone gave, so typing never matches it.
        let named = named_by_user && search.builder.name_matches(&id, label);
        if search.filtering && !named && members.is_empty() {
            continue;
        }
        tree.nodes.insert(id.clone(), Node::Group);
        tree.counts.insert(id.clone(), members.len());
        let open = search.filtering || folds.get(&id).copied().unwrap_or(id == current_row);
        tree.items.push(
            TreeItem::new(id, label.to_string())
                .expanded(open)
                .children(members),
        );
    }
    tree.highlights.extend(search.builder.into_highlights());
    tree
}

/// One search over the names the tree lists.
struct Search {
    builder: Builder,
    /// There is text typed: what it matches is kept, the rest is not.
    filtering: bool,
}

/// The connection's row and its own rows, or `None` when the filter keeps
/// neither its name nor anything it has loaded.
fn connection_item(
    tree: &mut SidebarTree,
    connection: Listed<'_>,
    folds: &HashMap<String, bool>,
    search: &mut Search,
) -> Option<TreeItem> {
    let id = connection_row(connection.id);
    let named = search.builder.name_matches(&id, connection.name);
    // Without rows of its own to match, only its name can keep a connection.
    let has_rows =
        matches!(&connection.content, Content::Loaded(content) if !content.items.is_empty());
    if search.filtering && !named && !has_rows {
        return None;
    }
    tree.nodes.insert(
        id.clone(),
        Node::Connection {
            profile: connection.id.to_string(),
        },
    );
    let followed = folds.get(&id).copied().unwrap_or(false);
    // What it counts is the rows where databases would be, which a flat
    // connection has none of.
    let counts_databases = connection.levels != Levels::Flat;
    let mut count = None;
    let (open, children) = match connection.content {
        // Nothing has asked it to load, and nothing will until this row is
        // opened: an open row here would show a `Loading…` that no answer is
        // coming to replace.
        Content::Unloaded => (false, vec![notice(tree, connection.id, "loading", LOADING)]),
        Content::Loading => (
            followed,
            vec![notice(tree, connection.id, "loading", LOADING)],
        ),
        Content::Failed(text) => (followed, vec![notice(tree, connection.id, "notice", &text)]),
        Content::Loaded(content) if content.items.is_empty() => {
            count = counts_databases.then_some(0);
            (
                followed,
                vec![notice(
                    tree,
                    connection.id,
                    "notice",
                    "No database objects found.",
                )],
            )
        }
        Content::Loaded(content) => {
            let ExplorerTree {
                items,
                leaves,
                highlights,
                shortcuts,
            } = content;
            let mut rows = Rows {
                profile: connection.id,
                leaves,
                highlights,
                shortcuts,
                folds,
                filtering: search.filtering,
            };
            // The Pinned group, the folders and the Queries group stay on the
            // connection; the catalog's rows go under its database.
            let (catalog, listed): (Vec<_>, Vec<_>) =
                items.into_iter().partition(|item| is_schema_row(&item.id));
            let mut children: Vec<TreeItem> = listed
                .into_iter()
                .map(|item| scoped_item(tree, &mut rows, None, item))
                .collect();
            // ponytail: a Snowflake profile that names no database has none
            // to draw until it lists one, so its schemas sit on the
            // connection. The upgrade is to read the database the session
            // reports once connected.
            match catalog_database(connection.levels, connection.database.as_deref()) {
                Some(database) => {
                    let under: Vec<TreeItem> = catalog
                        .into_iter()
                        .map(|item| scoped_item(tree, &mut rows, Some(database), item))
                        .collect();
                    let row = database_row(connection.id, database);
                    let named = search.builder.name_matches(&row, database);
                    if !search.filtering || named || !under.is_empty() {
                        tree.nodes.insert(row.clone(), Node::Database);
                        tree.counts.insert(row.clone(), under.len());
                        let open = search.filtering || folds.get(&row).copied().unwrap_or(false);
                        let mut item = TreeItem::new(row.clone(), database.to_string());
                        item = match under.is_empty() {
                            true => item.children(vec![empty_row(tree, &row)]),
                            false => item.children(under),
                        };
                        children.push(item.expanded(open));
                        count = Some(1);
                    } else {
                        count = Some(0);
                    }
                }
                None => {
                    count = counts_databases.then_some(catalog.len());
                    children.extend(
                        catalog
                            .into_iter()
                            .map(|item| scoped_item(tree, &mut rows, None, item)),
                    );
                }
            }
            (search.filtering || followed, children)
        }
    };
    if let Some(count) = count {
        tree.counts.insert(id.clone(), count);
    }
    Some(
        TreeItem::new(id, connection.name.to_string())
            .expanded(open)
            .children(children),
    )
}

/// The one row a loaded folder with nothing in it holds, so that it can be
/// opened and says why there is nothing. Its id starts where no other row's
/// does, and names `parent`.
fn empty_row(tree: &mut SidebarTree, parent: &str) -> TreeItem {
    let id = format!("empty:{}:{parent}", parent.len());
    tree.nodes.insert(id.clone(), Node::Notice);
    TreeItem::new(id, NO_OBJECTS.to_string()).disabled(true)
}

/// A row that says something about its connection and opens nothing. Its id
/// is one no simple-mode row can have.
fn notice(tree: &mut SidebarTree, profile: &str, row: &str, text: &str) -> TreeItem {
    let id = scoped_row(profile, None, row);
    tree.nodes.insert(id.clone(), Node::Notice);
    TreeItem::new(id, text.to_string()).disabled(true)
}

/// What one connection's rows are given their ids from.
struct Rows<'a> {
    profile: &'a str,
    leaves: HashMap<String, ExplorerLeaf>,
    highlights: HashMap<String, Vec<Range<usize>>>,
    shortcuts: HashMap<String, Shortcut>,
    folds: &'a HashMap<String, bool>,
    /// There is text typed: every row with rows under it is open.
    filtering: bool,
}

/// `item` and everything under it with its id scoped to the connection, and
/// to `database` where the rows sit under one. What the filter matched and
/// what a shortcut stands for move to the new id with the row. A folder is
/// folded unless `folds` says it was open: a connect must not unfold the
/// thousands of rows a catalog can hold.
fn scoped_item(
    tree: &mut SidebarTree,
    rows: &mut Rows<'_>,
    database: Option<&str>,
    mut item: TreeItem,
) -> TreeItem {
    let id = scoped_row(rows.profile, database, &item.id);
    tree.nodes.insert(
        id.clone(),
        Node::Row {
            leaf: rows.leaves.get(item.id.as_str()).copied(),
        },
    );
    if let Some(ranges) = rows.highlights.remove(item.id.as_str()) {
        tree.highlights.insert(id.clone(), ranges);
    }
    let shortcut = rows.shortcuts.remove(item.id.as_str());
    // A folder of shortcuts counts its members even with none; an object and
    // a member of a folder are one row each and hold nothing to count.
    let schema = is_schema_row(&item.id);
    let counted = !rows.leaves.contains_key(item.id.as_str())
        && (item.is_folder() || schema || matches!(shortcut, Some(Shortcut::Folder(_))));
    if let Some(shortcut) = shortcut {
        tree.shortcuts.insert(id.clone(), shortcut);
    }
    let disabled = item.is_disabled();
    let label = item.label.clone();
    let mut children: Vec<TreeItem> = std::mem::take(&mut item.children)
        .into_iter()
        .map(|child| scoped_item(tree, rows, database, child))
        .collect();
    if counted {
        // A schema counts the objects of its kinds, each already counted once
        // with its partitions under it; anything else its own rows.
        let count = match schema {
            true => children
                .iter()
                .filter_map(|child| tree.counts.get(child.id.as_str()))
                .sum(),
            false => children.len(),
        };
        tree.counts.insert(id.clone(), count);
    }
    if children.is_empty() && schema {
        children.push(empty_row(tree, &id));
    }
    let open =
        !children.is_empty() && (rows.filtering || rows.folds.get(&id).copied().unwrap_or(false));
    TreeItem::new(id, label)
        .expanded(open)
        .disabled(disabled)
        .children(children)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::db::{Catalog, Relation, RelationKind, Schema};
    use crate::explorer;

    fn project(name: &str, members: &[&str]) -> StoredProject {
        StoredProject {
            name: name.into(),
            connections: members.iter().map(|member| member.to_string()).collect(),
            ..Default::default()
        }
    }

    fn listed<'a>(id: &'a str, name: &'a str, content: Content) -> Listed<'a> {
        Listed {
            id,
            name,
            content,
            levels: Levels::Flat,
            database: None,
        }
    }

    fn unloaded(id: &str) -> Listed<'_> {
        listed(id, id, Content::Unloaded)
    }

    fn unloaded_named<'a>(id: &'a str, name: &'a str) -> Listed<'a> {
        listed(id, name, Content::Unloaded)
    }

    /// `main.birds`, as SQLite lists it.
    fn birds() -> ExplorerTree {
        explorer::tree(
            &Catalog {
                schemas: vec![Schema {
                    name: "main".into(),
                    relations: vec![Relation {
                        name: "birds".into(),
                        kind: RelationKind::Table,
                        partition_of: None,
                        size: None,
                        rows: None,
                    }],
                    routines: Vec::new(),
                    sequences: Vec::new(),
                }],
                database: None,
            },
            "",
        )
    }

    fn walk(items: &[TreeItem], ids: &mut Vec<String>) {
        for item in items {
            ids.push(item.id.to_string());
            walk(&item.children, ids);
        }
    }

    /// Every id in `items`, depth first.
    fn all_ids(items: &[TreeItem]) -> Vec<String> {
        let mut ids = Vec::new();
        walk(items, &mut ids);
        ids
    }

    fn row_ids(items: &[TreeItem]) -> Vec<String> {
        items.iter().map(|item| item.id.to_string()).collect()
    }

    fn search<'a>(items: &'a [TreeItem], id: &str) -> Option<&'a TreeItem> {
        items.iter().find_map(|item| match item.id.as_str() == id {
            true => Some(item),
            false => search(&item.children, id),
        })
    }

    fn find<'a>(items: &'a [TreeItem], id: &str) -> &'a TreeItem {
        search(items, id).unwrap_or_else(|| panic!("{id} is not a row"))
    }

    /// Every project listed, with nothing in front: what most tests mean.
    fn build(
        projects: &[StoredProject],
        listed: Vec<Listed<'_>>,
        folds: &HashMap<String, bool>,
    ) -> SidebarTree {
        super::build(projects, listed, folds, ListingScope::All, None, "")
    }

    #[test]
    fn connections_are_grouped_no_project_first_then_projects_in_their_order() {
        let projects = [project("P2", &["c"]), project("P1", &["a"])];
        let build_once = || {
            build(
                &projects,
                vec![unloaded("a"), unloaded("b"), unloaded("c")],
                &HashMap::new(),
            )
        };
        let tree = build_once();

        assert_eq!(
            row_ids(&tree.items),
            [NO_PROJECT_ROW, &project_row("P2"), &project_row("P1")]
        );
        for (group, member) in [
            (NO_PROJECT_ROW.to_string(), "b"),
            (project_row("P2"), "c"),
            (project_row("P1"), "a"),
        ] {
            assert_eq!(
                row_ids(&find(&tree.items, &group).children),
                [connection_row(member)]
            );
        }
        assert_eq!(all_ids(&tree.items), all_ids(&build_once().items));
    }

    #[test]
    fn without_projects_connections_are_listed_with_no_group_row() {
        let tree = build(&[], vec![unloaded("b"), unloaded("a")], &HashMap::new());

        assert_eq!(
            row_ids(&tree.items),
            [connection_row("b"), connection_row("a")]
        );
        assert!(!tree.nodes.values().any(|node| matches!(node, Node::Group)));
    }

    #[test]
    fn an_empty_project_is_listed_with_nothing_under_it() {
        let tree = build(
            &[project("Empty", &["gone"])],
            vec![unloaded("a")],
            &HashMap::new(),
        );

        assert_eq!(
            row_ids(&tree.items),
            [NO_PROJECT_ROW, &project_row("Empty")]
        );
        let empty = find(&tree.items, &project_row("Empty"));
        assert!(empty.children.is_empty());
        assert!(!empty.is_folder());
    }

    #[test]
    fn two_connections_with_the_same_objects_and_name_never_share_a_row_id() {
        let tree = build(
            &[],
            vec![
                listed("x", "Same", Content::Loaded(birds())),
                listed("y", "Same", Content::Loaded(birds())),
            ],
            &HashMap::new(),
        );

        let ids = all_ids(&tree.items);
        assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
        for profile in ["x", "y"] {
            let birds = scoped_row(profile, None, &explorer::relation_row("main", "birds"));
            assert!(ids.contains(&birds));
            assert!(matches!(
                tree.nodes.get(&birds),
                Some(Node::Row { leaf: Some(_) })
            ));
        }
    }

    #[test]
    fn a_scoped_row_id_reads_back_whatever_the_names_hold() {
        let names = ["a", "a/b", "1:x", "x:1/", "\0", "ü/ß", ""];
        for profile in names {
            for row in names {
                assert_eq!(
                    unscoped(&scoped_row(profile, None, row)),
                    Some(Scoped {
                        profile,
                        database: None,
                        inner: row
                    }),
                    "{profile:?} {row:?}"
                );
            }
            // A connection's own row is not a row under one.
            assert_eq!(unscoped(&connection_row(profile)), None);
        }
        assert_eq!(unscoped("connection:9:short/x"), None);
        assert_eq!(unscoped("schema-main"), None);
        // The same row under a length that is not the one it was written with.
        assert_eq!(unscoped("connection:+1:a/x"), None);
        assert_eq!(
            unscoped("connection:1:a/x"),
            Some(Scoped {
                profile: "a",
                database: None,
                inner: "x"
            })
        );
    }

    fn database_listed<'a>(
        id: &'a str,
        content: ExplorerTree,
        levels: Levels,
        database: &str,
    ) -> Listed<'a> {
        Listed {
            id,
            name: id,
            content: Content::Loaded(content),
            levels,
            database: Some(database.to_string()),
        }
    }

    /// What `profiles::explorer_content` gives for a connection holding a
    /// pinned `birds`, the saved query `q` and `main.birds`.
    fn content_with_shortcuts(database: &str, filter: &str) -> ExplorerTree {
        use crate::explorer::{Member, ObjectKind, ObjectRef, Pin, ShortcutDatabase, Shortcuts};

        let catalog = Catalog {
            schemas: vec![Schema {
                name: "main".into(),
                relations: vec![Relation {
                    name: "birds".into(),
                    kind: RelationKind::Table,
                    partition_of: None,
                    size: None,
                    rows: None,
                }],
                routines: Vec::new(),
                sequences: Vec::new(),
            }],
            database: None,
        };
        let shortcuts = Shortcuts {
            folders: Vec::new(),
            pins: vec![Pin::Member(Member::Object(ObjectRef {
                database: Some(database.to_string()),
                schema: "main".into(),
                name: "birds".into(),
                kind: ObjectKind::Relation(RelationKind::Table),
            }))],
        };
        let queries = ["birds q".to_string()];
        let mut content = explorer::shortcuts(
            &shortcuts,
            Some(&catalog),
            &queries,
            &ShortcutDatabase::On(database.to_string()),
            filter,
        );
        content.extend(explorer::queries(&queries, filter));
        content.extend(explorer::tree(&catalog, filter));
        content
    }

    #[test]
    fn a_database_then_schema_connection_lists_its_shortcuts_ahead_of_its_database() {
        let tree = build(
            &[],
            vec![database_listed(
                "a",
                content_with_shortcuts("d", "bir"),
                Levels::DatabaseThenSchema,
                "d",
            )],
            &HashMap::new(),
        );

        let connection = find(&tree.items, &connection_row("a"));
        assert_eq!(
            row_ids(&connection.children),
            [
                scoped_row("a", None, explorer::PINNED_GROUP),
                scoped_row("a", None, explorer::QUERIES_GROUP),
                database_row("a", "d"),
            ]
        );
        assert!(matches!(
            tree.nodes.get(&database_row("a", "d")),
            Some(Node::Database)
        ));
        let database = find(&tree.items, &database_row("a", "d"));
        let schema = scoped_row("a", Some("d"), "schema-main");
        assert_eq!(row_ids(&database.children), std::slice::from_ref(&schema));
        let birds = scoped_row("a", Some("d"), &explorer::relation_row("main", "birds"));
        assert!(search(&find(&tree.items, &schema).children, &birds).is_some());
        assert!(matches!(
            tree.nodes.get(&birds),
            Some(Node::Row { leaf: Some(_) })
        ));
        // What the filter matched and what a shortcut stands for are keyed by
        // the ids the rows were given, under the database or not.
        assert!(tree.highlights.contains_key(&birds));
        assert!(
            tree.shortcuts
                .keys()
                .all(|row| unscoped(row).is_some_and(|scoped| scoped.database.is_none()))
        );
        assert!(!tree.shortcuts.is_empty());
        let ids = all_ids(&tree.items);
        assert!(ids.iter().all(|id| tree.nodes.contains_key(id)));
    }

    #[test]
    fn a_database_then_schema_connection_with_no_known_database_lists_its_schemas_directly() {
        let tree = build(
            &[],
            vec![listed(
                "a",
                "a",
                Content::Loaded(content_with_shortcuts("d", "")),
            )]
            .into_iter()
            .map(|listed| Listed {
                levels: Levels::DatabaseThenSchema,
                ..listed
            })
            .collect(),
            &HashMap::new(),
        );

        let connection = find(&tree.items, &connection_row("a"));
        assert!(
            connection
                .children
                .iter()
                .any(|child| child.id.as_str() == scoped_row("a", None, "schema-main"))
        );
        assert!(
            !tree
                .nodes
                .values()
                .any(|node| matches!(node, Node::Database))
        );
    }

    #[test]
    fn row_ids_under_a_database_name_the_connection_and_the_database() {
        let names = ["a", "a/b", "1:x", "x:1/", "\0", "ü/ß", "database:1:x", ""];
        for profile in names {
            for database in names {
                for row in ["x", "a/b", "1:x", "\0", "ü/ß", "schema-main", ""] {
                    assert_eq!(
                        unscoped(&scoped_row(profile, Some(database), row)),
                        Some(Scoped {
                            profile,
                            database: Some(database),
                            inner: row
                        }),
                        "{profile:?} {database:?} {row:?}"
                    );
                }
                // A database's own row is not a row under one.
                assert_eq!(unscoped(&database_row(profile, database)), None);
            }
        }
        // A database named like a schema's id is still a database's.
        let spoof = scoped_row("a", Some("schema-main"), "x");
        assert_ne!(spoof, scoped_row("a", None, "schema-main"));
        assert_eq!(
            unscoped(&spoof).and_then(|scoped| scoped.database),
            Some("schema-main")
        );
        // The length is the one written, and a database name cannot lend its
        // tail to the next segment.
        assert_eq!(unscoped("connection:1:a/database:9:short/x"), None);
        assert_eq!(unscoped("connection:1:a/database:+1:d/x"), None);
        assert_eq!(unscoped("connection:1:a/database:1:d"), None);
    }

    #[test]
    fn a_database_holds_objects_connection_draws_its_one_schema_as_its_database() {
        let content = explorer::tree(
            &Catalog {
                schemas: vec![Schema {
                    name: "shop".into(),
                    relations: vec![Relation {
                        name: "orders".into(),
                        kind: RelationKind::Table,
                        partition_of: None,
                        size: None,
                        rows: None,
                    }],
                    routines: Vec::new(),
                    sequences: Vec::new(),
                }],
                database: None,
            },
            "",
        );
        let tree = build(
            &[],
            vec![database_listed(
                "a",
                content,
                Levels::DatabaseHoldsObjects,
                "shop",
            )],
            &HashMap::new(),
        );

        let connection = find(&tree.items, &connection_row("a"));
        assert_eq!(
            row_ids(&connection.children),
            [scoped_row("a", None, "schema-shop")]
        );
        assert!(
            !all_ids(&tree.items)
                .iter()
                .any(|id| id.contains("/database:"))
        );
        assert!(
            !tree
                .nodes
                .values()
                .any(|node| matches!(node, Node::Database))
        );
        assert!(schema_row_is_a_database(Levels::DatabaseHoldsObjects));
        assert!(schema_row_is_a_database(Levels::DatabaseIsSchema));
        assert!(!schema_row_is_a_database(Levels::DatabaseThenSchema));
        assert!(!schema_row_is_a_database(Levels::Flat));
    }

    fn relation(name: &str, kind: RelationKind, partition_of: Option<&str>) -> Relation {
        Relation {
            name: name.into(),
            kind,
            partition_of: partition_of.map(str::to_string),
            size: None,
            rows: None,
        }
    }

    fn schema(name: &str, relations: Vec<Relation>) -> Schema {
        Schema {
            name: name.into(),
            relations,
            routines: Vec::new(),
            sequences: Vec::new(),
        }
    }

    fn content_of(schemas: Vec<Schema>) -> ExplorerTree {
        explorer::tree(
            &Catalog {
                schemas,
                database: None,
            },
            "",
        )
    }

    fn count_of(tree: &SidebarTree, id: &str) -> Option<usize> {
        tree.counts.get(id).copied()
    }

    fn built(listed: Vec<Listed<'_>>) -> SidebarTree {
        build(&[], listed, &HashMap::new())
    }

    #[test]
    fn a_category_counts_its_top_level_rows_and_not_partitions() {
        let tree = built(vec![listed(
            "a",
            "a",
            Content::Loaded(content_of(vec![schema(
                "main",
                vec![
                    relation("p", RelationKind::Table, None),
                    relation("p_1", RelationKind::Table, Some("p")),
                    relation("p_2", RelationKind::Table, Some("p")),
                    relation("q", RelationKind::Table, None),
                    relation("v", RelationKind::View, None),
                ],
            )])),
        )]);

        let category = |label: &str| scoped_row("a", None, &format!("category-{label}-main"));
        assert_eq!(count_of(&tree, &category("Tables")), Some(2));
        assert_eq!(count_of(&tree, &category("Views")), Some(1));
        assert_eq!(count_of(&tree, &category("Dictionaries")), None);
        // A parent is one row, however many partitions it holds.
        let p = scoped_row("a", None, &explorer::relation_row("main", "p"));
        assert_eq!(count_of(&tree, &p), None);
    }

    #[test]
    fn a_schema_counts_the_objects_in_its_kinds_and_a_database_its_schemas() {
        let content = || {
            content_of(vec![
                schema(
                    "main",
                    vec![
                        relation("p", RelationKind::Table, None),
                        relation("p_1", RelationKind::Table, Some("p")),
                        relation("v", RelationKind::View, None),
                    ],
                ),
                schema("other", vec![relation("t", RelationKind::Table, None)]),
            ])
        };
        let tree = built(vec![database_listed(
            "a",
            content(),
            Levels::DatabaseThenSchema,
            "d",
        )]);

        assert_eq!(
            count_of(&tree, &scoped_row("a", Some("d"), "schema-main")),
            Some(2)
        );
        assert_eq!(
            count_of(&tree, &scoped_row("a", Some("d"), "schema-other")),
            Some(1)
        );
        assert_eq!(count_of(&tree, &database_row("a", "d")), Some(2));
        assert_eq!(count_of(&tree, &connection_row("a")), Some(1));

        // Where schemas are databases, a connection counts them.
        let tree = built(vec![database_listed(
            "a",
            content(),
            Levels::DatabaseIsSchema,
            "d",
        )]);
        assert_eq!(count_of(&tree, &connection_row("a")), Some(2));

        // With no database level there is nothing for a connection to count.
        let tree = built(vec![listed("a", "a", Content::Loaded(content()))]);
        assert_eq!(count_of(&tree, &connection_row("a")), None);
        assert_eq!(
            count_of(&tree, &scoped_row("a", None, "schema-main")),
            Some(2)
        );
    }

    #[test]
    fn nothing_is_counted_before_it_loads() {
        let tree = built(vec![
            database_listed(
                "unloaded",
                ExplorerTree::default(),
                Levels::DatabaseThenSchema,
                "d",
            ),
            listed("loading", "loading", Content::Loading),
            listed("failed", "failed", Content::Failed("it failed".into())),
        ]);
        let tree_with = |content: Content| {
            let mut one = listed("x", "x", content);
            one.levels = Levels::DatabaseThenSchema;
            built(vec![one])
        };

        for content in [
            Content::Unloaded,
            Content::Loading,
            Content::Failed("no".into()),
        ] {
            let tree = tree_with(content);
            assert!(tree.counts.is_empty(), "{:?}", tree.counts);
        }
        // A flat connection's own row is never counted, loaded or not.
        let flat = built(vec![listed("f", "f", Content::Loaded(birds()))]);
        assert_eq!(count_of(&flat, &connection_row("f")), None);
        // Nor is a notice.
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(id, node)| matches!(node, Node::Notice) && tree.counts.contains_key(id))
        );
    }

    #[test]
    fn a_loaded_empty_schema_counts_zero_and_says_so() {
        let tree = built(vec![listed(
            "a",
            "a",
            Content::Loaded(content_of(vec![schema("empty", Vec::new())])),
        )]);

        let id = scoped_row("a", None, "schema-empty");
        assert_eq!(count_of(&tree, &id), Some(0));
        let row = find(&tree.items, &id);
        assert_eq!(row.children.len(), 1);
        assert_eq!(row.children[0].label.as_str(), NO_OBJECTS);
        assert!(row.children[0].is_disabled());
        assert!(matches!(
            tree.nodes.get(row.children[0].id.as_str()),
            Some(Node::Notice)
        ));
        // The row that says so is not itself counted, and shares no id with
        // a row of the catalog.
        assert_eq!(count_of(&tree, row.children[0].id.as_str()), None);
        let ids = all_ids(&tree.items);
        assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
    }

    #[test]
    fn counts_are_whole_numbers_whatever_their_size() {
        let relations = (0..100_000)
            .map(|at| relation(&format!("t{at}"), RelationKind::Table, None))
            .collect();
        let tree = built(vec![listed(
            "a",
            "a",
            Content::Loaded(content_of(vec![schema("main", relations)])),
        )]);

        assert_eq!(
            count_of(&tree, &scoped_row("a", None, "category-Tables-main")),
            Some(100_000)
        );
        assert_eq!(
            count_of(&tree, &scoped_row("a", None, "schema-main")),
            Some(100_000)
        );
    }

    #[test]
    fn counts_do_not_depend_on_row_order() {
        let relations = vec![
            relation("b", RelationKind::Table, None),
            relation("b_1", RelationKind::Table, Some("b")),
            relation("a", RelationKind::View, None),
            relation("c", RelationKind::Table, None),
        ];
        let mut reversed = relations.clone();
        reversed.reverse();
        let counts = |relations| {
            built(vec![listed(
                "a",
                "a",
                Content::Loaded(content_of(vec![
                    schema("z", Vec::new()),
                    schema("main", relations),
                ])),
            )])
            .counts
        };

        let forward = counts(relations);
        assert_eq!(forward, counts(reversed));
        assert!(!forward.is_empty());
    }

    #[test]
    fn group_rows_count_their_members_and_project_rows_their_connections() {
        use crate::explorer::{
            Folder, Member, ObjectKind, ObjectRef, Pin, ShortcutDatabase, Shortcuts,
        };

        let catalog = Catalog {
            schemas: vec![schema(
                "main",
                vec![
                    relation("birds", RelationKind::Table, None),
                    relation("fish", RelationKind::Table, None),
                ],
            )],
            database: None,
        };
        let object = |name: &str| {
            Member::Object(ObjectRef {
                database: None,
                schema: "main".into(),
                name: name.into(),
                kind: ObjectKind::Relation(RelationKind::Table),
            })
        };
        let shortcuts = Shortcuts {
            folders: vec![Folder {
                name: "Wet".into(),
                members: vec![object("fish"), object("birds")],
            }],
            pins: vec![Pin::Member(object("birds"))],
        };
        let queries = ["one".to_string(), "two".to_string(), "three".to_string()];
        let mut content = explorer::shortcuts(
            &shortcuts,
            Some(&catalog),
            &queries,
            &ShortcutDatabase::Unnamed,
            "",
        );
        content.extend(explorer::queries(&queries, ""));
        content.extend(explorer::tree(&catalog, ""));
        let projects = [project("Zoo", &["a", "b"]), project("Empty", &[])];
        let tree = build(
            &projects,
            vec![
                listed("a", "a", Content::Loaded(content)),
                listed("b", "b", Content::Unloaded),
                listed("c", "c", Content::Unloaded),
            ],
            &HashMap::new(),
        );

        assert_eq!(
            count_of(&tree, &scoped_row("a", None, explorer::PINNED_GROUP)),
            Some(1)
        );
        assert_eq!(
            count_of(&tree, &scoped_row("a", None, explorer::QUERIES_GROUP)),
            Some(3)
        );
        assert_eq!(
            count_of(&tree, &scoped_row("a", None, &explorer::folder_row("Wet"))),
            Some(2)
        );
        assert_eq!(count_of(&tree, &project_row("Zoo")), Some(2));
        assert_eq!(count_of(&tree, &project_row("Empty")), Some(0));
        assert_eq!(count_of(&tree, NO_PROJECT_ROW), Some(1));
        // A member of a folder is one row, not a group.
        assert!(
            tree.counts
                .keys()
                .all(|id| !id.contains("relation-main\0birds"))
        );
    }

    fn filtered(projects: &[StoredProject], listed: Vec<Listed<'_>>, filter: &str) -> SidebarTree {
        super::build(
            projects,
            listed,
            &HashMap::new(),
            ListingScope::All,
            None,
            filter,
        )
    }

    /// The rows of a catalog holding `names` as tables, narrowed by `filter` as
    /// the caller narrows a connection's.
    fn tables(names: &[&str], filter: &str) -> ExplorerTree {
        explorer::tree(
            &Catalog {
                schemas: vec![schema(
                    "main",
                    names
                        .iter()
                        .map(|name| relation(name, RelationKind::Table, None))
                        .collect(),
                )],
                database: None,
            },
            filter,
        )
    }

    #[test]
    fn a_filter_keeps_connections_whose_loaded_rows_or_name_match() {
        let tree = filtered(
            &[],
            vec![
                listed("a", "Zoo", Content::Loaded(tables(&["birds"], "bir"))),
                listed("b", "Pond", Content::Loaded(tables(&["fish"], "bir"))),
            ],
            "bir",
        );

        assert_eq!(row_ids(&tree.items), [connection_row("a")]);
        let zoo = find(&tree.items, &connection_row("a"));
        assert!(zoo.is_expanded());
        let schema = find(&tree.items, &scoped_row("a", None, "schema-main"));
        let category = find(&tree.items, &scoped_row("a", None, "category-Tables-main"));
        assert!(schema.is_expanded() && category.is_expanded());
        let birds = scoped_row("a", None, &explorer::relation_row("main", "birds"));
        assert!(search(&category.children, &birds).is_some());
        assert!(tree.highlights.contains_key(&birds));

        // A name that matches keeps the connection with every row it has,
        // which the caller reads whole.
        let name = "Birdhouse";
        assert!(keeps_rows_whole("bir", name, Levels::Flat, None));
        assert!(!keeps_rows_whole("bir", "Pond", Levels::Flat, None));
        let tree = filtered(
            &[],
            vec![
                listed("c", name, Content::Loaded(tables(&["fish", "eel"], ""))),
                listed("b", "Pond", Content::Loaded(tables(&["fish"], "bir"))),
            ],
            "bir",
        );
        assert_eq!(row_ids(&tree.items), [connection_row("c")]);
        let fish = scoped_row("c", None, &explorer::relation_row("main", "fish"));
        assert!(search(&tree.items, &fish).is_some());
        assert!(tree.highlights.contains_key(&connection_row("c")));
    }

    #[test]
    fn an_unloaded_connection_is_kept_only_by_its_name_and_stays_folded() {
        let open = HashMap::from([(connection_row("u"), true)]);
        let with = |filter: &str| {
            super::build(
                &[],
                vec![unloaded_named("u", "Flocks")],
                &open,
                ListingScope::All,
                None,
                filter,
            )
        };

        assert!(with("bir").items.is_empty());
        let kept = with("flo");
        let row = find(&kept.items, &connection_row("u"));
        assert!(!row.is_expanded());
        assert_eq!(row.children.len(), 1);
        assert_eq!(row.children[0].label.as_str(), LOADING);
        assert!(kept.highlights.contains_key(&connection_row("u")));
        // No text keeps every connection, as before.
        assert_eq!(with("").items.len(), 1);
    }

    #[test]
    fn while_filtering_counts_are_of_the_rows_kept() {
        let table_count = |filter: &str| {
            let tree = filtered(
                &[],
                vec![listed(
                    "a",
                    "Zoo",
                    Content::Loaded(tables(&["birds", "bison", "fish"], filter)),
                )],
                filter,
            );
            count_of(&tree, &scoped_row("a", None, "category-Tables-main"))
        };

        assert_eq!(table_count("bi"), Some(2));
        assert_eq!(table_count(""), Some(3));
    }

    #[test]
    fn the_filter_highlights_the_matched_characters_of_connection_and_database_names() {
        let content = || tables(&["fish"], "");
        let zoo = project("Birdy", &["a"]);
        let tree = filtered(
            &[zoo],
            vec![database_listed(
                "a",
                content(),
                Levels::DatabaseThenSchema,
                "birds_db",
            )],
            "bird",
        );

        // Each name the filter matched is lit where it matched, and keeps what
        // is under it.
        for id in [project_row("Birdy"), database_row("a", "birds_db")] {
            let lit = tree.highlights.get(&id).expect("the name is lit");
            assert_eq!(
                (lit.first().map(|r| r.start), lit.last().map(|r| r.end)),
                (Some(0), Some(4)),
                "{id}"
            );
        }
        assert!(search(&tree.items, &database_row("a", "birds_db")).is_some());
        // The connection's own name did not match, so only its rows do.
        assert!(!tree.highlights.contains_key(&connection_row("a")));
        assert!(keeps_rows_whole(
            "bird",
            "a",
            Levels::DatabaseThenSchema,
            Some("birds_db")
        ));
        assert!(!keeps_rows_whole(
            "bird",
            "a",
            Levels::Flat,
            Some("birds_db")
        ));
    }

    #[test]
    fn an_idle_connection_is_folded_whatever_its_stored_fold_says() {
        let folds = HashMap::from([(connection_row("a"), true)]);
        let tree = build(&[], vec![unloaded("a")], &folds);

        let row = find(&tree.items, &connection_row("a"));
        assert!(!row.is_expanded());
        assert_eq!(row.children.len(), 1);
        let loading = &row.children[0];
        assert_eq!(loading.id.as_str(), scoped_row("a", None, "loading"));
        assert_eq!(loading.label.as_str(), LOADING);
        assert!(loading.is_disabled());
    }

    #[test]
    fn a_loading_connection_follows_its_fold_and_holds_one_disabled_loading_row() {
        let open = HashMap::from([(connection_row("a"), true)]);
        for (folds, expanded) in [(&open, true), (&HashMap::new(), false)] {
            let tree = build(&[], vec![listed("a", "a", Content::Loading)], folds);

            let row = find(&tree.items, &connection_row("a"));
            assert_eq!(row.is_expanded(), expanded);
            assert_eq!(row.children.len(), 1);
            assert_eq!(row.children[0].label.as_str(), LOADING);
            assert!(row.children[0].is_disabled());
        }
    }

    #[test]
    fn a_failed_connection_holds_one_disabled_row_saying_what_happened() {
        let text = "Connection refused: nothing is listening on `h:1`";
        let tree = build(
            &[],
            vec![listed("a", "a", Content::Failed(text.into()))],
            &HashMap::new(),
        );

        let row = find(&tree.items, &connection_row("a"));
        assert_eq!(row.children.len(), 1);
        assert_eq!(row.children[0].label.as_str(), text);
        assert!(row.children[0].is_disabled());
        assert!(matches!(
            tree.nodes.get(row.children[0].id.as_str()),
            Some(Node::Notice)
        ));
    }

    #[test]
    fn a_loaded_connection_with_nothing_in_it_says_so_in_a_row_of_its_own() {
        let tree = build(
            &[],
            vec![listed("a", "a", Content::Loaded(ExplorerTree::default()))],
            &HashMap::new(),
        );

        let row = find(&tree.items, &connection_row("a"));
        assert_eq!(row.children.len(), 1);
        assert_eq!(row.children[0].label.as_str(), "No database objects found.");
        assert!(row.children[0].is_disabled());
    }

    #[test]
    fn rows_under_a_connection_start_folded_and_keep_the_folds_they_are_given() {
        let schema = scoped_row("a", None, "schema-main");
        let category = scoped_row("a", None, "category-Tables-main");
        let build_with = |folds: &HashMap<String, bool>| {
            build(&[], vec![listed("a", "a", Content::Loaded(birds()))], folds)
        };

        let folded = build_with(&HashMap::new());
        assert!(!find(&folded.items, &schema).is_expanded());
        assert!(!find(&folded.items, &category).is_expanded());

        let opened = build_with(&HashMap::from([(schema.clone(), true)]));
        assert!(find(&opened.items, &schema).is_expanded());
        assert!(!find(&opened.items, &category).is_expanded());
    }

    #[test]
    fn the_project_in_front_decides_and_no_project_follows_the_setting() {
        let with = |slug: &str| StoredProject {
            view_mode: Some(slug.into()),
            ..project("Zoo", &[])
        };
        assert_eq!(
            effective_view_mode(ViewMode::Simple, Some(&with("complex"))),
            ViewMode::Complex
        );
        assert_eq!(
            effective_view_mode(ViewMode::Complex, Some(&with("simple"))),
            ViewMode::Simple
        );
        for global in [ViewMode::Simple, ViewMode::Complex] {
            // No choice of its own, one this build cannot read, and no project
            // at all, each follow the setting.
            assert_eq!(
                effective_view_mode(global, Some(&project("Zoo", &[]))),
                global
            );
            assert_eq!(effective_view_mode(global, Some(&with("tree"))), global);
            assert_eq!(effective_view_mode(global, None), global);
        }
    }

    #[test]
    fn the_current_scope_lists_only_the_front_group_with_no_group_row() {
        let projects = [project("Zoo", &["b"]), project("Other", &["c"])];
        let tree_for = |current| {
            super::build(
                &projects,
                vec![unloaded("a"), unloaded("b"), unloaded("c")],
                &HashMap::new(),
                ListingScope::Current,
                current,
                "",
            )
        };

        let zoo = tree_for(Some("Zoo"));
        assert_eq!(row_ids(&zoo.items), [connection_row("b")]);
        assert!(!zoo.nodes.values().any(|node| matches!(node, Node::Group)));

        let none = tree_for(None);
        assert_eq!(row_ids(&none.items), [connection_row("a")]);
        assert!(!none.nodes.values().any(|node| matches!(node, Node::Group)));
    }

    #[test]
    fn with_every_project_listed_only_the_current_group_starts_open() {
        let projects = [project("Zoo", &["b"]), project("Other", &["c"])];
        let open_in = |folds: &HashMap<String, bool>, current, row: &str| {
            let tree = super::build(
                &projects,
                vec![unloaded("a"), unloaded("b"), unloaded("c")],
                folds,
                ListingScope::All,
                current,
                "",
            );
            find(&tree.items, row).is_expanded()
        };
        let none = HashMap::new();

        assert!(open_in(&none, Some("Zoo"), &project_row("Zoo")));
        assert!(!open_in(&none, Some("Zoo"), NO_PROJECT_ROW));
        assert!(!open_in(&none, Some("Zoo"), &project_row("Other")));
        // With no project in front the connections in none are the current group.
        assert!(open_in(&none, None, NO_PROJECT_ROW));
        assert!(!open_in(&none, None, &project_row("Zoo")));

        // A fold the caller passes wins either way.
        let opened = HashMap::from([(project_row("Other"), true)]);
        assert!(open_in(&opened, Some("Zoo"), &project_row("Other")));
        let closed = HashMap::from([(project_row("Zoo"), false)]);
        assert!(!open_in(&closed, Some("Zoo"), &project_row("Zoo")));
    }

    #[test]
    fn the_listing_scope_follows_the_project_in_front() {
        let with = |slug: &str| StoredProject {
            listing_scope: Some(slug.into()),
            ..project("Zoo", &[])
        };
        assert_eq!(
            effective_listing_scope(ListingScope::All, Some(&with("current"))),
            ListingScope::Current
        );
        assert_eq!(
            effective_listing_scope(ListingScope::Current, Some(&with("all"))),
            ListingScope::All
        );
        for global in [ListingScope::All, ListingScope::Current] {
            assert_eq!(
                effective_listing_scope(global, Some(&project("Zoo", &[]))),
                global
            );
            assert_eq!(effective_listing_scope(global, Some(&with("some"))), global);
            assert_eq!(effective_listing_scope(global, None), global);
        }
        for scope in [ListingScope::All, ListingScope::Current] {
            assert_eq!(ListingScope::from_slug(Some(scope.slug())), scope);
        }
        assert_eq!(ListingScope::from_slug(None), ListingScope::All);
    }

    #[test]
    fn the_view_mode_reads_back_from_its_slug_and_defaults_to_simple() {
        assert_eq!(ViewMode::from_slug(Some("complex")), ViewMode::Complex);
        assert_eq!(ViewMode::from_slug(Some("simple")), ViewMode::Simple);
        assert_eq!(ViewMode::from_slug(None), ViewMode::Simple);
        assert_eq!(ViewMode::from_slug(Some("tree")), ViewMode::Simple);
        for mode in [ViewMode::Simple, ViewMode::Complex] {
            assert_eq!(ViewMode::from_slug(Some(mode.slug())), mode);
        }
    }
}

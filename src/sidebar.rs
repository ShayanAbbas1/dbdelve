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

use crate::explorer::{ExplorerLeaf, ExplorerTree, Shortcut};
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

    /// The mode a stored slug names. A file with none, or one this build does
    /// not know, reads as Simple: what every file written before it showed.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        match slug {
            Some("complex") => ViewMode::Complex,
            _ => ViewMode::Simple,
        }
    }
}

/// The row of connections that belong to no project, listed first while any
/// project exists.
pub(crate) const NO_PROJECT_ROW: &str = "no-project";

/// A project's row id. The name follows its length, as a folder's does.
pub(crate) fn project_row(name: &str) -> String {
    format!("project:{}:{name}", name.len())
}

/// A connection's row id, by the profile's id.
pub(crate) fn connection_row(profile: &str) -> String {
    format!("connection:{}:{profile}", profile.len())
}

/// The id of `row`, one of simple mode's rows, listed under `profile`. The
/// profile's length keeps a `/` in either from moving the seam.
pub(crate) fn scoped_row(profile: &str, row: &str) -> String {
    format!("{}/{row}", connection_row(profile))
}

/// The profile and the simple-mode row id a [`scoped_row`] was made from.
/// `None` for any other id, including a connection's own row.
pub(crate) fn unscoped(id: &str) -> Option<(&str, &str)> {
    let (length, rest) = id.strip_prefix("connection:")?.split_once(':')?;
    let profile = rest.get(..length.parse().ok()?)?;
    let row = rest.get(profile.len()..)?.strip_prefix('/')?;
    // Read back the way it was written: `+3` parses as a length, and would
    // otherwise name the same row as `3`.
    (scoped_row(profile, row) == id).then_some((profile, row))
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
}

/// What a row stands for.
pub(crate) enum Node {
    /// A project, or the connections in none.
    Group,
    Connection {
        profile: String,
    },
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
}

/// The rows for `listed`, grouped under `projects` the way the palette's
/// Connections scope orders them: the connections in no project first, and
/// only while a project exists, then each project in its stored order, every
/// connection in the order `listed` has it. Nothing is sorted, so two builds
/// over the same input give the same rows.
///
/// `folds` is whether each row was open in the tree this one replaces, by
/// row id, so a rebuild puts the user's folding back.
pub(crate) fn build(
    projects: &[StoredProject],
    listed: Vec<Listed<'_>>,
    folds: &HashMap<String, bool>,
) -> SidebarTree {
    let mut tree = SidebarTree::default();
    // One bucket per project, and the first for the connections in none. A
    // connection a second project also names stays under the first, as it
    // does in the palette.
    let mut buckets: Vec<Vec<TreeItem>> = vec![Vec::new(); projects.len() + 1];
    for connection in listed {
        let bucket = projects
            .iter()
            .position(|project| project.connections.iter().any(|id| id == connection.id))
            .map_or(0, |at| at + 1);
        buckets[bucket].push(connection_item(&mut tree, connection, folds));
    }
    if projects.is_empty() {
        tree.items = buckets.swap_remove(0);
        return tree;
    }
    let mut buckets = buckets.into_iter();
    // A group with nothing in it is a row that only says so, which is wanted
    // of a project the user made and not of the absence of one.
    let unlisted = buckets.next().unwrap_or_default();
    let groups = (!unlisted.is_empty())
        .then(|| (NO_PROJECT_ROW.to_string(), "No project", unlisted))
        .into_iter()
        .chain(projects.iter().zip(buckets).map(|(project, members)| {
            (project_row(&project.name), project.name.as_str(), members)
        }));
    for (id, label, members) in groups {
        tree.nodes.insert(id.clone(), Node::Group);
        let open = folds.get(&id).copied().unwrap_or(true);
        tree.items.push(
            TreeItem::new(id, label.to_string())
                .expanded(open)
                .children(members),
        );
    }
    tree
}

fn connection_item(
    tree: &mut SidebarTree,
    connection: Listed<'_>,
    folds: &HashMap<String, bool>,
) -> TreeItem {
    let id = connection_row(connection.id);
    tree.nodes.insert(
        id.clone(),
        Node::Connection {
            profile: connection.id.to_string(),
        },
    );
    let followed = folds.get(&id).copied().unwrap_or(false);
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
        Content::Loaded(content) if content.items.is_empty() => (
            followed,
            vec![notice(
                tree,
                connection.id,
                "notice",
                "No database objects found.",
            )],
        ),
        Content::Loaded(content) => {
            let ExplorerTree {
                items,
                leaves,
                highlights,
                shortcuts,
            } = content;
            let children = items
                .into_iter()
                .map(|item| scoped_item(tree, connection.id, item, &leaves, folds))
                .collect();
            for (row, ranges) in highlights {
                tree.highlights
                    .insert(scoped_row(connection.id, &row), ranges);
            }
            for (row, shortcut) in shortcuts {
                tree.shortcuts
                    .insert(scoped_row(connection.id, &row), shortcut);
            }
            (followed, children)
        }
    };
    TreeItem::new(id, connection.name.to_string())
        .expanded(open)
        .children(children)
}

/// A row that says something about its connection and opens nothing. Its id
/// is one no simple-mode row can have.
fn notice(tree: &mut SidebarTree, profile: &str, row: &str, text: &str) -> TreeItem {
    let id = scoped_row(profile, row);
    tree.nodes.insert(id.clone(), Node::Notice);
    TreeItem::new(id, text.to_string()).disabled(true)
}

/// `item` and everything under it with its id scoped to `profile`. A folder
/// is folded unless `folds` says it was open: a connect must not unfold the
/// thousands of rows a catalog can hold.
fn scoped_item(
    tree: &mut SidebarTree,
    profile: &str,
    mut item: TreeItem,
    leaves: &HashMap<String, ExplorerLeaf>,
    folds: &HashMap<String, bool>,
) -> TreeItem {
    let id = scoped_row(profile, &item.id);
    tree.nodes.insert(
        id.clone(),
        Node::Row {
            leaf: leaves.get(item.id.as_str()).copied(),
        },
    );
    let folder = item.is_folder();
    let disabled = item.is_disabled();
    let label = item.label.clone();
    let children: Vec<TreeItem> = std::mem::take(&mut item.children)
        .into_iter()
        .map(|child| scoped_item(tree, profile, child, leaves, folds))
        .collect();
    let open = folder && folds.get(&id).copied().unwrap_or(false);
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
        }
    }

    fn listed<'a>(id: &'a str, name: &'a str, content: Content) -> Listed<'a> {
        Listed { id, name, content }
    }

    fn unloaded(id: &str) -> Listed<'_> {
        listed(id, id, Content::Unloaded)
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
            let birds = scoped_row(profile, &explorer::relation_row("main", "birds"));
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
                    unscoped(&scoped_row(profile, row)),
                    Some((profile, row)),
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
        assert_eq!(unscoped("connection:1:a/x"), Some(("a", "x")));
    }

    #[test]
    fn an_idle_connection_is_folded_whatever_its_stored_fold_says() {
        let folds = HashMap::from([(connection_row("a"), true)]);
        let tree = build(&[], vec![unloaded("a")], &folds);

        let row = find(&tree.items, &connection_row("a"));
        assert!(!row.is_expanded());
        assert_eq!(row.children.len(), 1);
        let loading = &row.children[0];
        assert_eq!(loading.id.as_str(), scoped_row("a", "loading"));
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
        let schema = scoped_row("a", "schema-main");
        let category = scoped_row("a", "category-Tables-main");
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

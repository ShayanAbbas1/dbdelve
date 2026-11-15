//! What the connections sidebar lists, as plain data.
//!
//! Complex mode draws every connection under its project, and each
//! connection's own rows are the ones simple mode draws for it. Two rules hold
//! the model together. Ids are length-prefixed, so no name -- a profile's, a
//! project's, a table's -- can run on into the next segment and be read as
//! another row's. And [`build`] takes loaded data only: it neither connects,
//! reads a catalog nor sees a window, so what a row says is what the caller
//! had already been told.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use gpui_component::tree::TreeItem;

use crate::db::{Levels, RelationKind, Target};
use crate::explorer::{
    Builder, ExplorerLeaf, ExplorerTree, RELATION_CATEGORIES, Shortcut, is_file_row, is_schema_row,
    saved_query,
};
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

/// How tall the connections tree's rows are and how far each level steps in.
/// The simple tree keeps its own rows whatever this is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Density {
    Compact,
    #[default]
    Standard,
    Comfortable,
}

impl Density {
    pub(crate) const ALL: [Density; 3] =
        [Density::Compact, Density::Standard, Density::Comfortable];

    /// How `profiles.toml` spells it.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Density::Compact => "compact",
            Density::Standard => "standard",
            Density::Comfortable => "comfortable",
        }
    }

    /// The density a stored slug names; none, or one this build does not
    /// know, is the standard one.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        Self::ALL
            .into_iter()
            .find(|density| Some(density.slug()) == slug)
            .unwrap_or_default()
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Density::Compact => "Compact",
            Density::Standard => "Standard",
            Density::Comfortable => "Comfortable",
        }
    }

    /// A row's height before zoom.
    pub(crate) fn row_height(self) -> f32 {
        match self {
            Density::Compact => 22.,
            Density::Standard => 26.,
            Density::Comfortable => 30.,
        }
    }

    /// What each level of depth adds to a row's indent, before zoom.
    pub(crate) fn indent(self) -> f32 {
        match self {
            Density::Compact => 12.,
            Density::Standard => 14.,
            Density::Comfortable => 16.,
        }
    }
}

/// The size a row draws: the one it has, while sizes are shown.
pub(crate) fn shown_size(size: Option<u64>, show_sizes: bool) -> Option<u64> {
    size.filter(|_| show_sizes)
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

/// Whether `filter` matches the name of `database`, so the rows of it are
/// kept whole.
pub(crate) fn database_kept_whole(filter: &str, database: &str) -> bool {
    Builder::new(filter).name_matches("", database)
}

/// The database the catalog's rows sit under in the tree, if its shape draws
/// one over them.
fn catalog_database(levels: Levels, database: Option<&str>) -> Option<&str> {
    database.filter(|_| levels == Levels::DatabaseThenSchema)
}

/// What a catalog with no rows to list says: that none was found, or how many
/// the filter hid. Both trees say it, so it is built once.
pub(crate) fn empty_catalog_line(levels: Levels, left_out: usize) -> String {
    if left_out == 0 {
        return "No database objects found.".to_string();
    }
    let noun = match levels {
        Levels::DatabaseThenSchema => "schema",
        Levels::Flat | Levels::DatabaseIsSchema | Levels::DatabaseHoldsObjects => "database",
    };
    let plural = if left_out == 1 { "" } else { "s" };
    format!("No database objects shown: the filter hides {left_out} {noun}{plural}.")
}

/// The id the tree gives `row`, one of the rows simple mode lists for the tab
/// in front of a connection: an object's sits under the database where the
/// connection has one, a saved query's or a file's under the connection.
pub(crate) fn listed_row(
    profile: &str,
    levels: Levels,
    database: Option<&str>,
    row: &str,
) -> String {
    let database = match saved_query(row).is_some() || is_file_row(row) {
        true => None,
        false => catalog_database(levels, database),
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
    /// The other databases of its server, in the order the server listed
    /// them, with how much of each is known. Empty unless the shape draws
    /// databases over its schemas or objects.
    pub(crate) databases: Vec<(String, Content)>,
    /// How many databases of its server the profile's filter left out of
    /// `databases`.
    pub(crate) databases_left_out: usize,
}

/// The databases of a server that are not `current`, the one the connection
/// is in, in the order the server listed them: the rows listed beside the
/// connection's own database, which appears once whatever the engine.
pub(crate) fn other_databases<'a>(names: &'a [String], current: Option<&str>) -> Vec<&'a str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|name| Some(*name) != current)
        .collect()
}

/// What a row stands for.
pub(crate) enum Node {
    /// A project, or the connections in none.
    Group,
    Connection {
        profile: String,
    },
    /// A connection's database, the connection's own or another of its
    /// server's.
    Database {
        profile: String,
        database: String,
    },
    /// Says what happened to its connection, and is never acted on.
    Notice,
    /// One of simple mode's rows, its leaf when it opens an object.
    Row {
        leaf: Option<ExplorerLeaf>,
    },
}

/// What the sidebar's row is, as far as an action on it needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Connection,
    Database,
    Schema,
    Relation(RelationKind),
    Routine,
    Sequence,
}

impl NodeKind {
    /// What a number of them are called, as the tree's categories call them.
    fn plural(self) -> String {
        match self {
            NodeKind::Connection => "connections".into(),
            NodeKind::Database => "databases".into(),
            NodeKind::Schema => "schemas".into(),
            NodeKind::Routine => "routines".into(),
            NodeKind::Sequence => "sequences".into(),
            NodeKind::Relation(kind) => RELATION_CATEGORIES
                .iter()
                .find(|(of, _)| *of == kind)
                .map_or_else(|| "items".into(), |(_, label)| label.to_lowercase()),
        }
    }
}

/// One thing a node offers, from its menu and the palette alike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NodeAction {
    NewQueryHere,
    /// The name as the catalog lists it, so what is copied is what is read.
    CopyName(String),
    ShowDdl,
    Refresh,
}

/// What a node of `kind` named `name` offers, in the order its menu lists
/// them. The one list both trees' menus and the palette read, as
/// [`crate::explorer::relation_copies`] is for the copies.
pub(crate) fn node_actions(kind: NodeKind, name: &str, target: Target) -> Vec<NodeAction> {
    let copy = NodeAction::CopyName(name.to_string());
    match kind {
        NodeKind::Connection | NodeKind::Database | NodeKind::Schema => {
            vec![NodeAction::NewQueryHere, copy, NodeAction::Refresh]
        }
        // A target with no definition to write out has none to show.
        NodeKind::Relation(_) | NodeKind::Sequence if target.copies_ddl() => {
            vec![copy, NodeAction::ShowDdl]
        }
        NodeKind::Relation(_) | NodeKind::Sequence => vec![copy],
        NodeKind::Routine => vec![copy, NodeAction::ShowDdl],
    }
}

impl NodeAction {
    /// What a menu and the palette call it.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            NodeAction::NewQueryHere => "New Query Here",
            NodeAction::CopyName(_) => "Copy Name",
            NodeAction::ShowDdl => "Show DDL",
            NodeAction::Refresh => "Refresh",
        }
    }
}

/// A node of either tree, with what an action on it needs: the connection it
/// belongs to, which comes to the front first, and where it sits in it. A
/// palette row carries one, so what it acts on is what it was built for and
/// not whatever the sidebar has selected when it is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectedNode {
    pub(crate) profile: String,
    /// The database the connections tree lists the node under, as its row id
    /// says it; `None` in simple mode, and on a connection's own row.
    pub(crate) database: Option<String>,
    pub(crate) kind: NodeKind,
    /// As the catalog lists it, never quoted or qualified.
    pub(crate) name: String,
    /// The schema an object is in.
    pub(crate) schema: Option<String>,
    pub(crate) leaf: Option<ExplorerLeaf>,
}

/// The rows of the connections tree the user has selected, by row id, in the
/// order they were picked. The tree selects one row of its own; this is the
/// set a modifier click builds around it, and `anchor` is where a Shift-click
/// range starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) rows: Vec<String>,
    pub(crate) anchor: Option<String>,
}

impl Selection {
    /// A plain click: the row is the whole selection.
    pub(crate) fn click(&mut self, id: &str) {
        self.rows = vec![id.to_string()];
        self.anchor = Some(id.to_string());
    }

    /// A Cmd- or Ctrl-click: the row joins the selection, or leaves it.
    pub(crate) fn toggle(&mut self, id: &str) {
        match self.rows.iter().position(|row| row == id) {
            Some(at) => _ = self.rows.remove(at),
            None => self.rows.push(id.to_string()),
        }
        self.anchor = Some(id.to_string());
    }

    /// A Shift-click: every row of `visible` from the anchor to `id`, the
    /// anchor staying where it is. With no anchor, or one that is not
    /// showing, it is a plain click.
    pub(crate) fn extend_to(&mut self, id: &str, visible: &[String]) {
        let at = |wanted: &str| visible.iter().position(|row| row == wanted);
        match self.anchor.as_deref().and_then(at).zip(at(id)) {
            Some((from, to)) => self.rows = visible[from.min(to)..=from.max(to)].to_vec(),
            None => self.click(id),
        }
    }

    /// Forget the rows a rebuild no longer lists.
    pub(crate) fn retain(&mut self, present: &HashSet<String>) {
        self.rows.retain(|row| present.contains(row));
        self.anchor = self.anchor.take().filter(|anchor| present.contains(anchor));
    }
}

/// What a selection of several rows offers, from its menu and the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionAction {
    OpenAll,
    CopyNames,
    CopyDdl,
    ShowDdl,
    Pin,
    Refresh,
}

impl SelectionAction {
    /// What a menu and the palette call it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SelectionAction::OpenAll => "Open All",
            SelectionAction::CopyNames => "Copy Names",
            SelectionAction::CopyDdl => "Copy DDL",
            SelectionAction::ShowDdl => "Show DDL",
            SelectionAction::Pin => "Pin",
            SelectionAction::Refresh => "Refresh",
        }
    }
}

/// What every one of `items` offers, in the order the menu lists it: an action
/// that does not apply to all of them is left out.
pub(crate) fn selection_actions(items: &[(NodeKind, Target)]) -> Vec<SelectionAction> {
    let all = |applies: &dyn Fn(NodeKind, Target) -> bool| {
        !items.is_empty() && items.iter().all(|(kind, target)| applies(*kind, *target))
    };
    let offers = |kind, target, action: fn(&NodeAction) -> bool| {
        node_actions(kind, "", target).iter().any(action)
    };
    // The text of a definition is shown for what has one to read: a routine's
    // is its own tab, which a list of texts has no place for.
    let definition = |kind, target| {
        kind != NodeKind::Routine && offers(kind, target, |action| *action == NodeAction::ShowDdl)
    };
    [
        (
            SelectionAction::OpenAll,
            all(&|kind, _| matches!(kind, NodeKind::Relation(_) | NodeKind::Routine)),
        ),
        (
            SelectionAction::CopyNames,
            all(&|kind, target| {
                offers(kind, target, |action| {
                    matches!(action, NodeAction::CopyName(_))
                })
            }),
        ),
        (SelectionAction::CopyDdl, all(&definition)),
        (SelectionAction::ShowDdl, all(&definition)),
        (
            SelectionAction::Pin,
            all(&|kind, _| {
                matches!(
                    kind,
                    NodeKind::Relation(_) | NodeKind::Routine | NodeKind::Sequence
                )
            }),
        ),
        // Whatever a row is, it belongs to a connection or a database that
        // can be read again.
        (SelectionAction::Refresh, !items.is_empty()),
    ]
    .into_iter()
    .filter_map(|(action, applies)| applies.then_some(action))
    .collect()
}

/// What the selection's menu says it holds: `3 tables selected`, or `3 items
/// selected` when the rows are not all one kind.
pub(crate) fn selection_header(kinds: &[NodeKind]) -> String {
    let count = kinds.len();
    match kinds.split_first() {
        Some((first, rest)) if rest.iter().all(|kind| kind == first) => {
            format!("{count} {} selected", first.plural())
        }
        _ => format!("{count} items selected"),
    }
}

/// How many open rows, and how many selected rows, one profile keeps for the
/// next run. `profiles.toml` is a file a person edits: a list is read no
/// further than this.
///
/// ponytail: the first rows in tree order are kept, so what is dropped past it
/// is the deepest and latest opened; a profile with more open than this comes
/// back with its outer folders open and its inner ones folded. The upgrade is
/// a cap that counts folds at each depth.
pub(crate) const OPEN_ROWS_CAP: usize = 500;

/// The ids of the open folders under `profile`'s connection row, the row
/// itself included, in tree order, at most [`OPEN_ROWS_CAP`]. A folder inside a
/// folded one is as open as it was left: an item keeps its rows until the
/// next build.
pub(crate) fn open_rows_of(items: &[TreeItem], profile: &str) -> Vec<String> {
    let id = connection_row(profile);
    // A connection is a root, or a row of the group that is.
    let connection = items
        .iter()
        .find(|item| item.id.as_str() == id)
        .or_else(|| {
            items
                .iter()
                .flat_map(|group| &group.children)
                .find(|item| item.id.as_str() == id)
        });
    let mut rows = Vec::new();
    if let Some(connection) = connection {
        collect_open(connection, &mut rows);
    }
    rows
}

fn collect_open(item: &TreeItem, rows: &mut Vec<String>) {
    if rows.len() >= OPEN_ROWS_CAP {
        return;
    }
    if item.is_folder() && item.is_expanded() {
        rows.push(item.id.to_string());
    }
    for child in &item.children {
        collect_open(child, rows);
    }
}

/// Whether each folder of `items` is open, by row id, for the folds a rebuild
/// is made with: [`crate::explorer::folds`] less the rows the last build
/// closed because nothing had loaded under them (`unloaded`), which nobody
/// chose to fold. A row the user opened since is the user's, and stays.
pub(crate) fn held_folds(items: &[TreeItem], unloaded: &HashSet<String>) -> HashMap<String, bool> {
    let mut folds = crate::explorer::folds(items);
    folds.retain(|row, open| *open || !unloaded.contains(row));
    folds
}

/// The rows to keep for a profile: those `open` in its tree, then the ones
/// already `stored` that the tree has no say over (`held` is what it has a say
/// over). A stored row the tree does not hold yet is waiting for its subtree
/// to load; one it holds is as the tree has it. Rows that name something that
/// no longer exists stay until the cap pushes them out, since a row that is not
/// there cannot be told from one that is not loaded.
pub(crate) fn remembered_rows(
    open: Vec<String>,
    stored: &[String],
    held: &HashMap<String, bool>,
) -> Vec<String> {
    open.into_iter()
        .chain(
            stored
                .iter()
                .filter(|row| !held.contains_key(row.as_str()))
                .cloned(),
        )
        .take(OPEN_ROWS_CAP)
        .collect()
}

/// The id of the profile a connections-tree row is under or is, for a
/// connection's row, a database's and every row scoped to one.
pub(crate) fn profile_of_row(id: &str) -> Option<&str> {
    length_prefixed(id.strip_prefix("connection:")?).map(|(profile, _)| profile)
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
    /// While there is text, how many of the listed connections have not
    /// loaded, so the filter could not look inside them. Nothing loads for a
    /// filter, and a tree that dropped them without a word would read as
    /// having searched them.
    pub(crate) unsearched: usize,
    /// What a row's count is of, shown plus left out by a filter, recorded
    /// only when something was left out and no search text is typed. A row
    /// without one draws its bare count.
    pub(crate) totals: HashMap<String, usize>,
    /// The connection and database rows built folded because nothing had
    /// loaded under them. Their closed state is the build's, not a fold anyone
    /// chose, so a stored open row is not overruled by it.
    pub(crate) unloaded: HashSet<String>,
    /// The loaded rows built folded, each holding one inert row where its
    /// rows would be: opening one asks for a rebuild that builds them.
    pub(crate) pruned: HashSet<String>,
    /// The row each row left out under a pruned one hangs from. A row's
    /// ancestors cannot be read from its id, and a reveal has to open them.
    pub(crate) hidden: HashMap<String, String>,
}

/// What a row's count slot says: `count`, or `count of total` where a filter
/// left some out.
pub(crate) fn count_label(count: usize, total: Option<usize>) -> String {
    match total {
        Some(total) => format!("{count} of {total}"),
        None => count.to_string(),
    }
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
/// it is open, so a match is seen, and the counts are of the rows kept. The
/// listed connections that have not loaded are counted in `unsearched`.
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
    let mut unsearched = vec![0; projects.len() + 1];
    for connection in listed {
        let bucket = projects
            .iter()
            .position(|project| project.connections.iter().any(|id| id == connection.id))
            .map_or(0, |at| at + 1);
        if search.filtering && !matches!(connection.content, Content::Loaded(_)) {
            unsearched[bucket] += 1;
        }
        buckets[bucket].extend(connection_item(&mut tree, connection, folds, &mut search));
    }
    if projects.is_empty() {
        tree.unsearched = unsearched[0];
        tree.items = buckets.swap_remove(0);
        tree.highlights.extend(search.builder.into_highlights());
        return tree;
    }
    if scope == ListingScope::Current {
        let bucket = current
            .and_then(|name| projects.iter().position(|project| project.name == name))
            .map_or(0, |at| at + 1);
        tree.unsearched = unsearched[bucket];
        tree.items = buckets.swap_remove(bucket);
        tree.highlights.extend(search.builder.into_highlights());
        return tree;
    }
    tree.unsearched = unsearched.iter().sum();
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
    // Without rows of its own to match, only its name can keep a connection,
    // or the name of its own database, which is a row whether or not anything
    // is in it.
    let has_rows =
        matches!(&connection.content, Content::Loaded(content) if !content.items.is_empty());
    let own_database = catalog_database(connection.levels, connection.database.as_deref());
    let database_named = matches!(connection.content, Content::Loaded(_))
        && own_database.is_some_and(|database| {
            search
                .builder
                .name_matches(&database_row(connection.id, database), database)
        });
    if search.filtering && !named && !has_rows && !database_named {
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
    // What the filter dropped from the catalog rows that sit on the
    // connection itself: where they are databases, the databases it hides.
    let mut catalog_left_out = 0;
    let others = connection.databases;
    let (open, children) = match connection.content {
        // Nothing has asked it to load, and nothing will until this row is
        // opened: an open row here would show a `Loading…` that no answer is
        // coming to replace.
        Content::Unloaded => {
            tree.unloaded.insert(id.clone());
            (
                false,
                vec![notice(tree, connection.id, None, "loading", LOADING)],
            )
        }
        Content::Loading => (
            followed,
            vec![notice(tree, connection.id, None, "loading", LOADING)],
        ),
        Content::Failed(text) => (
            followed,
            vec![notice(tree, connection.id, None, "notice", &text)],
        ),
        // Another database of the server is rows of its own to list, even
        // where the connection's own has none (a MongoDB profile naming no
        // database).
        // A known database of its own is a row too, drawn empty below.
        Content::Loaded(content)
            if content.items.is_empty() && others.is_empty() && own_database.is_none() =>
        {
            count = counts_databases.then_some(0);
            // Where the schemas are the database's own, they are not what
            // the connection counts.
            if connection.levels != Levels::DatabaseThenSchema {
                catalog_left_out = content.left_out;
            }
            (
                followed,
                vec![notice(
                    tree,
                    connection.id,
                    None,
                    "notice",
                    &empty_catalog_line(connection.levels, content.left_out),
                )],
            )
        }
        Content::Loaded(content) => {
            let ExplorerTree {
                items,
                leaves,
                highlights,
                shortcuts,
                left_out,
            } = content;
            let mut rows = Rows {
                profile: connection.id,
                leaves,
                highlights,
                shortcuts,
                folds,
                filtering: search.filtering,
            };
            // The Pinned group, the folders and the Files group stay on the
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
                        tree.nodes.insert(
                            row.clone(),
                            Node::Database {
                                profile: connection.id.to_string(),
                                database: database.to_string(),
                            },
                        );
                        tree.counts.insert(row.clone(), under.len());
                        if left_out > 0 && !search.filtering {
                            tree.totals.insert(row.clone(), under.len() + left_out);
                        }
                        let open = search.filtering || folds.get(&row).copied().unwrap_or(false);
                        let mut item = TreeItem::new(row.clone(), database.to_string());
                        item = match (under.is_empty(), open) {
                            (true, _) => item.children(vec![empty_row(tree, &row)]),
                            (false, true) => item.children(under),
                            (false, false) => item.children(prune(tree, &row, under)),
                        };
                        children.push(item.expanded(open));
                        count = Some(1);
                    } else {
                        count = Some(0);
                    }
                }
                None => {
                    count = counts_databases.then_some(catalog.len());
                    catalog_left_out = left_out;
                    children.extend(
                        catalog
                            .into_iter()
                            .map(|item| scoped_item(tree, &mut rows, None, item)),
                    );
                }
            }
            let others = others_items(
                tree,
                connection.id,
                connection.levels,
                others,
                folds,
                search,
            );
            // Each database row is one more of what the connection counts.
            count = count.map(|count| count + others.len());
            children.extend(others);
            (search.filtering || followed, children)
        }
    };
    if let Some(count) = count {
        tree.counts.insert(id.clone(), count);
        let databases_left_out = match connection.levels.lists_other_databases() {
            true => connection.databases_left_out,
            false => 0,
        };
        let left_out = catalog_left_out + databases_left_out;
        if left_out > 0 && !search.filtering {
            tree.totals.insert(id.clone(), count + left_out);
        }
    }
    let children = match open || search.filtering {
        true => children,
        false => prune(tree, &id, children),
    };
    Some(
        TreeItem::new(id, connection.name.to_string())
            .expanded(open)
            .children(children),
    )
}

/// The rows of the other databases of a connection's server, each folded
/// until its catalog has loaded over the session opened for it. Where the
/// shape's one schema is its database, a database's row holds that schema's
/// kinds directly. A database the filter keeps by neither its name nor
/// anything loaded under it is left out.
fn others_items(
    tree: &mut SidebarTree,
    profile: &str,
    levels: Levels,
    others: Vec<(String, Content)>,
    folds: &HashMap<String, bool>,
    search: &mut Search,
) -> Vec<TreeItem> {
    let mut items = Vec::new();
    for (database, content) in others {
        let row = database_row(profile, &database);
        let named = search.builder.name_matches(&row, &database);
        let followed = folds.get(&row).copied().unwrap_or(false);
        let (open, children, count) = match content {
            // Nothing will load it until its row is opened, as for a
            // connection: an open row here would wait on no answer.
            Content::Unloaded => {
                tree.unloaded.insert(row.clone());
                (
                    false,
                    vec![notice(tree, profile, Some(&database), "loading", LOADING)],
                    None,
                )
            }
            Content::Loading => (
                followed,
                vec![notice(tree, profile, Some(&database), "loading", LOADING)],
                None,
            ),
            Content::Failed(text) => (
                followed,
                vec![notice(tree, profile, Some(&database), "notice", &text)],
                None,
            ),
            Content::Loaded(content) => {
                let ExplorerTree {
                    items,
                    leaves,
                    highlights,
                    shortcuts,
                    left_out,
                } = content;
                let mut rows = Rows {
                    profile,
                    leaves,
                    highlights,
                    shortcuts,
                    folds,
                    filtering: search.filtering,
                };
                let holds_objects = levels == Levels::DatabaseHoldsObjects;
                let schemas = match holds_objects {
                    true => items
                        .into_iter()
                        .flat_map(|schema| schema.children)
                        .collect(),
                    false => items,
                };
                let under: Vec<TreeItem> = schemas
                    .into_iter()
                    .map(|item| scoped_item(tree, &mut rows, Some(&database), item))
                    .collect();
                let count = match holds_objects {
                    true => under
                        .iter()
                        .filter_map(|child| tree.counts.get(child.id.as_str()))
                        .sum(),
                    false => under.len(),
                };
                if search.filtering && !named && under.is_empty() {
                    continue;
                }
                if left_out > 0 && !search.filtering {
                    tree.totals.insert(row.clone(), count + left_out);
                }
                let children = match under.is_empty() {
                    true => vec![empty_row(tree, &row)],
                    false => under,
                };
                (search.filtering || followed, children, Some(count))
            }
        };
        if search.filtering && !named && count.is_none() {
            continue;
        }
        tree.nodes.insert(
            row.clone(),
            Node::Database {
                profile: profile.to_string(),
                database: database.clone(),
            },
        );
        if let Some(count) = count {
            tree.counts.insert(row.clone(), count);
        }
        let children = match open || search.filtering {
            true => children,
            false => prune(tree, &row, children),
        };
        items.push(
            TreeItem::new(row, database)
                .expanded(open)
                .children(children),
        );
    }
    items
}

/// The one row a folded loaded row holds in place of its rows, so that it is
/// a folder, can be opened, and fires the event that builds them. Its id names
/// `parent` and starts where no other row's does: ended with a suffix, a
/// schema named `x/folded` would have the id of `x`'s.
fn folded_row(tree: &mut SidebarTree, parent: &str) -> TreeItem {
    let id = format!("folded:{parent}");
    tree.nodes.insert(id.clone(), Node::Notice);
    TreeItem::new(id, String::new()).disabled(true)
}

/// What a folded row holds: `children`, or one inert row when there are rows
/// among them to leave out. Counts are taken from the whole content before
/// this, and every row left out stays in `tree.nodes`, so a selection or a
/// stored row can still name it; only the items, which the tree flattens and
/// clones per drawn row, stay small. A search builds everything open and never
/// asks.
///
/// ponytail: the rows are built, then left out; the catalog is still walked
/// whole on every rebuild. The upgrade is to hand the builder the folds and
/// walk only what is open, which the counts would then need from the catalog.
fn prune(tree: &mut SidebarTree, parent: &str, children: Vec<TreeItem>) -> Vec<TreeItem> {
    if children.iter().all(TreeItem::is_disabled) {
        return children;
    }
    hide(tree, parent, &children);
    tree.pruned.insert(parent.to_string());
    vec![folded_row(tree, parent)]
}

fn hide(tree: &mut SidebarTree, parent: &str, children: &[TreeItem]) {
    for child in children {
        tree.hidden.insert(child.id.to_string(), parent.to_string());
        hide(tree, &child.id, &child.children);
    }
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
fn notice(
    tree: &mut SidebarTree,
    profile: &str,
    database: Option<&str>,
    row: &str,
    text: &str,
) -> TreeItem {
    let id = scoped_row(profile, database, row);
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
    let children = match open || rows.filtering {
        true => children,
        false => prune(tree, &id, children),
    };
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

    #[test]
    fn each_density_has_its_row_height_and_indent() {
        let steps = Density::ALL.map(|density| (density.row_height(), density.indent()));
        assert_eq!(steps, [(22., 12.), (26., 14.), (30., 16.)]);
        assert_eq!(Density::default(), Density::Standard);
        for density in Density::ALL {
            assert_eq!(Density::from_slug(Some(density.slug())), density);
        }
        assert_eq!(Density::from_slug(None), Density::Standard);
        assert_eq!(Density::from_slug(Some("roomy")), Density::Standard);
    }

    #[test]
    fn a_size_is_shown_only_while_sizes_are_on() {
        assert_eq!(shown_size(Some(10), true), Some(10));
        assert_eq!(shown_size(Some(10), false), None);
        assert_eq!(shown_size(None, true), None);
    }

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
            databases: Vec::new(),
            databases_left_out: 0,
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
                ..Default::default()
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

    /// Folds that open `rows`: a folded loaded row is built as one inert row,
    /// so a test that looks inside one opens it first.
    fn opened(rows: impl IntoIterator<Item = String>) -> HashMap<String, bool> {
        rows.into_iter().map(|row| (row, true)).collect()
    }

    /// The rows `birds()` is under, under `profile`, down to its category.
    fn above_birds(profile: &str) -> Vec<String> {
        vec![
            connection_row(profile),
            scoped_row(profile, None, "schema-main"),
            scoped_row(profile, None, "category-Tables-main"),
        ]
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
            &opened(["x", "y"].into_iter().flat_map(above_birds)),
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
            databases: Vec::new(),
            databases_left_out: 0,
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
            ..Default::default()
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
        content.extend(explorer::files(
            &[],
            &queries,
            filter,
            explorer::FilesOrder::Name,
        ));
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
            &opened([
                connection_row("a"),
                database_row("a", "d"),
                scoped_row("a", Some("d"), "schema-main"),
                scoped_row("a", Some("d"), "category-Tables-main"),
            ]),
        );

        let connection = find(&tree.items, &connection_row("a"));
        assert_eq!(
            row_ids(&connection.children),
            [
                scoped_row("a", None, explorer::PINNED_GROUP),
                scoped_row("a", None, explorer::FILES_GROUP),
                database_row("a", "d"),
            ]
        );
        assert!(matches!(
            tree.nodes.get(&database_row("a", "d")),
            Some(Node::Database { .. })
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
            &opened([connection_row("a")]),
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
                .any(|node| matches!(node, Node::Database { .. }))
        );
    }

    #[test]
    fn a_known_database_with_nothing_loaded_in_it_is_a_row_and_its_name_keeps_it() {
        let empty = |filter: &str| {
            filtered(
                &[],
                vec![Listed {
                    levels: Levels::DatabaseThenSchema,
                    database: Some("d".into()),
                    ..listed("a", "a", Content::Loaded(ExplorerTree::default()))
                }],
                filter,
            )
        };
        let database = database_row("a", "d");

        let tree = super::build(
            &[],
            vec![Listed {
                levels: Levels::DatabaseThenSchema,
                database: Some("d".into()),
                ..listed("a", "a", Content::Loaded(ExplorerTree::default()))
            }],
            &HashMap::from([(connection_row("a"), true)]),
            ListingScope::All,
            None,
            "",
        );
        let row = find(&tree.items, &database);
        assert_eq!(row.children.len(), 1);
        assert_eq!(row.children[0].label.as_str(), NO_OBJECTS);
        assert_eq!(count_of(&tree, &database), Some(0));
        assert_eq!(count_of(&tree, &connection_row("a")), Some(1));

        // The database's name keeps it, as an object's would.
        assert!(search(&empty("d").items, &database).is_some());
        assert!(empty("zzz").items.is_empty());
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
                ..Default::default()
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
            &opened([connection_row("a")]),
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
                .any(|node| matches!(node, Node::Database { .. }))
        );
        assert!(schema_row_is_a_database(Levels::DatabaseHoldsObjects));
        assert!(schema_row_is_a_database(Levels::DatabaseIsSchema));
        assert!(!schema_row_is_a_database(Levels::DatabaseThenSchema));
        assert!(!schema_row_is_a_database(Levels::Flat));
    }

    /// A connection of `levels` on `database`, holding `birds`, with `others`
    /// beside it.
    fn with_others<'a>(
        levels: Levels,
        database: &str,
        others: Vec<(String, Content)>,
    ) -> Listed<'a> {
        Listed {
            databases: others,
            ..database_listed(
                "a",
                content_of(vec![schema(
                    match levels {
                        Levels::DatabaseHoldsObjects => database,
                        _ => "main",
                    },
                    vec![relation("birds", RelationKind::Table, None)],
                )]),
                levels,
                database,
            )
        }
    }

    #[test]
    fn a_database_the_filter_left_out_is_never_a_row_where_databases_hold_objects() {
        use crate::db::{Databases, Include};
        let others_of = |own: &str, all: &[&str], list: &[&str]| {
            let include = Include::from_stored(
                list.iter().map(|entry| entry.to_string()).collect(),
                Vec::new(),
            );
            let narrowed = include
                .resolve(Levels::DatabaseHoldsObjects, Some(own), None)
                .narrow_databases(Databases {
                    names: all.iter().map(|name| name.to_string()).collect(),
                    current: Some(own.into()),
                    left_out: 0,
                });
            let others = other_databases(&narrowed.names, narrowed.current.as_deref())
                .into_iter()
                .map(|name| (name.to_string(), Content::Unloaded))
                .collect();
            let tree = build(
                &[],
                vec![with_others(Levels::DatabaseHoldsObjects, own, others)],
                &opened([connection_row("a")]),
            );
            row_ids(&find(&tree.items, &connection_row("a")).children)
        };

        assert_eq!(
            others_of("db0", &["db0", "db1", "db2"], &["db1"]),
            [
                scoped_row("a", None, "schema-db0"),
                database_row("a", "db1")
            ]
        );
        assert_eq!(
            others_of(
                "dbdelve_dev",
                &["dbdelve_archive", "dbdelve_dev"],
                &["nothing*"]
            ),
            [scoped_row("a", None, "schema-dbdelve_dev")]
        );
    }

    #[test]
    fn other_databases_are_listed_folded_until_they_load() {
        let other = database_row("a", "other");
        // The fold is the user's, and says open: an unloaded database is
        // folded whatever it says, since nothing here has asked it to load.
        let open = opened([other.clone(), connection_row("a")]);
        let build_with = |content| {
            build(
                &[],
                vec![with_others(
                    Levels::DatabaseThenSchema,
                    "d",
                    vec![("other".into(), content)],
                )],
                &open,
            )
        };

        let tree = build_with(Content::Unloaded);
        let connection = find(&tree.items, &connection_row("a"));
        assert_eq!(
            row_ids(&connection.children),
            [database_row("a", "d"), other.clone()]
        );
        let row = find(&tree.items, &other);
        assert!(!row.is_expanded());
        assert_eq!(
            row_ids(&row.children),
            [scoped_row("a", Some("other"), "loading")]
        );
        assert_eq!(row.children[0].label.as_str(), LOADING);
        assert!(row.children[0].is_disabled());
        assert!(matches!(
            tree.nodes.get(&other),
            Some(Node::Database { profile, database }) if (profile.as_str(), database.as_str()) == ("a", "other")
        ));
        // Nothing is counted of what has not loaded.
        assert_eq!(count_of(&tree, &other), None);

        let tree = build_with(Content::Failed("It would not open.".into()));
        let row = find(&tree.items, &other);
        assert!(row.is_expanded(), "a failure follows its fold");
        assert_eq!(
            row_ids(&row.children),
            [scoped_row("a", Some("other"), "notice")]
        );
        assert_eq!(row.children[0].label.as_str(), "It would not open.");
        assert!(row.children[0].is_disabled());

        let tree = build_with(Content::Loading);
        let row = find(&tree.items, &other);
        assert!(row.is_expanded(), "the row just clicked stays open");
        assert_eq!(row.children[0].label.as_str(), LOADING);

        let tree = build_with(Content::Loaded(birds()));
        let row = find(&tree.items, &other);
        assert_eq!(
            row_ids(&row.children),
            [scoped_row("a", Some("other"), "schema-main")]
        );
        assert_eq!(count_of(&tree, &other), Some(1));
        // The own database is untouched by any of it.
        assert!(search(&tree.items, &database_row("a", "d")).is_some());
    }

    #[test]
    fn the_own_database_is_listed_once_whatever_engine() {
        for (levels, names, current) in [
            (Levels::DatabaseHoldsObjects, ["db0", "db1"], "db0"),
            (
                Levels::DatabaseThenSchema,
                ["dbdelve_dev", "postgres"],
                "dbdelve_dev",
            ),
        ] {
            let names = names.map(str::to_string);
            let others = other_databases(&names, Some(current));
            assert_eq!(others, [names[1].as_str()], "{levels:?}");

            let tree = build(
                &[],
                vec![with_others(
                    levels,
                    current,
                    others
                        .into_iter()
                        .map(|name| (name.to_string(), Content::Unloaded))
                        .collect(),
                )],
                &opened([connection_row("a")]),
            );
            let databases = tree
                .nodes
                .iter()
                .filter_map(|(id, node)| match node {
                    Node::Database { database, .. } => Some((id.clone(), database.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            // The shape that draws its schema as its database has no
            // database row of its own to count.
            let own = match levels {
                Levels::DatabaseHoldsObjects => {
                    assert!(
                        search(
                            &tree.items,
                            &scoped_row("a", None, &format!("schema-{current}"))
                        )
                        .is_some()
                    );
                    0
                }
                _ => 1,
            };
            assert_eq!(databases.len(), own + 1, "{levels:?}: {databases:?}");
            assert!(databases.contains(&(database_row("a", &names[1]), names[1].clone())));
            assert_eq!(
                count_of(&tree, &connection_row("a")),
                Some(2),
                "{levels:?}: the own database and the other"
            );
        }
    }

    #[test]
    fn another_database_of_a_database_holds_objects_connection_holds_its_kinds_directly() {
        let shop = content_of(vec![schema(
            "shop",
            vec![relation("orders", RelationKind::Table, None)],
        )]);
        let tree = build(
            &[],
            vec![Listed {
                content: Content::Loaded(ExplorerTree::default()),
                databases: vec![("shop".into(), Content::Loaded(shop))],
                ..database_listed(
                    "a",
                    ExplorerTree::default(),
                    Levels::DatabaseHoldsObjects,
                    "other",
                )
            }],
            &opened([
                connection_row("a"),
                database_row("a", "shop"),
                scoped_row("a", Some("shop"), "category-Tables-shop"),
            ]),
        );

        let row = find(&tree.items, &database_row("a", "shop"));
        assert_eq!(
            row_ids(&row.children),
            [scoped_row("a", Some("shop"), "category-Tables-shop")]
        );
        assert!(
            !all_ids(&tree.items)
                .iter()
                .any(|id| id.ends_with("schema-shop")),
            "no schema row is repeated under the database"
        );
        let orders = scoped_row("a", Some("shop"), &explorer::relation_row("shop", "orders"));
        assert!(search(&row.children, &orders).is_some());
        assert!(matches!(
            tree.nodes.get(&orders),
            Some(Node::Row { leaf: Some(_) })
        ));
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
                ..Default::default()
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
        let tree = build(
            &[],
            vec![listed(
                "a",
                "a",
                Content::Loaded(content_of(vec![schema("empty", Vec::new())])),
            )],
            &opened([connection_row("a")]),
        );

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
            ..Default::default()
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
        content.extend(explorer::files(
            &[],
            &queries,
            "",
            explorer::FilesOrder::Name,
        ));
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
            count_of(&tree, &scoped_row("a", None, explorer::FILES_GROUP)),
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
                ..Default::default()
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
    fn the_filter_counts_the_listed_connections_it_could_not_look_inside() {
        let projects = [project("Zoo", &["z"])];
        let with = |scope: ListingScope, current: Option<&str>, filter: &str| {
            super::build(
                &projects,
                vec![
                    listed("a", "Birds", Content::Loaded(tables(&["birds"], filter))),
                    unloaded_named("u", "Flocks"),
                    listed("f", "Pond", Content::Failed("refused".into())),
                    unloaded_named("z", "Keepers"),
                ],
                &HashMap::new(),
                scope,
                current,
                filter,
            )
            .unsearched
        };

        // Idle and failed alike: neither has rows to look in, kept by its
        // name or not.
        assert_eq!(with(ListingScope::All, None, "bir"), 3);
        assert_eq!(with(ListingScope::All, None, "flo"), 3);
        // Only what is listed: the current project alone, or no project.
        assert_eq!(with(ListingScope::Current, Some("Zoo"), "bir"), 1);
        assert_eq!(with(ListingScope::Current, None, "bir"), 2);
        // No text, no search.
        assert_eq!(with(ListingScope::All, None, ""), 0);
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

    /// `tree` as built from a catalog the filter dropped `left_out` of.
    fn left_out(tree: ExplorerTree, left_out: usize) -> ExplorerTree {
        ExplorerTree { left_out, ..tree }
    }

    fn schemas(names: &[&str]) -> Vec<Schema> {
        names.iter().map(|name| schema(name, Vec::new())).collect()
    }

    #[test]
    fn the_filtered_empty_line_names_how_many_the_filter_hides() {
        assert_eq!(
            empty_catalog_line(Levels::DatabaseIsSchema, 0),
            "No database objects found."
        );
        assert_eq!(
            empty_catalog_line(Levels::DatabaseIsSchema, 37),
            "No database objects shown: the filter hides 37 databases."
        );
        assert_eq!(
            empty_catalog_line(Levels::DatabaseHoldsObjects, 1),
            "No database objects shown: the filter hides 1 database."
        );
        assert_eq!(
            empty_catalog_line(Levels::DatabaseThenSchema, 2),
            "No database objects shown: the filter hides 2 schemas."
        );
        assert!(empty_catalog_line(Levels::DatabaseThenSchema, 1).ends_with("1 schema."));
    }

    #[test]
    fn a_connection_the_filter_emptied_says_so_in_its_notice_row() {
        let emptied = |hidden: usize| {
            let listed = Listed {
                levels: Levels::DatabaseIsSchema,
                ..listed(
                    "a",
                    "a",
                    Content::Loaded(left_out(ExplorerTree::default(), hidden)),
                )
            };
            let tree = build(&[], vec![listed], &opened([connection_row("a")]));
            let row = find(&tree.items, &connection_row("a"));
            assert_eq!(row.children.len(), 1);
            assert!(row.children[0].is_disabled());
            (
                row.children[0].label.to_string(),
                count_of(&tree, &connection_row("a")),
                tree.totals.get(&connection_row("a")).copied(),
            )
        };
        assert_eq!(
            emptied(37),
            (
                empty_catalog_line(Levels::DatabaseIsSchema, 37),
                Some(0),
                Some(37)
            )
        );
        assert_eq!(
            emptied(0),
            ("No database objects found.".to_string(), Some(0), None)
        );
    }

    #[test]
    fn a_connection_count_carries_its_total_only_when_the_filter_left_a_database_out() {
        let total = |levels: Levels, hidden: usize| {
            let listed = Listed {
                databases_left_out: hidden,
                ..with_others(levels, "d", vec![("other".into(), Content::Unloaded)])
            };
            let tree = build(&[], vec![listed], &opened([connection_row("a")]));
            (
                count_of(&tree, &connection_row("a")),
                tree.totals.get(&connection_row("a")).copied(),
            )
        };
        assert_eq!(total(Levels::DatabaseThenSchema, 3), (Some(2), Some(5)));
        assert_eq!(total(Levels::DatabaseThenSchema, 0), (Some(2), None));
        assert_eq!(total(Levels::Flat, 3), (None, None));
    }

    #[test]
    fn a_connection_count_drops_its_total_while_the_search_box_has_text() {
        let listed = Listed {
            databases_left_out: 3,
            ..with_others(
                Levels::DatabaseThenSchema,
                "d",
                vec![("other".into(), Content::Unloaded)],
            )
        };
        let tree = super::build(
            &[],
            vec![listed],
            &opened([connection_row("a")]),
            ListingScope::All,
            None,
            "a",
        );
        assert!(search(&tree.items, &connection_row("a")).is_some());
        assert!(tree.totals.is_empty());
    }

    #[test]
    fn a_count_reads_shown_of_total_from_counts_alone() {
        assert_eq!(count_label(4, Some(38)), "4 of 38");
        assert_eq!(count_label(0, Some(38)), "0 of 38");
        assert_eq!(count_label(38, None), "38");

        let counted = |names: [String; 2]| {
            let listed = Listed {
                databases_left_out: 3,
                ..with_others(
                    Levels::DatabaseThenSchema,
                    "d",
                    names
                        .into_iter()
                        .map(|name| (name, Content::Unloaded))
                        .collect(),
                )
            };
            let tree = build(&[], vec![listed], &opened([connection_row("a")]));
            (
                count_of(&tree, &connection_row("a")),
                tree.totals.get(&connection_row("a")).copied(),
            )
        };
        assert_eq!(
            counted(["x".repeat(200), "ü日本語".repeat(40)]),
            counted(["p".into(), "q".into()])
        );
    }

    #[test]
    fn a_database_row_says_how_many_of_its_schemas_it_shows() {
        let listed = || Listed {
            databases: vec![(
                "other".into(),
                Content::Loaded(left_out(content_of(schemas(&["main"])), 1)),
            )],
            ..database_listed(
                "a",
                left_out(content_of(schemas(&["main", "extra"])), 3),
                Levels::DatabaseThenSchema,
                "d",
            )
        };
        let (own, other) = (database_row("a", "d"), database_row("a", "other"));
        let tree = build(&[], vec![listed()], &opened([connection_row("a")]));
        assert_eq!(tree.totals.get(&own), Some(&5));
        assert_eq!(tree.totals.get(&other), Some(&2));

        let tree = super::build(
            &[],
            vec![listed()],
            &opened([connection_row("a")]),
            ListingScope::All,
            None,
            "a",
        );
        assert!(tree.totals.is_empty());
    }

    #[test]
    fn where_schemas_are_databases_the_connection_says_how_many_it_shows() {
        let listed = Listed {
            levels: Levels::DatabaseIsSchema,
            ..listed(
                "a",
                "a",
                Content::Loaded(left_out(content_of(schemas(&["a1", "a2", "a3", "a4"])), 34)),
            )
        };
        let tree = build(&[], vec![listed], &opened([connection_row("a")]));
        assert_eq!(count_of(&tree, &connection_row("a")), Some(4));
        assert_eq!(tree.totals.get(&connection_row("a")), Some(&38));
    }

    #[test]
    fn the_totals_add_up_to_the_rows_drawn_and_the_rows_left_out() {
        let loaded = |names: &[&str], hidden: usize| {
            Content::Loaded(left_out(content_of(schemas(names)), hidden))
        };
        // Each case: the connection as listed, and every row that may carry
        // a total with what the filter left out of it.
        let cases = [
            (
                Levels::DatabaseIsSchema,
                Listed {
                    levels: Levels::DatabaseIsSchema,
                    ..listed("a", "a", loaded(&["main", "other"], 5))
                },
                vec![(connection_row("a"), 5)],
            ),
            (
                Levels::DatabaseThenSchema,
                Listed {
                    databases: vec![("other".into(), loaded(&["main"], 1))],
                    databases_left_out: 4,
                    ..database_listed(
                        "a",
                        left_out(content_of(schemas(&["main", "extra"])), 3),
                        Levels::DatabaseThenSchema,
                        "d",
                    )
                },
                vec![
                    (connection_row("a"), 4),
                    (database_row("a", "d"), 3),
                    (database_row("a", "other"), 1),
                ],
            ),
            (
                Levels::DatabaseHoldsObjects,
                Listed {
                    levels: Levels::DatabaseHoldsObjects,
                    databases: vec![("other".into(), loaded(&["other"], 0))],
                    databases_left_out: 4,
                    ..listed("a", "a", loaded(&["d"], 0))
                },
                vec![(connection_row("a"), 4)],
            ),
        ];
        for (levels, listed, fed) in cases {
            let tree = build(&[], vec![listed], &opened([connection_row("a")]));
            let mut totals: Vec<_> = tree.totals.keys().cloned().collect();
            let mut expected: Vec<_> = fed.iter().map(|(row, _)| row.clone()).collect();
            totals.sort();
            expected.sort();
            assert_eq!(totals, expected, "{levels:?}");
            for (row, left_out) in fed {
                let count = count_of(&tree, &row).expect("a counted row");
                assert_eq!(tree.totals[&row], count + left_out, "{levels:?}: {row}");
            }
        }
        // A kept row is one of the shown, never also one left out: the own
        // database's two schemas are its count whatever the filter hid.
        let tree = build(
            &[],
            vec![database_listed(
                "a",
                left_out(content_of(schemas(&["main", "extra"])), 3),
                Levels::DatabaseThenSchema,
                "d",
            )],
            &opened([connection_row("a")]),
        );
        assert_eq!(count_of(&tree, &database_row("a", "d")), Some(2));
    }

    #[test]
    fn rows_under_a_connection_start_folded_and_keep_the_folds_they_are_given() {
        let schema = scoped_row("a", None, "schema-main");
        let category = scoped_row("a", None, "category-Tables-main");
        let build_with = |rows: &[&String]| {
            let folds = opened(rows.iter().map(|row| row.to_string()));
            build(
                &[],
                vec![listed("a", "a", Content::Loaded(birds()))],
                &folds,
            )
        };
        let connection = connection_row("a");

        let folded = build_with(&[&connection]);
        assert!(!find(&folded.items, &schema).is_expanded());
        assert!(
            search(&folded.items, &category).is_none(),
            "a folded schema is built without its rows"
        );

        let schema_open = build_with(&[&connection, &schema]);
        assert!(find(&schema_open.items, &schema).is_expanded());
        assert!(!find(&schema_open.items, &category).is_expanded());
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

    #[test]
    fn each_node_offers_the_actions_its_target_allows() {
        use crate::db::Engine;
        let on = |kind, engine| node_actions(kind, "birds", Target::built_in(engine));
        let table = NodeKind::Relation(RelationKind::Table);
        let copy = || NodeAction::CopyName("birds".into());
        assert_eq!(on(table, Engine::Postgres), [copy(), NodeAction::ShowDdl]);
        assert_eq!(
            on(table, Engine::Redis),
            [copy()],
            "a target with no definition to write offers none to show"
        );
        assert_eq!(
            on(NodeKind::Sequence, Engine::Postgres),
            [copy(), NodeAction::ShowDdl]
        );
        assert_eq!(
            on(NodeKind::Routine, Engine::Postgres),
            [copy(), NodeAction::ShowDdl]
        );
        for kind in [NodeKind::Connection, NodeKind::Database, NodeKind::Schema] {
            assert_eq!(
                on(kind, Engine::Postgres),
                [NodeAction::NewQueryHere, copy(), NodeAction::Refresh],
                "{kind:?} has no definition of its own"
            );
        }
    }

    #[test]
    fn a_sequence_offers_copy_name_and_show_ddl() {
        use crate::db::Engine;
        assert_eq!(
            node_actions(NodeKind::Sequence, "s", Target::built_in(Engine::DuckDb)),
            [NodeAction::CopyName("s".into()), NodeAction::ShowDdl]
        );
        assert_eq!(
            node_actions(NodeKind::Sequence, "s", Target::built_in(Engine::Redis)),
            [NodeAction::CopyName("s".into())],
            "a target with no definition to write offers none to show"
        );
    }

    #[test]
    fn a_plain_click_selects_one_a_modifier_click_toggles_and_shift_extends() {
        let ids = |rows: &[&str]| rows.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let mut selection = Selection::default();
        selection.click("x");
        assert_eq!(selection.rows, ids(&["x"]));
        selection.toggle("y");
        assert_eq!(selection.rows, ids(&["x", "y"]), "a toggle adds a row");
        selection.toggle("x");
        assert_eq!(selection.rows, ids(&["y"]), "and takes one away again");
        selection.click("z");
        assert_eq!(
            selection.rows,
            ids(&["z"]),
            "a plain click is the whole selection"
        );

        let visible = ids(&["r1", "r2", "r3", "r4"]);
        selection.click("r1");
        selection.extend_to("r3", &visible);
        assert_eq!(selection.rows, ids(&["r1", "r2", "r3"]));
        selection.extend_to("r2", &visible);
        assert_eq!(
            selection.rows,
            ids(&["r1", "r2"]),
            "a range is always from the anchor, which a Shift-click does not move"
        );
        let mut backwards = Selection::default();
        backwards.click("r4");
        backwards.extend_to("r2", &visible);
        assert_eq!(backwards.rows, ids(&["r2", "r3", "r4"]));

        let mut lost = Selection::default();
        lost.extend_to("r2", &visible);
        assert_eq!(lost.rows, ids(&["r2"]), "no anchor: a plain click");
        lost.click("gone");
        lost.extend_to("r3", &visible);
        assert_eq!(
            lost.rows,
            ids(&["r3"]),
            "an anchor that is not showing: a plain click"
        );

        let mut kept = Selection::default();
        kept.click("r1");
        kept.toggle("r2");
        kept.toggle("r3");
        let present: HashSet<String> = ids(&["r2", "r3", "r4"]).into_iter().collect();
        kept.retain(&present);
        assert_eq!(
            kept.rows,
            ids(&["r2", "r3"]),
            "a row that is gone drops out"
        );
    }

    #[test]
    fn a_mixed_selection_offers_only_what_applies_to_every_row() {
        use crate::db::Engine;
        use SelectionAction::*;
        let pg = Target::built_in(Engine::Postgres);
        let table = (NodeKind::Relation(RelationKind::Table), pg);
        let view = NodeKind::Relation(RelationKind::View);
        assert_eq!(
            selection_actions(&[table, table]),
            [OpenAll, CopyNames, CopyDdl, ShowDdl, Pin, Refresh]
        );
        assert_eq!(
            selection_actions(&[table, (NodeKind::Schema, pg)]),
            [CopyNames, Refresh],
            "a schema opens nothing, has no definition and is no object to pin"
        );
        assert_eq!(
            selection_actions(&[
                table,
                (
                    NodeKind::Relation(RelationKind::Table),
                    Target::built_in(Engine::Redis)
                )
            ]),
            [OpenAll, CopyNames, Pin, Refresh],
            "a target with no definition to write drops both DDL actions"
        );
        assert_eq!(
            selection_actions(&[table, (NodeKind::Routine, pg)]),
            [OpenAll, CopyNames, Pin, Refresh],
            "a routine's definition is its own tab, not a text to list"
        );
        assert_eq!(
            selection_actions(&[table, (NodeKind::Sequence, pg)]),
            [CopyNames, CopyDdl, ShowDdl, Pin, Refresh],
            "a sequence opens no tab"
        );
        assert_eq!(
            selection_actions(&[(NodeKind::Connection, pg), (NodeKind::Database, pg)]),
            [CopyNames, Refresh]
        );
        assert_eq!(selection_header(&[table.0, table.0]), "2 tables selected");
        assert_eq!(selection_header(&[table.0, view]), "2 items selected");
        assert_eq!(selection_header(&[view, view, view]), "3 views selected");
        assert_eq!(
            selection_header(&[NodeKind::Schema, NodeKind::Schema]),
            "2 schemas selected"
        );
    }

    #[test]
    fn a_stored_open_row_applies_only_once_its_row_exists() {
        let schema = scoped_row("a", None, "schema-main");
        let stored = opened([
            connection_row("a"),
            schema.clone(),
            scoped_row("a", Some("gone"), "schema-main"),
        ]);
        let before = build(&[], vec![unloaded("a")], &stored);
        assert_eq!(
            all_ids(&before.items),
            [connection_row("a"), scoped_row("a", None, "loading")],
            "an idle connection stays folded and gains no row from what was stored"
        );
        assert!(!find(&before.items, &connection_row("a")).is_expanded());

        let loaded = build(
            &[],
            vec![listed("a", "a", Content::Loaded(birds()))],
            &stored,
        );
        assert!(
            find(&loaded.items, &schema).is_expanded(),
            "once its subtree has loaded the row opens as it was left"
        );
        assert_eq!(
            all_ids(&loaded.items),
            all_ids(
                &build(
                    &[],
                    vec![listed("a", "a", Content::Loaded(birds()))],
                    &opened([connection_row("a"), schema])
                )
                .items
            ),
            "a stored id naming a database that is not listed changes nothing"
        );
    }

    #[test]
    fn open_rows_of_lists_a_profiles_open_folders_capped() {
        let (schema, category) = (
            scoped_row("a", None, "schema-main"),
            scoped_row("a", None, "category-Tables-main"),
        );
        let folds = HashMap::from([
            (connection_row("a"), true),
            (schema.clone(), true),
            (category.clone(), true),
            (connection_row("b"), true),
            (scoped_row("b", None, "schema-main"), true),
        ]);
        let tree = build(
            &[],
            vec![
                listed("a", "a", Content::Loaded(birds())),
                listed("b", "b", Content::Loaded(birds())),
            ],
            &folds,
        );
        assert_eq!(
            open_rows_of(&tree.items, "a"),
            [connection_row("a"), schema, category],
            "open folders under that connection, its own row first, in tree order"
        );
        assert_eq!(
            open_rows_of(&tree.items, "b"),
            [connection_row("b"), scoped_row("b", None, "schema-main")],
            "and none of another's"
        );
        assert!(
            open_rows_of(&tree.items, "missing").is_empty(),
            "a profile the tree does not list has no rows"
        );

        let many: Vec<TreeItem> = (0..OPEN_ROWS_CAP + 20)
            .map(|at| {
                TreeItem::new(format!("folder-{at}"), "f")
                    .expanded(true)
                    .child(TreeItem::new(format!("leaf-{at}"), "l"))
            })
            .collect();
        let items = vec![
            TreeItem::new(connection_row("c"), "c")
                .expanded(true)
                .children(many),
        ];
        let rows = open_rows_of(&items, "c");
        assert_eq!(rows.len(), OPEN_ROWS_CAP);
        assert_eq!(rows[0], connection_row("c"));
        assert_eq!(rows[1], "folder-0", "what is dropped is the latest");
    }

    #[test]
    fn a_folded_loaded_row_carries_one_inert_row_and_no_descendants() {
        let listed = || {
            vec![database_listed(
                "a",
                content_with_shortcuts("d", ""),
                Levels::DatabaseThenSchema,
                "d",
            )]
        };
        let (database, schema) = (
            database_row("a", "d"),
            scoped_row("a", Some("d"), "schema-main"),
        );
        let mut folds = HashMap::from([(connection_row("a"), true)]);
        let folded = build(&[], listed(), &folds);

        let row = find(&folded.items, &database);
        assert!(!row.is_expanded());
        let inert = format!("folded:{database}");
        assert_eq!(row_ids(&row.children), [inert.as_str()]);
        assert!(row.children[0].is_disabled());
        assert!(matches!(folded.nodes.get(&inert), Some(Node::Notice)));
        assert!(
            search(&folded.items, &schema).is_none(),
            "what is under a folded row is not built as rows"
        );
        assert!(
            folded.nodes.contains_key(&schema),
            "though it is still a row a selection or a reveal can name"
        );
        assert_eq!(
            count_of(&folded, &database),
            Some(1),
            "and it is counted from the whole content, not from what is drawn"
        );

        folds.insert(database.clone(), true);
        let opened = build(&[], listed(), &folds);
        assert_eq!(
            row_ids(&find(&opened.items, &database).children),
            [schema.as_str()],
            "opened, it is built with its rows, folded in their turn"
        );
        assert!(!find(&opened.items, &schema).is_expanded());
        assert_eq!(
            row_ids(&find(&opened.items, &schema).children),
            [format!("folded:{schema}")]
        );
    }

    #[test]
    fn ten_folded_connections_of_five_thousand_relations_build_one_row_each() {
        let names: Vec<String> = (0..5000).map(|at| format!("t{at}")).collect();
        let listed: Vec<Listed<'_>> = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9"]
            .into_iter()
            .map(|id| {
                let relations = names
                    .iter()
                    .map(|name| relation(name, RelationKind::Table, None))
                    .collect();
                listed(
                    id,
                    id,
                    Content::Loaded(content_of(vec![schema("main", relations)])),
                )
            })
            .collect();
        let tree = built(listed);

        assert_eq!(tree.items.len(), 10);
        for item in &tree.items {
            assert_eq!(item.children.len(), 1, "{} holds one inert row", item.id);
            assert!(item.children[0].is_disabled());
        }
        assert!(
            all_ids(&tree.items).len() < 100,
            "ten folded connections are twenty rows, not fifty thousand"
        );
        assert_eq!(
            count_of(&tree, &connection_row("c0")),
            None,
            "a flat connection counts nothing, as it did"
        );
    }

    #[test]
    fn a_typed_filter_builds_every_matching_row_open_whatever_was_folded() {
        let listed = vec![listed(
            "a",
            "a",
            Content::Loaded(tables(&["birds", "flocks"], "bir")),
        )];
        let tree = filtered(&[], listed, "bir");
        let (schema, category) = (
            scoped_row("a", None, "schema-main"),
            scoped_row("a", None, "category-Tables-main"),
        );
        let birds = scoped_row("a", None, &explorer::relation_row("main", "birds"));
        for row in [connection_row("a"), schema, category] {
            assert!(find(&tree.items, &row).is_expanded(), "{row} is open");
        }
        assert!(
            search(&tree.items, &birds).is_some(),
            "the match is built, not folded away"
        );
        assert!(
            all_ids(&tree.items)
                .iter()
                .all(|id| !id.starts_with("folded:")),
            "nothing is pruned while text is typed"
        );
    }
}

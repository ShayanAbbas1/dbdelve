//! The connections sidebar of complex mode: every connection under its
//! project, one tree for the window rather than one per connection.
//!
//! A connection's own rows are the rows simple mode draws for it
//! (`profiles::explorer_content`), under ids that name the connection, so the
//! same table of two connections is two rows. Acting on a row of a connection
//! that is not in front makes it the one in front first: the gate, the mode and
//! the engine a statement meets are always those of the connection it was
//! opened on.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

use gpui::{Entity, SharedString, StyledText};
use gpui_component::{
    ActiveTheme as _,
    tree::{TreeEvent, TreeItem, TreeState},
};

use super::forms::{MenuRow, explorer_menu, node_items, selection_menu};
use super::profiles::{database_content, explorer_content};
use super::*;
use crate::db::{Levels, Syntax};
use crate::explorer::{
    ExplorerLeaf, Member, PINNED_GROUP, QUERIES_GROUP, Shortcut, is_schema_row, saved_query,
    schema_row_name,
};
use crate::icons::{queries_group_icon, saved_query_icon};
use crate::session::Binding;
use crate::sidebar::{
    self as model, Content, Listed, Node, NodeKind, OPEN_ROWS_CAP, SelectedNode, Selection,
    SelectionAction, node_actions, schema_row_is_a_database, shown_size,
};
use crate::ui::{Status, row_icon_tinted, status_dot};

/// The id of the row of `profile`'s front tab in the connections tree, if
/// the tab has one.
pub(crate) fn front_row_id(profile: &Profile) -> Option<String> {
    if let Some((database, row)) = profile.front_row_elsewhere() {
        return Some(model::listed_row(
            &profile.id,
            profile.config.target().levels(),
            Some(database),
            &row,
        ));
    }
    let row = profile.front_row()?;
    Some(model::listed_row(
        &profile.id,
        profile.config.target().levels(),
        profile.active_database().as_deref(),
        &row,
    ))
}

/// What a connection's row draws from its profile, copied once per frame for
/// the tree's row closure.
struct RowLook {
    color: Option<ConnectionColor>,
    status: Status,
    host: String,
}

/// The name a test reads a drawn status by.
fn status_name(status: Status) -> &'static str {
    match status {
        Status::Idle => "idle",
        Status::Connecting => "connecting",
        Status::Connected => "connected",
        Status::Failed => "failed",
    }
}

/// The database and schema a New Query Here on `node` binds, by the shape of
/// its connection ([`Binding::for_schema_row`]): a connection row names no
/// database, so its tab is an ordinary one; a database row binds that
/// database; a schema row binds by its connection's levels, and where that is
/// no binding (a connection that switches no database, or a schema whose
/// database is not known) its tab is an ordinary one too.
pub(crate) fn new_query_binding(
    node: &SelectedNode,
    target: Target,
) -> (Option<String>, Option<String>) {
    match node.kind {
        NodeKind::Database => (Some(node.name.clone()), None),
        NodeKind::Schema => {
            match Binding::for_schema_row(target, node.database.as_deref(), &node.name) {
                Some(binding) => (Some(binding.database), binding.schema),
                None => (None, None),
            }
        }
        _ => (None, None),
    }
}

pub(crate) struct ConnectionsSidebar {
    pub(crate) tree: Entity<TreeState>,
    /// What each row stands for, by row id.
    pub(crate) nodes: Arc<HashMap<String, Node>>,
    pub(crate) highlights: Arc<HashMap<String, Vec<Range<usize>>>>,
    pub(crate) shortcuts: Arc<HashMap<String, Shortcut>>,
    /// How many rows each folder row holds, by row id.
    pub(crate) counts: Arc<HashMap<String, usize>>,
    /// How many listed connections the filter could not look inside.
    pub(crate) unsearched: usize,
    /// What the tree is narrowed by.
    pub(crate) filter: Entity<InputState>,
    /// The folds the tree had when the first key was typed, which come back
    /// when the field is cleared: while text is in it every kept row is open.
    pub(crate) folds_before_filter: Option<HashMap<String, bool>>,
    /// The rows a modifier click gathered; complex mode only.
    pub(crate) selection: Selection,
    /// The rows the last build closed because nothing had loaded under them.
    unloaded: HashSet<String>,
    /// The rows the last build left folded with their rows left out, and the
    /// row each left-out row hangs from.
    pruned: HashSet<String>,
    hidden: HashMap<String, String>,
    /// Rows the next build opens whatever the folds say: the rows above one
    /// a reveal is after.
    reveal: Vec<String>,
    /// The group the last build was made for, which `None` outer says is no
    /// build yet. A build for another group opens that one.
    group: Option<Option<String>>,
}

impl ConnectionsSidebar {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter loaded objects…"));
        cx.subscribe(&filter, |workspace, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                workspace.rebuild_connections(cx);
            }
        })
        .detach();
        // The tree emits `Expanded` for a click, `enter` and an arrow, and
        // also for every ancestor a reveal opens, so what listens must be
        // happy to hear it twice (`expand_node`).
        cx.subscribe(&tree, |workspace, _, event: &TreeEvent, cx| {
            if let TreeEvent::Expanded(id) = event {
                workspace.expand_node(id, cx);
            }
            workspace.tree_folded(cx);
        })
        .detach();
        Self {
            tree,
            nodes: Arc::default(),
            highlights: Arc::default(),
            shortcuts: Arc::default(),
            counts: Arc::default(),
            unsearched: 0,
            filter,
            folds_before_filter: None,
            selection: Selection::default(),
            unloaded: HashSet::new(),
            pruned: HashSet::new(),
            hidden: HashMap::new(),
            reveal: Vec::new(),
            group: None,
        }
    }
}

impl Workspace {
    /// The tree again from what every connection has loaded. Folds and the
    /// selected row are put back by id, so nothing a landing changes moves the
    /// user's place.
    pub(crate) fn rebuild_connections(&mut self, cx: &mut Context<Self>) {
        if self.view_mode() != model::ViewMode::Complex {
            return;
        }
        let tree = self.connections.tree.clone();
        let state = tree.read(cx);
        let roots: Vec<_> = (0..)
            .map_while(|ix| state.entry(ix))
            .filter(|entry| entry.depth() == 0)
            .map(|entry| entry.item().clone())
            .collect();
        let mut folds = model::held_folds(&roots, &self.connections.unloaded);
        // The tree is the user's unless a filter has opened it: the folds it
        // had are kept aside from the first key, and what it holds in between
        // is not theirs to be remembered.
        if self.connections.folds_before_filter.is_none() {
            self.capture_rows(&roots.iter().collect::<Vec<_>>(), &folds);
        }
        // While text is in the filter every kept row is open, so the folds
        // the user had are kept aside from its first key and come back when
        // the field is empty. Read on every rebuild: a landing in the
        // background rebuilds mid-filter, and must not mistake the opened
        // tree for the user's.
        let text = self.connections.filter.read(cx).value();
        let filter = text.trim();
        match (
            filter.is_empty(),
            self.connections.folds_before_filter.take(),
        ) {
            (true, Some(before)) => folds = before,
            (false, None) => self.connections.folds_before_filter = Some(folds.clone()),
            (false, kept) => self.connections.folds_before_filter = kept,
            (true, None) => {}
        }
        // What was stored opens a row once the tree has one to open: the rows
        // the tree holds are as it has them, so a fold made since is kept, and
        // the rest wait for their subtree to load.
        for row in self.profiles.iter().flat_map(|profile| &profile.open_rows) {
            folds.entry(row.clone()).or_insert(true);
        }
        for row in std::mem::take(&mut self.connections.reveal) {
            folds.insert(row, true);
        }
        // The group in front starts open. What the last build left of its
        // fold is only how it started, so it is let go when the front
        // connection has moved to another group; a fold made in the group
        // it stays in is kept.
        let current = self.current_group().map(str::to_string);
        if self.connections.group.as_ref() != Some(&current) {
            folds.remove(&model::group_row(current.as_deref()));
        }
        let selected = state.selected_item().map(|item| item.id.clone());

        let listed = self
            .profiles
            .iter()
            .map(|profile| {
                let levels = profile.config.target().levels();
                let database = profile.active_database();
                // What the filter matches by name it keeps whole: a connection
                // or a database named for it lists everything it has.
                let narrowed = match model::keeps_rows_whole(
                    filter,
                    &profile.name,
                    levels,
                    database.as_deref(),
                ) {
                    true => "",
                    false => filter,
                };
                Listed {
                    id: &profile.id,
                    name: &profile.name,
                    content: match (&profile.state, &profile.catalog) {
                        (ProfileState::Idle, _) => Content::Unloaded,
                        (ProfileState::Connecting { .. }, _) | (_, CatalogState::Loading) => {
                            Content::Loading
                        }
                        (ProfileState::Failed(message), _) | (_, CatalogState::Failed(message)) => {
                            Content::Failed(message.clone())
                        }
                        (ProfileState::Connected(_), CatalogState::Loaded(..)) => {
                            Content::Loaded(explorer_content(profile, narrowed))
                        }
                    },
                    databases: match (&profile.state, &profile.catalog) {
                        (ProfileState::Connected(_), CatalogState::Loaded(..))
                            if levels.lists_other_databases() =>
                        {
                            model::other_databases(&profile.databases.names, database.as_deref())
                                .into_iter()
                                .map(|name| {
                                    // A name the filter matches keeps its rows
                                    // whole, as the connection's own does.
                                    let narrowed = match model::database_kept_whole(filter, name) {
                                        true => "",
                                        false => filter,
                                    };
                                    (name.to_string(), database_content(profile, name, narrowed))
                                })
                                .collect()
                        }
                        _ => Vec::new(),
                    },
                    levels,
                    database,
                }
            })
            .collect();
        let built = model::build(
            &self.projects,
            listed,
            &folds,
            self.listing_scope(),
            current.as_deref(),
            filter,
        );
        self.connections.group = Some(current);
        self.connections.unloaded = built.unloaded;
        self.connections.pruned = built.pruned;
        self.connections.hidden = built.hidden;
        let front = self
            .profile()
            .map(|profile| SharedString::from(front_row_id(profile).unwrap_or_default()));

        // A row that no longer exists is no longer selected. A filter hides
        // rows without removing them, so it keeps what it hides.
        if filter.is_empty() {
            let present: HashSet<String> = built.nodes.keys().cloned().collect();
            self.connections.selection.retain(&present);
        }
        // No selection of the user's yet, and the tree's row only the one it
        // falls to: what was stored comes back as its rows appear.
        let restored = filter.is_empty()
            && self.connections.selection.rows.is_empty()
            && selected
                .as_ref()
                .is_none_or(|row| front.as_ref().is_some_and(|front| front == row));
        if restored {
            let rows: Vec<String> = self
                .profiles
                .iter()
                .flat_map(|profile| &profile.selected_rows)
                .filter(|row| built.nodes.contains_key(row.as_str()))
                .cloned()
                .collect();
            if !rows.is_empty() {
                self.connections.selection = Selection {
                    anchor: rows.last().cloned(),
                    rows,
                };
            }
        }
        self.remember_selection();
        self.connections.nodes = Arc::new(built.nodes);
        self.connections.highlights = Arc::new(built.highlights);
        self.connections.shortcuts = Arc::new(built.shortcuts);
        self.connections.counts = Arc::new(built.counts);
        self.connections.unsearched = built.unsearched;
        // When the row the tree had is gone, one of the rows still selected
        // is where it goes, so the selection is not left with none of its own.
        let kept = self
            .connections
            .selection
            .rows
            .last()
            .map(|id| SharedString::from(id.clone()));
        tree.update(cx, |tree, cx| {
            tree.set_items(built.items, cx);
            // A row the user moved to stays selected: a landing in the
            // background must not send Enter elsewhere. A selection that has
            // just come back from the file is where the tree goes, not the
            // row it fell to.
            let at = |id: Option<SharedString>| id.and_then(|id| tree.index_of(&id));
            let row = match restored {
                true => at(kept.clone()).or_else(|| at(selected.clone())),
                false => at(selected.clone()).or_else(|| at(kept)),
            }
            .or_else(|| at(front));
            tree.set_selected_index(row, cx);
        });
    }

    /// What the tree holds of every profile's open rows, put on the profile:
    /// the rows open in the tree, then the ones it has no say over yet, which
    /// wait for their subtree to load. `held` is what the tree has a say over.
    /// The rows are written out with the profile at its next save.
    fn capture_rows(&mut self, roots: &[&TreeItem], held: &HashMap<String, bool>) {
        let open: Vec<Vec<String>> = self
            .profiles
            .iter()
            .map(|profile| {
                roots
                    .iter()
                    .map(|root| model::open_rows_of(std::slice::from_ref(*root), &profile.id))
                    .find(|rows| !rows.is_empty())
                    .unwrap_or_default()
            })
            .collect();
        for (profile, open) in self.profiles.iter_mut().zip(open) {
            profile.open_rows = model::remembered_rows(open, &profile.open_rows, held);
        }
    }

    /// The selection, split between the profiles whose rows it holds. Nothing
    /// is selected until a click has, and nothing is written then, so a
    /// selection that is still on its way back from the file is not lost to
    /// one that was never made.
    fn remember_selection(&mut self) {
        let rows = &self.connections.selection.rows;
        if rows.is_empty() {
            return;
        }
        for profile in &mut self.profiles {
            profile.selected_rows = rows
                .iter()
                .filter(|row| model::profile_of_row(row) == Some(profile.id.as_str()))
                .take(OPEN_ROWS_CAP)
                .cloned()
                .collect();
        }
    }

    /// The connections tree as it stands now, put on the profiles that save
    /// it. Not while a filter has opened the tree: its folds are not the
    /// user's.
    pub(crate) fn capture_tree(&mut self, cx: &mut Context<Self>) {
        if self.view_mode() != model::ViewMode::Complex
            || self.connections.folds_before_filter.is_some()
        {
            return;
        }
        let state = self.connections.tree.clone();
        let state = state.read(cx);
        let roots: Vec<&TreeItem> = (0..)
            .map_while(|ix| state.entry(ix))
            .filter(|entry| entry.depth() == 0)
            .map(|entry| entry.item())
            .collect();
        let mut held = HashMap::new();
        for root in &roots {
            held.extend(model::held_folds(
                std::slice::from_ref(*root),
                &self.connections.unloaded,
            ));
        }
        self.capture_rows(&roots, &held);
        self.remember_selection();
    }

    /// A row of the connections tree was opened or folded: the tree as the
    /// user has it now is what the next start comes back to, and nothing else
    /// saves it.
    fn tree_folded(&mut self, cx: &mut Context<Self>) {
        if self.view_mode() != model::ViewMode::Complex {
            return;
        }
        self.capture_tree(cx);
        self.remember_profiles(cx);
    }

    /// What a row of one connection's own rows is: the object its leaf opens,
    /// resolved against the catalog that listed it, or the schema it names.
    /// `database` is the one the connections tree lists it under.
    pub(crate) fn row_node(
        &self,
        profile: &str,
        database: Option<&str>,
        inner: &str,
        leaf: Option<ExplorerLeaf>,
    ) -> Option<SelectedNode> {
        let (schema, name, kind) = match leaf {
            Some(leaf) => {
                let (schema, name, kind) = self.object_in(profile, database, leaf.target)?;
                (Some(schema), name, kind)
            }
            None => (None, schema_row_name(inner)?.to_string(), NodeKind::Schema),
        };
        Some(SelectedNode {
            profile: profile.to_string(),
            database: database.map(str::to_string),
            kind,
            name,
            schema,
            leaf,
        })
    }

    /// The node the sidebar has selected, in whichever tree is drawn.
    pub(crate) fn selected_node(&self, cx: &App) -> Option<SelectedNode> {
        if self.view_mode() == model::ViewMode::Complex {
            let id = self.connections.tree.read(cx).selected_item()?.id.clone();
            return self.connections_node(&id);
        }
        let profile = self.profile()?;
        let id = profile
            .session
            .explorer_tree
            .read(cx)
            .selected_item()?
            .id
            .clone();
        let leaf = profile.session.explorer_leaves.get(id.as_ref()).copied();
        self.row_node(&profile.id, None, &id, leaf)
    }

    /// The ids of the rows the connections tree has selected: the set a
    /// modifier click gathered while the tree's own row is in it, and that
    /// row alone once an arrow or a click has moved the tree off the set.
    pub(crate) fn selected_rows(&self, cx: &App) -> Vec<String> {
        let Some(front) = self
            .connections
            .tree
            .read(cx)
            .selected_item()
            .map(|item| item.id.to_string())
        else {
            return Vec::new();
        };
        let rows = &self.connections.selection.rows;
        match rows.contains(&front) {
            true => rows.clone(),
            false => vec![front],
        }
    }

    /// The nodes of those rows, in the order they were picked.
    pub(crate) fn selected_nodes(&self, cx: &App) -> Vec<SelectedNode> {
        self.nodes_of(&self.selected_rows(cx))
    }

    /// The nodes of the rows `ids` that are nodes an action applies to.
    fn nodes_of(&self, ids: &[String]) -> Vec<SelectedNode> {
        ids.iter()
            .filter_map(|id| self.connections_node(id))
            .collect()
    }

    /// What every one of `nodes` offers: the menu's and the palette's list of
    /// what a selection can do. Pin is withheld unless each is an object of
    /// its connection's own database that a shortcut can hold.
    pub(crate) fn selection_offers(&self, nodes: &[SelectedNode]) -> Vec<SelectionAction> {
        let mut items = Vec::new();
        for node in nodes {
            let Some(profile) = self.profiles.iter().find(|found| found.id == node.profile) else {
                return Vec::new();
            };
            items.push((node.kind, profile.config.target()));
        }
        let mut actions = model::selection_actions(&items);
        let pinnable = |node: &SelectedNode| {
            let own = self
                .profiles
                .iter()
                .find(|found| found.id == node.profile)
                .and_then(|found| found.active_database());
            let elsewhere = node
                .database
                .as_ref()
                .is_some_and(|database| own.as_ref() != Some(database));
            !elsewhere
                && node
                    .leaf
                    .is_some_and(|leaf| self.member_in(&node.profile, leaf.target).is_some())
        };
        if !nodes.iter().all(pinnable) {
            actions.retain(|action| *action != SelectionAction::Pin);
        }
        actions
    }

    /// Opens every row above `id` that the tree has left out, so that the row
    /// is there to be selected and scrolled to. The rows above it that are
    /// drawn are the tree's to open (`TreeState::reveal_item`).
    pub(crate) fn reveal_connections_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut above = Vec::new();
        let mut at = id.to_string();
        while let Some(parent) = self.connections.hidden.get(&at) {
            above.push(parent.clone());
            at = parent.clone();
        }
        if above.is_empty() {
            return;
        }
        self.connections.reveal.extend(above);
        self.rebuild_connections(cx);
    }

    /// What a click on the connections tree's row `id` does with `modifiers`
    /// held. Whether the tree is to be kept from its own click: a Cmd-, Ctrl-
    /// or Shift-click on a node changes the selection and nothing else, where
    /// the tree would also move its row and fold or unfold it.
    pub(crate) fn click_connections_row(
        &mut self,
        id: &str,
        opens: bool,
        modifiers: gpui::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if (modifiers.secondary() || modifiers.shift) && self.connections_node(id).is_some() {
            // The tree's own row is where a range or a toggle starts, whether
            // or not a modifier click has been there before.
            let current = self.selected_rows(cx);
            let selection = &mut self.connections.selection;
            if selection.rows != current {
                *selection = Selection {
                    anchor: current.last().cloned(),
                    rows: current,
                };
            }
            match modifiers.shift {
                true => {
                    let visible = self.visible_nodes(cx);
                    self.connections.selection.extend_to(id, &visible);
                }
                false => self.connections.selection.toggle(id),
            }
            let rows = &self.connections.selection.rows;
            let shown = match rows.iter().any(|row| row == id) {
                true => id.to_string(),
                false => rows.last().cloned().unwrap_or_else(|| id.to_string()),
            };
            self.connections.tree.update(cx, |tree, cx| {
                tree.set_selected_index(tree.index_of(&shown.into()), cx);
            });
            // Nothing folds or opens, so nothing else would save it.
            self.capture_tree(cx);
            self.remember_profiles(cx);
            cx.notify();
            return true;
        }
        self.connections.selection.click(id);
        // Saved by what the click does next, if it does anything: a fold, a
        // tab opened.
        self.capture_tree(cx);
        if opens {
            self.open_connection_row(id, window, cx);
        }
        false
    }

    /// The ids of the rows the tree is showing that an action applies to.
    fn visible_nodes(&self, cx: &App) -> Vec<String> {
        let tree = self.connections.tree.read(cx);
        (0..)
            .map_while(|ix| tree.entry(ix))
            .map(|entry| entry.item().id.to_string())
            .filter(|id| self.connections_node(id).is_some())
            .collect()
    }

    /// What the connections tree's row `id` is, if it is a node an action
    /// applies to: a connection, a database, a schema or an object.
    pub(crate) fn connections_node(&self, id: &str) -> Option<SelectedNode> {
        let named = |profile: &str, database: Option<&str>, kind, name: String| {
            Some(SelectedNode {
                profile: profile.to_string(),
                database: database.map(str::to_string),
                kind,
                name,
                schema: None,
                leaf: None,
            })
        };
        match self.connections.nodes.get(id)? {
            Node::Connection { profile } => {
                let name = self
                    .profiles
                    .iter()
                    .find(|found| found.id == *profile)?
                    .name
                    .clone();
                named(profile, None, NodeKind::Connection, name)
            }
            Node::Database { profile, database } => named(
                profile,
                Some(database),
                NodeKind::Database,
                database.clone(),
            ),
            Node::Row { leaf } => {
                let scoped = model::unscoped(id)?;
                self.row_node(scoped.profile, scoped.database, scoped.inner, *leaf)
            }
            Node::Group | Node::Notice => None,
        }
    }

    /// New Query Here: a query tab on `profile`, which comes to the front
    /// first, so the gate, the mode and the engine its statements meet are
    /// that connection's. Bound to `database` and `schema` where the
    /// connection has them to bind ([`Binding::for_target`]); on a connection
    /// row, which names no database, an ordinary tab on the profile's own
    /// session.
    pub(crate) fn new_query_here(
        &mut self,
        profile: &str,
        database: Option<String>,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.profiles.iter().position(|p| p.id == profile) else {
            return;
        };
        if self.active != Some(index) {
            self.activate(index, cx);
        }
        let binding = Binding::for_target(
            self.profiles[index].config.target(),
            database.as_deref(),
            schema.as_deref(),
        );
        self.open_query_tab(binding, window, cx);
    }

    /// A connection row was opened: connect it if it has not been asked to.
    /// Anything else lands here too (a reveal opens every ancestor of its
    /// row, a second expand repeats the first), and does nothing.
    pub(crate) fn expand_node(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        // A row built folded holds one inert row where its rows are: opened,
        // it is built again with them.
        if self.connections.pruned.contains(id.as_str()) {
            self.rebuild_connections(cx);
        }
        match self.connections.nodes.get(id.as_str()) {
            Some(Node::Connection { profile }) => {
                let profile = profile.clone();
                let Some(index) = self.profiles.iter().position(|p| p.id == profile) else {
                    return;
                };
                if matches!(
                    self.profiles[index].state,
                    ProfileState::Idle | ProfileState::Failed(_)
                ) {
                    self.begin_connect(index, cx);
                    return;
                }
                // Connected before the tree was in complex mode, or before
                // the server said what it holds: listed once, here.
                let profile = &self.profiles[index];
                if matches!(profile.catalog, CatalogState::Loaded(..))
                    && profile.databases.names.is_empty()
                {
                    let (id, generation) = (profile.id.clone(), profile.generation);
                    self.list_databases(&id, generation, cx);
                }
            }
            Some(Node::Database { profile, database }) => {
                let (profile, database) = (profile.clone(), database.clone());
                let Some(profile) = self.profiles.iter().find(|p| p.id == profile) else {
                    return;
                };
                // The connection's own database is read over its own
                // session; another is read over one of its own, opened on its
                // first expand and again after a failure to open it.
                let own = profile.active_database().as_deref() == Some(database.as_str());
                let reading = profile.bound.contains_key(&database)
                    || matches!(
                        profile.database_catalogs.get(&database),
                        Some(CatalogState::Loading | CatalogState::Loaded(..))
                    );
                if !own && !reading {
                    let id = profile.id.clone();
                    self.open_database(&id, database, cx);
                }
            }
            _ => {}
        }
    }

    /// What a click, or `enter`, on the row `id` does when the row opens
    /// something: the connection it belongs to comes to the front, then the
    /// object or the saved query opens on it. Whether it was one that does.
    pub(crate) fn open_connection_row(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(model::Scoped {
            profile,
            database,
            inner: row,
        }) = model::unscoped(id)
        else {
            return false;
        };
        let Some(index) = self.profiles.iter().position(|p| p.id == profile) else {
            return false;
        };
        // A row under another database opens through the catalog read for it,
        // and only an object does: the connection's saved queries and
        // shortcuts are not listed there.
        let elsewhere = database
            .filter(|database| self.profiles[index].active_database().as_deref() != Some(database))
            .map(str::to_string);
        let leaf = match self.connections.nodes.get(id) {
            Some(Node::Row { leaf }) => *leaf,
            _ => None,
        };
        let query = saved_query(row)
            .or_else(|| self.connections.shortcuts.get(id).and_then(Shortcut::query))
            .map(str::to_string)
            .filter(|_| elsewhere.is_none());
        if leaf.is_none() && query.is_none() {
            return false;
        }
        if self.active != Some(index) {
            self.activate(index, cx);
        }
        match (query, leaf, elsewhere) {
            (Some(name), ..) => self.open_saved_query(name, window, cx),
            (None, Some(leaf), Some(database)) => {
                self.open_object_on(profile, database, leaf.target, window, cx)
            }
            (None, Some(leaf), None) => self.open_explorer_target(leaf.target, window, cx),
            (None, None, _) => {}
        }
        true
    }

    pub(crate) fn render_connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let nodes = self.connections.nodes.clone();
        let highlights = self.connections.highlights.clone();
        let shortcuts = self.connections.shortcuts.clone();
        let counts = self.connections.counts.clone();
        // What each connection's rows are drawn in, by profile id: its logo
        // and the dialect its saved queries are written in.
        let targets: HashMap<String, (&'static str, Syntax, Levels)> = self
            .profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    (
                        profile.config.target().logo(),
                        profile.config.engine().syntax(),
                        profile.config.target().levels(),
                    ),
                )
            })
            .collect();
        // What a connection's row says about it, read from the profile on
        // every frame rather than baked into the tree, so a colour or a state
        // shows on the next frame without a rebuild.
        let looks: HashMap<String, RowLook> = self
            .profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    RowLook {
                        color: profile.color,
                        status: Status::from(&profile.state),
                        host: profile.config.endpoint(),
                    },
                )
            })
            .collect();
        // The rows a modifier click gathered, which the tree does not know to
        // draw as selected: a lone row is the tree's own.
        let selected = Arc::new(self.selected_rows(cx));
        let gathered: HashSet<String> = Some(selected.as_slice())
            .filter(|rows| rows.len() > 1)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let menu_workspace = workspace.clone();
        let menu_shortcuts = shortcuts.clone();
        let menu_selected = selected.clone();
        let (density, show_host, show_sizes) = (
            self.settings.density,
            self.settings.show_host,
            self.settings.show_sizes,
        );
        let tree = render_tree(&self.connections.tree, move |index, entry, _, _, cx| {
            let t = *theme(cx);
            let id = entry.item().id.as_str();
            let node = nodes.get(id);
            let scoped = model::unscoped(id);
            let inner = scoped.as_ref().map(|scoped| scoped.inner);
            let target = match (node, &scoped) {
                (Some(Node::Connection { profile }), _) => targets.get(profile.as_str()),
                (_, Some(scoped)) => targets.get(scoped.profile),
                _ => None,
            };
            let leaf = match node {
                Some(Node::Row { leaf }) => *leaf,
                _ => None,
            };
            let shortcut = shortcuts.get(id);
            let missing = matches!(shortcut, Some(Shortcut::Member { missing: true, .. }));
            let query = inner
                .and_then(saved_query)
                .or_else(|| shortcut.and_then(Shortcut::query))
                .map(str::to_string);
            let pinned_group = inner == Some(PINNED_GROUP);
            let schema_row = inner.is_some_and(is_schema_row);
            let folder_shortcut = matches!(shortcut, Some(Shortcut::Folder(_)));
            let label = entry.item().label.clone();
            let matched = highlights.get(id).map_or(&[][..], Vec::as_slice);

            let (label, row) = match node {
                Some(Node::Notice) => (
                    StyledText::new(label),
                    ListItem::new(index).text_color(t.text_muted),
                ),
                _ if missing => (
                    highlighted(label, matched, t),
                    ListItem::new(index).text_color(t.text_muted),
                ),
                _ if leaf.is_some() || query.is_some() || folder_shortcut => (
                    highlighted(label, matched, t),
                    ListItem::new(index).text_color(t.text),
                ),
                Some(Node::Group | Node::Connection { .. } | Node::Database { .. }) => (
                    highlighted(label, matched, t),
                    ListItem::new(index)
                        .text_color(t.text)
                        .font_weight(FontWeight::MEDIUM),
                ),
                _ if schema_row => (
                    highlighted(label, matched, t),
                    ListItem::new(index)
                        .text_color(t.text)
                        .font_weight(FontWeight::MEDIUM),
                ),
                // A category only labels the run of objects under it.
                _ => (
                    StyledText::new(label.to_uppercase()),
                    ListItem::new(index)
                        .text_color(t.text_faint)
                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                        .font_weight(FontWeight::MEDIUM),
                ),
            };
            // A folder shows which way it is facing; an object shows what kind
            // of object it is. Both occupy the same slot, so the labels line up.
            let lead = match (node, leaf, shortcut) {
                (Some(Node::Notice), ..) => None,
                (_, Some(leaf), _) => Some(object_icon(leaf.kind)),
                (
                    _,
                    None,
                    Some(Shortcut::Member {
                        member: Member::Object(object),
                        ..
                    }),
                ) => Some(object_icon(object.kind)),
                _ if query.is_some() || missing => {
                    target.map(|(_, syntax, _)| saved_query_icon(*syntax))
                }
                _ if entry.is_expanded() => Some(icon::CHEVRON_DOWN),
                _ => Some(icon::CHEVRON_RIGHT),
            };
            let heading = match node {
                Some(Node::Group) => Some(icon::PROJECT),
                Some(Node::Connection { .. }) => target.map(|(logo, ..)| *logo),
                Some(Node::Database { .. }) => Some(icon::DATABASE),
                _ if inner == Some(QUERIES_GROUP) => {
                    target.map(|(_, syntax, _)| queries_group_icon(*syntax))
                }
                _ if pinned_group => Some(icon::PIN),
                // A shape that lists databases as schemas, or whose one
                // schema is its database, draws that row as a database.
                _ if schema_row => Some(
                    match target.is_some_and(|(.., levels)| schema_row_is_a_database(*levels)) {
                        true => icon::DATABASE,
                        false => icon::SCHEMA,
                    },
                ),
                _ if folder_shortcut => Some(icon::FOLDER),
                _ => None,
            };
            let connection = match node {
                Some(Node::Connection { profile }) => looks
                    .get(profile.as_str())
                    .map(|look| (profile.as_str(), look)),
                _ => None,
            };
            let host = connection
                .map(|(_, look)| look.host.as_str())
                .filter(|host| show_host && !host.is_empty());
            // The fill sits under the row's own content; `ListItem` draws the
            // selection and the hover over it, so a selected row still reads
            // as selected.
            let row = row
                .mx(px(layout::SPACE_XS))
                .rounded(px(layout::RADIUS_CONTROL))
                .when_some(connection.and_then(|(_, look)| look.color), |row, color| {
                    row.bg(color.fill())
                })
                .when(gathered.contains(id), |row| {
                    // What the tree draws its own selected row with.
                    row.bg(match cx.theme().list.active_highlight {
                        true => cx.theme().list_active,
                        false => cx.theme().accent,
                    })
                })
                // `ListItem` sizes its text in `rems`, which tracks the
                // library's 16 rather than dbdelve's body size.
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                // The tree gives every row the height of its first, so each
                // is told its density's.
                .h(px(layout::chrome(density.row_height())))
                .pl(px(
                    layout::SPACE_SM + entry.depth() as f32 * density.indent()
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .min_w_0()
                        .flex_1()
                        .when_some(lead, |row, path| {
                            row.child(match missing {
                                true => row_icon_tinted(t, path, None).into_any_element(),
                                false => row_icon(t, path).into_any_element(),
                            })
                        })
                        .when_some(heading, |row, path| {
                            row.child(match connection {
                                Some((profile, look)) => {
                                    let profile = profile.to_string();
                                    div()
                                        .flex_none()
                                        .debug_selector(move || {
                                            format!("connections-logo-{profile}")
                                        })
                                        .child(row_icon_tinted(t, path, look.color))
                                        .into_any_element()
                                }
                                None => row_icon(t, path).into_any_element(),
                            })
                        })
                        .child(match pinned_group {
                            // The tree gives every row the height of its first:
                            // the empty text keeps this one a line tall.
                            true => div().flex_1().flex().items_center().child("\u{200B}"),
                            // A host after the name takes the room the name
                            // leaves, so the name shortens only once the host
                            // has none.
                            false => {
                                let name = div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .when(host.is_none(), |name| name.flex_1())
                                    .child(label);
                                match connection {
                                    Some((profile, _)) => {
                                        let profile = profile.to_string();
                                        name.debug_selector(move || {
                                            format!("connections-name-{profile}")
                                        })
                                    }
                                    None => name,
                                }
                            }
                        })
                        .when_some(connection.zip(host), |row, ((profile, _), host)| {
                            let profile = profile.to_string();
                            row.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                    .debug_selector(move || format!("connections-host-{profile}"))
                                    .child(host.to_string()),
                            )
                        })
                        .when_some(counts.get(id), |row, count| {
                            let tag = connection.map(|(profile, _)| profile.to_string());
                            row.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                    .when_some(tag, |count, profile| {
                                        count.debug_selector(move || {
                                            format!("connections-count-{profile}")
                                        })
                                    })
                                    .child(count.to_string()),
                            )
                        })
                        .when_some(
                            shown_size(leaf.and_then(|leaf| leaf.size), show_sizes),
                            |row, size| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .text_color(t.text_faint)
                                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                                        .child(human_bytes(size)),
                                )
                            },
                        )
                        // The user's own connection, in a slot as wide as the
                        // icon column.
                        .when_some(connection, |row, (profile, look)| {
                            let (slot, dot) = (profile.to_string(), profile.to_string());
                            let status = look.status;
                            row.child(
                                div()
                                    .flex_none()
                                    .size(px(layout::chrome(layout::ICON_SIZE)))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .debug_selector(move || format!("connections-status-{slot}"))
                                    .child(status_dot(t, status).debug_selector(move || {
                                        format!("connections-dot-{dot}-{}", status_name(status))
                                    })),
                            )
                        })
                        .when_some(
                            connection.and_then(|(profile, look)| look.color.map(|_| profile)),
                            |row, profile| {
                                let profile = profile.to_string();
                                row.debug_selector(move || format!("connections-fill-{profile}"))
                            },
                        ),
                )
                .debug_selector(move || format!("connections-row-{index}"));
            if matches!(node, Some(Node::Notice)) {
                return row;
            }
            let opens = !missing && (leaf.is_some() || query.is_some());
            let workspace = workspace.clone();
            let id = id.to_string();
            row.on_click(move |event, window, cx| {
                let modifiers = event.modifiers();
                let gathered = workspace.update(cx, |workspace, cx| {
                    workspace.click_connections_row(&id, opens, modifiers, window, cx)
                });
                if gathered.unwrap_or(false) {
                    cx.stop_propagation();
                }
            })
        })
        .context_menu(move |_, entry, menu, window, cx| {
            let id = entry.item().id.as_str();
            let Some(entity) = menu_workspace.upgrade() else {
                return menu;
            };
            let state = entity.read(cx);
            // A row of several selected is the menu of all of them.
            // As of the last frame: the tree is being updated while its menu
            // is built, so it cannot be read for its row.
            let rows = &menu_selected;
            if rows.len() > 1 && rows.iter().any(|row| row == id) {
                let nodes = state.nodes_of(rows);
                if nodes.len() == rows.len() {
                    let kinds: Vec<_> = nodes.iter().map(|node| node.kind).collect();
                    return selection_menu(
                        menu,
                        model::selection_header(&kinds),
                        state.selection_offers(&nodes),
                        &menu_workspace,
                    );
                }
            }
            let Some(node) = state.connections_node(id) else {
                return menu;
            };
            let shortcut = menu_shortcuts.get(id).cloned();
            let inner = model::unscoped(id).map(|scoped| scoped.inner.to_string());
            match node.kind {
                NodeKind::Connection | NodeKind::Database => {
                    let Some(target) = state
                        .profiles
                        .iter()
                        .find(|found| found.id == node.profile)
                        .map(|found| found.config.target())
                    else {
                        return menu;
                    };
                    let actions = node_actions(node.kind, &node.name, target);
                    node_items(menu, &node, actions, &menu_workspace)
                }
                // A row of one connection's own rows: the menu simple mode
                // draws for it.
                _ => explorer_menu(
                    menu,
                    MenuRow {
                        id: inner.unwrap_or_default().into(),
                        label: entry.item().label.clone(),
                        profile: node.profile.clone(),
                        database: node.database.clone(),
                        leaf: node.leaf,
                        shortcut,
                    },
                    &menu_workspace,
                    window,
                    cx,
                ),
            }
        });
        let t = *theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            // The same quiet filter row simple mode draws over its tree.
            .child(
                div()
                    .w_full()
                    .h(px(layout::chrome(layout::TAB_HEIGHT)))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .px(px(layout::SPACE_SM))
                    .border_b_1()
                    .border_color(t.border)
                    .child(row_icon(t, icon::SEARCH))
                    // Clipped by a box of its own: the input lays its
                    // placeholder out at its full width.
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Input::new(&self.connections.filter)
                                .w_full()
                                .min_w_0()
                                .appearance(false),
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .key_context("Explorer")
                    .track_focus(&self.explorer_focus)
                    .on_action(cx.listener(Self::open_sidebar_item))
                    .child(tree),
            )
            .children(unsearched_line(self.connections.unsearched).map(|line| {
                div()
                    .debug_selector(|| "connections-unsearched".into())
                    .flex_shrink_0()
                    .flex()
                    .items_start()
                    .gap(px(layout::SPACE_XS))
                    .px(px(layout::SPACE_SM))
                    .py(px(layout::SPACE_XS))
                    .border_t_1()
                    .border_color(t.border)
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(div().flex_none().child(row_icon(t, icon::SEARCH)))
                    // Wrapped, not clipped: the sidebar is narrow, and a
                    // sentence cut off mid-word says nothing.
                    .child(div().flex_1().min_w_0().child(line))
            }))
    }
}

/// What the foot of the filtered tree says about the connections the filter
/// could not look inside, or nothing when it looked inside them all.
fn unsearched_line(unsearched: usize) -> Option<String> {
    match unsearched {
        0 => None,
        1 => Some("1 connection is not loaded and was not searched".into()),
        n => Some(format!(
            "{n} connections are not loaded and were not searched"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Engine;

    fn node(kind: NodeKind, database: Option<&str>, name: &str) -> SelectedNode {
        SelectedNode {
            profile: "p".into(),
            database: database.map(str::to_string),
            kind,
            name: name.into(),
            schema: None,
            leaf: None,
        }
    }

    fn offered(
        engine: Engine,
        database: Option<&str>,
        row: &str,
    ) -> (Option<String>, Option<String>) {
        new_query_binding(
            &node(NodeKind::Schema, database, row),
            Target::built_in(engine),
        )
    }

    #[test]
    fn a_connection_row_opens_an_ordinary_tab_and_a_database_row_a_bound_one() {
        let target = Target::built_in(Engine::Postgres);
        assert_eq!(
            new_query_binding(&node(NodeKind::Connection, None, "Prod"), target),
            (None, None),
            "a connection row names no database"
        );
        assert_eq!(
            new_query_binding(&node(NodeKind::Database, Some("app"), "app"), target),
            (Some("app".into()), None)
        );
    }

    #[test]
    fn a_schema_row_binds_by_the_levels_of_its_connection_and_by_its_id_not_its_label() {
        assert_eq!(
            offered(Engine::Postgres, Some("app"), "we ird/name"),
            (Some("app".into()), Some("we ird/name".into())),
            "a schema under the database it sits in, spelled as the server listed it"
        );
        assert_eq!(
            offered(Engine::MySql, None, "shop"),
            (Some("shop".into()), None),
            "MySQL lists a database as a schema"
        );
        assert_eq!(
            offered(Engine::Sqlite, None, "main"),
            (None, None),
            "a target with no database to switch opens an ordinary tab"
        );
        assert_eq!(
            offered(Engine::Postgres, None, "public"),
            (None, None),
            "so does a schema whose database is not known"
        );
    }
}

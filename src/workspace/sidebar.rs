//! The connections sidebar of complex mode: every connection under its
//! project, one tree for the window rather than one per connection.
//!
//! A connection's own rows are the rows simple mode draws for it
//! (`profiles::explorer_content`), under ids that name the connection, so the
//! same table of two connections is two rows. Acting on a row of a connection
//! that is not in front makes it the one in front first: the gate, the mode and
//! the engine a statement meets are always those of the connection it was
//! opened on.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{Entity, SharedString, StyledText};
use gpui_component::tree::{TreeEvent, TreeState};

use super::profiles::{database_content, explorer_content};
use super::*;
use crate::db::{Levels, Syntax};
use crate::explorer::{
    Member, PINNED_GROUP, QUERIES_GROUP, Shortcut, folds, is_schema_row, saved_query,
};
use crate::icons::{queries_group_icon, saved_query_icon};
use crate::sidebar::{self as model, Content, Listed, Node, schema_row_is_a_database, shown_size};
use crate::ui::{Status, row_icon_tinted, status_dot};

/// The id of the row of `profile`'s front tab in the connections tree, if
/// the tab has one.
pub(crate) fn front_row_id(profile: &Profile) -> Option<String> {
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
        let mut folds = folds(&roots);
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
        let front = self
            .profile()
            .map(|profile| SharedString::from(front_row_id(profile).unwrap_or_default()));

        self.connections.nodes = Arc::new(built.nodes);
        self.connections.highlights = Arc::new(built.highlights);
        self.connections.shortcuts = Arc::new(built.shortcuts);
        self.connections.counts = Arc::new(built.counts);
        self.connections.unsearched = built.unsearched;
        tree.update(cx, |tree, cx| {
            tree.set_items(built.items, cx);
            // A row the user moved to stays selected: a landing in the
            // background must not send Enter elsewhere.
            let row = selected
                .and_then(|selected| tree.index_of(&selected))
                .or_else(|| front.and_then(|front| tree.index_of(&front)));
            tree.set_selected_index(row, cx);
        });
    }

    /// A connection row was opened: connect it if it has not been asked to.
    /// Anything else lands here too (a reveal opens every ancestor of its
    /// row, a second expand repeats the first), and does nothing.
    pub(crate) fn expand_node(&mut self, id: &SharedString, cx: &mut Context<Self>) {
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
        // Only the connection's own database opens through its own catalog.
        if database.is_some_and(|database| {
            self.profiles[index].active_database().as_deref() != Some(database)
        }) {
            return false;
        }
        let leaf = match self.connections.nodes.get(id) {
            Some(Node::Row { leaf }) => *leaf,
            _ => None,
        };
        let query = saved_query(row)
            .or_else(|| self.connections.shortcuts.get(id).and_then(Shortcut::query))
            .map(str::to_string);
        if leaf.is_none() && query.is_none() {
            return false;
        }
        if self.active != Some(index) {
            self.activate(index, cx);
        }
        match (query, leaf) {
            (Some(name), _) => self.open_saved_query(name, window, cx),
            (None, Some(leaf)) => self.open_explorer_target(leaf.target, window, cx),
            (None, None) => {}
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
            if missing || (leaf.is_none() && query.is_none()) {
                return row;
            }
            let workspace = workspace.clone();
            let id = id.to_string();
            row.on_click(move |_, window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.open_connection_row(&id, window, cx)
                });
            })
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

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

use super::profiles::explorer_content;
use super::*;
use crate::db::Syntax;
use crate::explorer::{
    Member, PINNED_GROUP, QUERIES_GROUP, Shortcut, folds, is_schema_row, saved_query,
};
use crate::icons::{queries_group_icon, saved_query_icon};
use crate::sidebar::{self as model, Content, Listed, Node};
use crate::ui::row_icon_tinted;

pub(crate) struct ConnectionsSidebar {
    pub(crate) tree: Entity<TreeState>,
    /// What each row stands for, by row id.
    pub(crate) nodes: Arc<HashMap<String, Node>>,
    pub(crate) highlights: Arc<HashMap<String, Vec<Range<usize>>>>,
    pub(crate) shortcuts: Arc<HashMap<String, Shortcut>>,
}

impl ConnectionsSidebar {
    pub(crate) fn new(cx: &mut Context<Workspace>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
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
        }
    }
}

impl Workspace {
    /// The tree again from what every connection has loaded. Folds and the
    /// selected row are put back by id, so nothing a landing changes moves the
    /// user's place.
    pub(crate) fn rebuild_connections(&mut self, cx: &mut Context<Self>) {
        if self.settings.view_mode != model::ViewMode::Complex {
            return;
        }
        let tree = self.connections.tree.clone();
        let state = tree.read(cx);
        let roots: Vec<_> = (0..)
            .map_while(|ix| state.entry(ix))
            .filter(|entry| entry.depth() == 0)
            .map(|entry| entry.item().clone())
            .collect();
        let folds = folds(&roots);
        let selected = state.selected_item().map(|item| item.id.clone());

        let listed = self
            .profiles
            .iter()
            .map(|profile| Listed {
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
                        Content::Loaded(explorer_content(profile, ""))
                    }
                },
            })
            .collect();
        let built = model::build(&self.projects, listed, &folds);
        let front = self.profile().map(|profile| {
            let row = profile
                .front_row()
                .map_or_else(String::new, |row| model::scoped_row(&profile.id, &row));
            SharedString::from(row)
        });

        self.connections.nodes = Arc::new(built.nodes);
        self.connections.highlights = Arc::new(built.highlights);
        self.connections.shortcuts = Arc::new(built.shortcuts);
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
        let Some(Node::Connection { profile }) = self.connections.nodes.get(id.as_str()) else {
            return;
        };
        let Some(index) = self.profiles.iter().position(|p| &p.id == profile) else {
            return;
        };
        if matches!(
            self.profiles[index].state,
            ProfileState::Idle | ProfileState::Failed(_)
        ) {
            self.begin_connect(index, cx);
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
        let Some((profile, row)) = model::unscoped(id) else {
            return false;
        };
        let Some(index) = self.profiles.iter().position(|p| p.id == profile) else {
            return false;
        };
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
        // What each connection's rows are drawn in, by profile id: its logo
        // and the dialect its saved queries are written in.
        let targets: HashMap<String, (&'static str, Syntax)> = self
            .profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    (
                        profile.config.target().logo(),
                        profile.config.engine().syntax(),
                    ),
                )
            })
            .collect();
        let tree = render_tree(&self.connections.tree, move |index, entry, _, _, cx| {
            let t = *theme(cx);
            let id = entry.item().id.as_str();
            let node = nodes.get(id);
            let scoped = model::unscoped(id);
            let inner = scoped.map(|(_, row)| row);
            let target = match (node, scoped) {
                (Some(Node::Connection { profile }), _) => targets.get(profile.as_str()),
                (_, Some((profile, _))) => targets.get(profile),
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
                Some(Node::Group | Node::Connection { .. }) => (
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
                    target.map(|(_, syntax)| saved_query_icon(*syntax))
                }
                _ if entry.is_expanded() => Some(icon::CHEVRON_DOWN),
                _ => Some(icon::CHEVRON_RIGHT),
            };
            let heading = match node {
                Some(Node::Group) => Some(icon::PROJECT),
                Some(Node::Connection { .. }) => target.map(|(logo, _)| *logo),
                _ if inner == Some(QUERIES_GROUP) => {
                    target.map(|(_, syntax)| queries_group_icon(*syntax))
                }
                _ if pinned_group => Some(icon::PIN),
                _ if schema_row => Some(icon::SCHEMA),
                _ if folder_shortcut => Some(icon::FOLDER),
                _ => None,
            };
            let row = row
                .mx(px(layout::SPACE_XS))
                .rounded(px(layout::RADIUS_CONTROL))
                // `ListItem` sizes its text in `rems`, which tracks the
                // library's 16 rather than dbdelve's body size.
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .pl(px(
                    layout::SPACE_SM + entry.depth() as f32 * layout::SPACE_MD
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
                        .when_some(heading, |row, path| row.child(row_icon(t, path)))
                        .child(match pinned_group {
                            // The tree gives every row the height of its first:
                            // the empty text keeps this one a line tall.
                            true => div().flex_1().flex().items_center().child("\u{200B}"),
                            false => div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(label),
                        })
                        .when_some(leaf.and_then(|leaf| leaf.size), |row, size| {
                            row.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                    .child(human_bytes(size)),
                            )
                        }),
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
        div().size_full().flex().flex_col().child(
            div()
                .flex_1()
                .min_h_0()
                .key_context("Explorer")
                .track_focus(&self.explorer_focus)
                .on_action(cx.listener(Self::open_sidebar_item))
                .child(tree),
        )
    }
}

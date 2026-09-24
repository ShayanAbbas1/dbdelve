//! `cmd+p` and `cmd+shift+p`: one list and one matcher, two jobs (spec §3.4).
//!
//! The jump is flat over everything the active profile can open, so a table
//! three schemas down is a few keystrokes away without touching the tree. The
//! command palette is over verbs, and it offers only the verbs that apply to
//! what is on screen — a Run on a tab with no buffer is a row to read past.
//!
//! Neither surface decides anything. A row carries a [`Command`], the workspace
//! runs it through the same methods the buttons and keystrokes call, and the
//! palette is gone by the time it happens.

use gpui::{App, Context, IntoElement, ParentElement, SharedString, Styled, Task, Window, div, px};
use gpui_component::{
    IndexPath,
    list::{ListDelegate, ListItem, ListState},
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    Workspace,
    db::{ExplainMode, RelationKind, RoutineKind},
    explorer::{ExplorerTarget, ObjectKind},
    export::Format,
    icons::icon,
    session::{CatalogState, ObjectBody, Profile, QueryState, Tab, routine_name},
    theme::{FontSlot, fonts, layout, theme},
    ui::{chord_hint, object_icon, row_icon},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `cmd+p` — everything this profile can open, in one flat list.
    Jump,
    /// `cmd+shift+p` — the verbs that apply right now.
    Commands,
    /// The statements this profile has run, reached from the command palette.
    History,
    /// Every family the text system knows, for one of the three slots. One mode
    /// with the slot in it rather than three: the list is the same list, and
    /// only the row's destination differs.
    Font(FontSlot),
}

/// What a row does when it is confirmed.
///
/// Every variant names something the workspace already does for a button or a
/// keystroke: the palette is another way to reach them, not a second
/// implementation of them.
#[derive(Clone)]
pub enum Command {
    OpenObject(ExplorerTarget),
    OpenQuery(String),
    OpenScratch,
    NewQuery,
    RunQuery,
    /// Ask how the statement would run. Both modes are offered as their own
    /// row: which one is which is the whole decision, and a palette that made
    /// the user guess would be hiding the one that writes.
    ExplainQuery(ExplainMode),
    /// Rewrite the buffer as formatted SQL.
    FormatQuery,
    /// Flip the results pane between the rows and the plan.
    ShowPlan(bool),
    SaveQuery,
    RenameQuery,
    /// Open the history list. The one command that puts the palette back up
    /// rather than doing something behind it.
    QueryHistory,
    /// Put a statement that has already been run back in the buffer.
    RecallStatement(String),
    ShowStructure(bool),
    RefreshRelation(u64),
    NextPage,
    PreviousPage,
    /// Put the cursor in the preview's filter, adding the bar to type into when
    /// there is none. The palette does not type the filter; it gets the user to
    /// the place they would have clicked.
    FilterRows,
    ClearFilter,
    /// Open the "New row" form over the preview. Insertion needs no primary
    /// key, so this is offered where the grid refuses to edit.
    NewRow,
    CloseObject(u64),
    /// Stage a `NULL`, the empty string or `DEFAULT` on the active cell. The
    /// rows are how the gestures stop being folklore; the cell menu dispatches
    /// the same actions.
    SetNull,
    SetEmpty,
    SetDefault,
    /// Generate the one-row `DELETE` and show it for confirmation. Offered only
    /// where the grid can name the row by its primary key (spec §5).
    DeleteRow,
    ApplyEdits,
    DiscardEdits,
    /// The format here only picks the extension the save dialog suggests. What
    /// the file is written as is read back off the path the user confirmed, so
    /// these two rows are one code path — see `export::Format::for_path`.
    ExportResults(Format),
    SwitchProfile(usize),
    NextProfile,
    PreviousProfile,
    NewConnection,
    RefreshConnection,
    CycleTheme,
    /// Put the palette back up over the font list for this slot, the way
    /// [`Command::QueryHistory`] does for the history.
    PickFont(FontSlot),
    SetFont(FontSlot, String),
    ToggleSidebar,
    ToggleRowPanel,
    ResetEditorZoom,
    OpenSettings,
}

struct Item {
    /// What the matcher scores, and what the row reads as. One string for both,
    /// so nothing can be found by text that is not on screen.
    label: String,
    /// Muted, at the far end: what kind of object this is, or the stroke that
    /// does the same job without the palette.
    hint: SharedString,
    icon: &'static str,
    command: Command,
}

impl Item {
    fn command(
        label: &str,
        hint: impl Into<SharedString>,
        icon: &'static str,
        command: Command,
    ) -> Self {
        Self {
            label: label.to_string(),
            hint: hint.into(),
            icon,
            command,
        }
    }
}

pub struct Palette {
    mode: Mode,
    items: Vec<Item>,
    /// Indices into `items`, best first. The list renders through this, so the
    /// order on screen is the matcher's order.
    matched: Vec<usize>,
    matcher: Matcher,
}

impl Palette {
    pub fn new(mode: Mode, workspace: &Workspace, cx: &App) -> Self {
        let items = match workspace.profile() {
            Some(profile) => match mode {
                Mode::Jump => jump_items(profile),
                Mode::Commands => command_items(workspace, profile, cx),
                Mode::History => history_items(profile),
                Mode::Font(slot) => font_items(slot, cx),
            },
            None => Vec::new(),
        };
        Self {
            mode,
            matched: (0..items.len()).collect(),
            items,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn len(&self) -> usize {
        self.matched.len()
    }

    pub fn command(&self, row: usize) -> Option<&Command> {
        let index = *self.matched.get(row)?;
        Some(&self.items.get(index)?.command)
    }

    pub fn placeholder(&self) -> &'static str {
        match self.mode {
            Mode::Jump => "Go to a table, view, routine or saved query…",
            Mode::Commands => "Run a command…",
            Mode::History => "Recall a statement you have run…",
            Mode::Font(_) => "Pick a font…",
        }
    }
}

impl ListDelegate for Palette {
    type Item = ListItem;

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.matched = matches(&self.items, query, &mut self.matcher);
        Task::ready(())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let t = *theme(cx);
        let item = self.items.get(*self.matched.get(ix.row)?)?;

        Some(
            ListItem::new(ix.row)
                .rounded(px(layout::RADIUS_CONTROL))
                // As in the explorer tree: `ListItem`'s own text size is in
                // `rems` and would otherwise ignore dbdelve's type scale.
                .text_size(px(layout::TEXT_MD))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .w_full()
                        .min_w_0()
                        .child(row_icon(t, item.icon))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(layout::TEXT_XS))
                                .text_color(t.text_faint)
                                .child(item.hint.clone()),
                        ),
                ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .p(px(layout::SPACE_MD))
            .text_color(theme(cx).text_muted)
            .child("No matches.")
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
    }
}

/// Everything the active profile can open: its buffers first, because they are
/// what somebody wrote, then the database's own objects.
fn jump_items(profile: &Profile) -> Vec<Item> {
    let session = &profile.session;
    let mut items = vec![Item::command(
        "New Query",
        "scratch",
        icon::SCRATCH_QUERY,
        Command::OpenScratch,
    )];

    items.extend(session.saved_queries.iter().map(|name| Item {
        label: name.clone(),
        hint: "query".into(),
        icon: icon::SAVED_QUERY,
        command: Command::OpenQuery(name.clone()),
    }));

    let CatalogState::Loaded(catalog) = &profile.catalog else {
        return items;
    };
    for (schema_index, schema) in catalog.schemas.iter().enumerate() {
        items.extend(
            schema
                .relations
                .iter()
                .enumerate()
                .map(|(relation_index, relation)| {
                    let kind = ObjectKind::Relation(relation.kind);
                    Item {
                        // Qualified, so a name that appears in three schemas is three
                        // rows that can be told apart, and "pub acc" reaches one of them.
                        label: format!("{}.{}", schema.name, relation.name),
                        hint: kind_label(kind).into(),
                        icon: object_icon(kind),
                        command: Command::OpenObject(ExplorerTarget::Relation {
                            schema_index,
                            relation_index,
                        }),
                    }
                }),
        );
        items.extend(
            schema
                .routines
                .iter()
                .enumerate()
                .map(|(routine_index, routine)| {
                    let kind = ObjectKind::Routine(routine.kind);
                    Item {
                        label: format!("{}.{}", schema.name, routine_name(routine)),
                        hint: kind_label(kind).into(),
                        icon: object_icon(kind),
                        command: Command::OpenObject(ExplorerTarget::Routine {
                            schema_index,
                            routine_index,
                        }),
                    }
                }),
        );
    }
    items
}

/// Every statement this profile has run, newest first.
fn history_items(profile: &Profile) -> Vec<Item> {
    profile
        .session
        .history
        .iter()
        .map(|sql| Item {
            label: one_line(sql),
            hint: "".into(),
            icon: icon::HISTORY,
            command: Command::RecallStatement(sql.clone()),
        })
        .collect()
}

/// Every family the text system can resolve, for one slot.
///
/// The leading-dot names are dropped: those are the platform's own internal
/// faces and gpui's aliases, and a row nobody should pick is a row to read
/// past. Sorted, because the unfiltered list is the list in the order it was
/// built and an install has hundreds of families in it.
///
/// ponytail: every row is drawn in the chrome face, and nothing here knows
/// which families are monospaced -- so a face is picked by name, and the grid
/// slot will take a proportional one. Render each label in its own family and
/// filter the grid's list by advance width if picking blind starts to cost.
fn font_items(slot: FontSlot, cx: &App) -> Vec<Item> {
    let current = fonts(cx).family(slot).clone();
    let mut names = cx.text_system().all_font_names();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .map(|name| Item {
            hint: if name.as_str() == current.as_ref() {
                "current"
            } else {
                ""
            }
            .into(),
            icon: icon::FONT,
            command: Command::SetFont(slot, name.clone()),
            label: name,
        })
        .collect()
}

/// A statement as a row: one line, however many it was written across. The
/// label is what the matcher scores as well as what the row reads as, so
/// collapsing the whitespace is also what makes `select from accounts` find a
/// statement whose `FROM` was on its own line.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The verbs, filtered to the ones that mean something from where the user is
/// standing. A palette that lists what it cannot do is a palette to read past.
fn command_items(workspace: &Workspace, profile: &Profile, cx: &App) -> Vec<Item> {
    let session = &profile.session;
    // Every cap here is the chord the action actually holds, so a rebind shows
    // up in the palette rather than only in settings.
    let overrides = &workspace.settings.custom_keybindings;
    let runnable = session.editor(session.active).is_some();
    let mut items = vec![Item::command(
        "New query",
        chord_hint("new_query", overrides),
        icon::SCRATCH_QUERY,
        Command::NewQuery,
    )];

    if runnable {
        items.push(Item::command(
            "Run query",
            chord_hint("run_query", overrides),
            icon::RUN,
            Command::RunQuery,
        ));
        // Offered only in the modes this engine has: SQLite cannot report what
        // a run actually cost, and a row that can only produce an error is a
        // row to read past.
        let engine = profile.config.engine();
        items.extend(
            ExplainMode::ALL
                .into_iter()
                .filter(|mode| engine.explain_prefix(*mode).is_some())
                .map(|mode| {
                    Item::command(
                        mode.label(),
                        match mode {
                            ExplainMode::Plan => chord_hint("explain_query", overrides),
                            ExplainMode::Analyze => String::new(),
                        },
                        icon::PLAN,
                        Command::ExplainQuery(mode),
                    )
                }),
        );
        items.push(Item::command(
            "Format query",
            chord_hint("format_query", overrides),
            icon::STRUCTURE,
            Command::FormatQuery,
        ));
        if let Some(tab) = session.active_query_tab()
            && tab.plan.is_some()
        {
            items.push(match tab.showing_plan {
                true => Item::command("Show data", "", icon::TABLE, Command::ShowPlan(false)),
                false => Item::command("Show plan", "", icon::PLAN, Command::ShowPlan(true)),
            });
        }
        if session.open_query().is_some() {
            items.push(Item::command(
                "Rename query",
                "",
                icon::RENAME,
                Command::RenameQuery,
            ));
        } else {
            items.push(Item::command(
                "Save query",
                chord_hint("rename_query_tab", overrides),
                icon::SAVE,
                Command::SaveQuery,
            ));
        }
    }

    // Not gated on the query tab being in front: recalling a statement brings
    // it forward, which is where the statement is going anyway.
    if !session.history.is_empty() {
        items.push(Item::command(
            "Query history",
            "",
            icon::HISTORY,
            Command::QueryHistory,
        ));
    }

    if let Some(tab) = session.active_object() {
        if let ObjectBody::Relation {
            showing_structure,
            query,
            filter,
            limit,
            offset,
            ..
        } = &tab.body
        {
            items.push(if *showing_structure {
                Item::command("Show data", "", icon::TABLE, Command::ShowStructure(false))
            } else {
                Item::command(
                    "Show structure",
                    "",
                    icon::STRUCTURE,
                    Command::ShowStructure(true),
                )
            });
            items.push(Item::command(
                "Refresh rows",
                chord_hint("refresh_relation", overrides),
                icon::RUN,
                Command::RefreshRelation(tab.id),
            ));
            // The same gates the pager buttons stand behind: forward only off
            // a full page, backwards only off a page that is not the first.
            if !*showing_structure {
                if matches!(query, QueryState::Complete { rows, .. } if *rows >= *limit) {
                    items.push(Item::command(
                        "Next page",
                        "",
                        icon::CHEVRON_RIGHT,
                        Command::NextPage,
                    ));
                }
                if *offset > 0 {
                    items.push(Item::command(
                        "Previous page",
                        "",
                        icon::CHEVRON_LEFT,
                        Command::PreviousPage,
                    ));
                }
                items.push(Item::command(
                    "Filter rows…",
                    "",
                    icon::SEARCH,
                    Command::FilterRows,
                ));
                // A row that would do nothing is worse than no row at all.
                if !filter.is_empty() {
                    items.push(Item::command(
                        "Clear filter",
                        "",
                        icon::CLOSE,
                        Command::ClearFilter,
                    ));
                }
                items.push(Item::command("New row…", "", icon::PLUS, Command::NewRow));
                // Only on a row dbdelve can name by its primary key -- the same
                // condition that makes a cell of it editable.
                if workspace.has_nameable_row(cx) {
                    items.push(Item::command(
                        "Delete row…",
                        "",
                        icon::DELETE,
                        Command::DeleteRow,
                    ));
                }
            }
        }
        items.push(Item::command(
            "Close tab",
            chord_hint("close_tab", overrides),
            icon::CLOSE,
            Command::CloseObject(tab.id),
        ));
    }

    // Only over a grid that has a result set behind it. A surface that has run
    // nothing has nothing to write out.
    if workspace.has_results(cx) {
        items.push(Item::command(
            "Export results as CSV",
            "",
            icon::SAVE,
            Command::ExportResults(Format::Csv),
        ));
        items.push(Item::command(
            "Export results as JSON",
            "",
            icon::SAVE,
            Command::ExportResults(Format::Json),
        ));
    }

    // Only where the ring is on a cell that can actually take one.
    if workspace.has_editable_cell(cx) {
        items.push(Item::command(
            "Set cell to NULL",
            chord_hint("set_null", overrides),
            icon::RENAME,
            Command::SetNull,
        ));
        items.push(Item::command(
            "Set cell to empty",
            "",
            icon::RENAME,
            Command::SetEmpty,
        ));
        // Offered on every editable cell rather than only where a default is
        // known. The palette is reached from anywhere and knows no column; the
        // cell menu is the surface that hides what would be refused.
        items.push(Item::command(
            "Set cell to default",
            "",
            icon::RENAME,
            Command::SetDefault,
        ));
    }

    // Only while there is something to write back, for the reason the footer's
    // pair of buttons is conditional too.
    if workspace.has_pending_edits(cx) {
        items.push(Item::command(
            "Apply edits",
            "",
            icon::SAVE,
            Command::ApplyEdits,
        ));
        items.push(Item::command(
            "Discard edits",
            "",
            icon::DELETE,
            Command::DiscardEdits,
        ));
    }

    items.extend(
        workspace
            .profiles
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != workspace.active)
            .map(|(index, other)| Item {
                label: format!("Switch to {}", other.name),
                hint: "".into(),
                icon: icon::DATABASE,
                command: Command::SwitchProfile(index),
            }),
    );

    if workspace.profiles.len() > 1 {
        items.push(Item::command(
            "Next connection",
            chord_hint("next_profile", overrides),
            icon::DATABASE,
            Command::NextProfile,
        ));
        items.push(Item::command(
            "Previous connection",
            chord_hint("previous_profile", overrides),
            icon::DATABASE,
            Command::PreviousProfile,
        ));
    }

    items.push(Item::command(
        "New connection",
        chord_hint("new_connection", overrides),
        icon::PLUS,
        Command::NewConnection,
    ));
    items.push(Item::command(
        "Refresh connection",
        chord_hint("refresh_connection", overrides),
        icon::DATABASE,
        Command::RefreshConnection,
    ));
    items.push(Item::command(
        "Cycle theme",
        chord_hint("cycle_theme", overrides),
        icon::SWITCHER,
        Command::CycleTheme,
    ));
    for (label, slot) in [
        ("Chrome font", FontSlot::Chrome),
        ("Editor font", FontSlot::Editor),
        ("Grid font", FontSlot::Grid),
    ] {
        items.push(Item::command(
            label,
            "",
            icon::FONT,
            Command::PickFont(slot),
        ));
    }
    items.push(Item::command(
        "Settings",
        chord_hint("open_settings", overrides),
        icon::SWITCHER,
        Command::OpenSettings,
    ));
    // One row rather than a Show/Hide pair: the palette is built from the
    // session, which does not know whether the column is folded.
    items.push(Item::command(
        "Toggle sidebar",
        chord_hint("toggle_sidebar", overrides),
        icon::SIDEBAR,
        Command::ToggleSidebar,
    ));
    items.push(Item::command(
        "Toggle row panel",
        chord_hint("toggle_row_panel", overrides),
        icon::ROW_PANEL,
        Command::ToggleRowPanel,
    ));
    if matches!(session.active, Tab::Query(_)) {
        items.push(Item::command(
            "Reset editor zoom",
            chord_hint("reset_editor_zoom", overrides),
            icon::SEARCH,
            Command::ResetEditorZoom,
        ));
    }
    items
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Relation(RelationKind::Table) => "table",
        ObjectKind::Relation(RelationKind::PartitionedTable) => "partitioned table",
        ObjectKind::Relation(RelationKind::View) => "view",
        ObjectKind::Relation(RelationKind::MaterializedView) => "materialized view",
        ObjectKind::Relation(RelationKind::ForeignTable) => "foreign table",
        ObjectKind::Routine(RoutineKind::Function) => "function",
        ObjectKind::Routine(RoutineKind::Procedure) => "procedure",
    }
}

/// Score every label against the query and keep the survivors, best first.
///
/// An empty query scores everything at zero, and the sort is stable, so the
/// unfiltered list is the list in the order it was built.
fn matches(items: &[Item], query: &str, matcher: &mut Matcher) -> Vec<usize> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let label = Utf32Str::new(&item.label, &mut buffer);
            pattern.score(label, matcher).map(|score| (index, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .map(|label| Item::command(label, "", icon::TABLE, Command::NewQuery))
            .collect()
    }

    fn matched(labels: &[&str], query: &str) -> Vec<String> {
        let items = items(labels);
        let mut matcher = Matcher::new(Config::DEFAULT);
        matches(&items, query, &mut matcher)
            .into_iter()
            .map(|index| items[index].label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_every_row_in_the_order_it_was_built() {
        let labels = ["public.accounts", "public.events", "analytics.events"];
        assert_eq!(matched(&labels, ""), labels);
    }

    #[test]
    fn a_query_drops_what_it_does_not_match_and_ranks_what_it_does() {
        let labels = ["analytics.events", "public.accounts", "public.account_log"];
        // The gaps in the subsequence are what the ranking is about: the whole
        // word beats the same letters spread across a longer name.
        assert_eq!(
            matched(&labels, "account"),
            ["public.accounts", "public.account_log"]
        );
        assert!(matched(&labels, "zzz").is_empty());
    }

    #[test]
    fn a_statement_written_across_lines_reads_and_matches_as_one() {
        let sql = "SELECT *\n  FROM accounts\n WHERE id = 1;";
        assert_eq!(one_line(sql), "SELECT * FROM accounts WHERE id = 1;");
        assert_eq!(matched(&[&one_line(sql)], "from accounts").len(), 1);
    }

    #[test]
    fn the_filter_rows_are_found_by_the_word_the_user_would_type() {
        // A palette row nobody can find by typing the obvious word is a row
        // that is not in the palette.
        let labels = ["Filter rows…", "Clear filter", "Next page"];
        assert_eq!(matched(&labels, "filter"), ["Filter rows…", "Clear filter"]);
        assert_eq!(matched(&labels, "clear"), ["Clear filter"]);
    }

    #[test]
    fn a_schema_prefix_narrows_to_one_schema() {
        let labels = ["analytics.events", "public.events"];
        assert_eq!(matched(&labels, "pub ev"), ["public.events"]);
    }
}

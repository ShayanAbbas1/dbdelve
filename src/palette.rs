//! `cmd+p` and `cmd+shift+p`: one palette, one matcher, several scopes.
//!
//! The palette searches everything the active profile can reach -- its verbs,
//! its tables and routines, its saved queries, the statements it has run and
//! the other connections -- and the scope pills narrow it to one of those.
//! `cmd+p` opens on Objects and `cmd+shift+p` on Commands; `tab` moves
//! between scopes. The pickers a command opens (themes, fonts, databases, schemas,
//! columns) are the same card over a list of their own.
//!
//! The command scope offers only the verbs that apply to what is on screen: a
//! Run on a tab with no buffer is a row to read past.
//!
//! Neither surface decides anything. A row carries a [`Command`], the workspace
//! runs it through the same methods the buttons and keystrokes call, and the
//! palette is gone by the time it happens.

use std::{collections::HashMap, ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ListSizingBehavior, MouseButton, ParentElement, Pixels,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    div, prelude::FluentBuilder, px, size,
};
use gpui_component::{
    VirtualListScrollHandle,
    input::{Input, InputEvent, InputState},
    v_virtual_list,
};

use crate::{
    Workspace,
    actions::{PaletteNext, PaletteNextScope, PalettePrevious, PalettePreviousScope},
    db::{ExplainMode, RelationKind, RoutineKind},
    explorer::{ExplorerTarget, ObjectKind},
    export::Format,
    fuzzy::{Fuzzy, Query},
    icons::icon,
    import::Source,
    session::{CatalogState, ObjectBody, Profile, QueryState, Tab, routine_name},
    theme::{
        ConnectionColor, FontSlot, Theme, fonts, install_theme, layout, popup_theme, theme, user,
    },
    ui::{
        chord_for, highlighted, key_chip, keycap_parts, keycaps, kind_color, object_icon, one_line,
        raised_card, row_icon_tinted,
    },
    workspace::opacity_for,
};

/// How many rows each group shows under All, which is for reaching across
/// scopes in a few keystrokes rather than for reading one of them whole.
const ALL_SCOPE_GROUP_ROWS: usize = 5;
/// How many of the rows last run the Recent group shows.
const RECENT_ROWS: usize = 5;
/// How many confirmed rows the workspace remembers for the Recent group.
pub const RECENTS_KEPT: usize = 8;
const RECENT_GROUP: &str = "Recent";

/// What the search palette is narrowed to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    All,
    Commands,
    Objects,
    Queries,
    History,
    Connections,
}

impl Scope {
    pub const ALL: [Scope; 6] = [
        Scope::All,
        Scope::Commands,
        Scope::Objects,
        Scope::Queries,
        Scope::History,
        Scope::Connections,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Scope::All => "All",
            Scope::Commands => "Commands",
            Scope::Objects => "Objects",
            Scope::Queries => "Queries",
            Scope::History => "History",
            Scope::Connections => "Connections",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Scope::All => "Search commands, tables, queries and connections…",
            Scope::Commands => "Run a command…",
            Scope::Objects => "Go to a table, view or routine…",
            Scope::Queries => "Open a saved query…",
            Scope::History => "Recall a statement you have run…",
            Scope::Connections => "Switch to a connection…",
        }
    }

    fn step(self, delta: isize) -> Self {
        let at = Scope::ALL
            .iter()
            .position(|scope| *scope == self)
            .unwrap_or(0) as isize;
        Scope::ALL[(at + delta).rem_euclid(Scope::ALL.len() as isize) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The palette proper, narrowed to one scope.
    Search(Scope),
    /// Every family the text system knows, for one of the three slots. One mode
    /// with the slot in it rather than three: the list is the same list, and
    /// only the row's destination differs.
    Font(FontSlot),
    /// Every theme, then a row per app to import one from. The highlighted
    /// theme is installed as it moves, and only a confirmed one is kept.
    Theme,
    /// Every theme one app has on this machine, previewed as the theme list
    /// previews, and imported when confirmed.
    ImportTheme(user::Source),
    /// The databases on the active profile's server, as last fetched.
    Database,
    /// The schemas in the active profile's catalog, to pick the active one.
    Schema,
    /// The columns of the grid in front, to jump to one of them.
    Columns,
}

impl Mode {
    fn scope(self) -> Option<Scope> {
        match self {
            Mode::Search(scope) => Some(scope),
            _ => None,
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Mode::Search(scope) => scope.placeholder(),
            Mode::Font(_) => "Pick a font…",
            Mode::Theme => "Pick a theme…",
            Mode::ImportTheme(_) => "Import a theme…",
            Mode::Database => "Switch database…",
            Mode::Schema => "Switch schema…",
            Mode::Columns => "Go to a column…",
        }
    }

    /// The one pill a picker shows in place of the scopes, naming its list.
    fn title(self) -> &'static str {
        match self {
            Mode::Search(scope) => scope.title(),
            Mode::Font(FontSlot::Chrome) => "Chrome font",
            Mode::Font(FontSlot::Editor) => "Editor font",
            Mode::Font(FontSlot::Grid) => "Grid font",
            Mode::Theme => "Themes",
            Mode::ImportTheme(_) => "Import theme",
            Mode::Database => "Databases",
            Mode::Schema => "Schemas",
            Mode::Columns => "Columns",
        }
    }

    /// What `enter` does to the row, for the footer.
    fn verb(self) -> &'static str {
        match self {
            Mode::Search(Scope::Commands) => "Run",
            Mode::Search(_) | Mode::Columns => "Open",
            _ => "Select",
        }
    }
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
    /// Narrow the palette to the History scope. The one command that keeps the
    /// palette up rather than doing something behind it.
    QueryHistory,
    /// Put a statement that has already been run back in the buffer.
    RecallStatement(String),
    ShowStructure(bool),
    RefreshRelation(u64),
    /// Run the relation's `COUNT(*)` under its filter: the status bar's Count.
    CountRows(u64),
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
    ImportRows,
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
    NextEdit,
    PreviousEdit,
    /// The format here only picks the extension the save dialog suggests. What
    /// the file is written as is read back off the path the user confirmed, so
    /// these two rows are one code path — see `export::Format::for_path`.
    ExportResults(Format),
    CopyRow,
    CopyResults(Format),
    SwitchProfile(usize),
    NextProfile,
    PreviousProfile,
    NewConnection,
    NewProject,
    ImportConnections(Source),
    RefreshConnection,
    /// Put the palette back up over the theme list.
    SelectTheme,
    SetTheme(Box<Theme>),
    /// Put the palette back up over the themes an app has, to import one.
    BrowseThemes(user::Source),
    /// By index into [`user::Source::found`], which stands for the process.
    ImportTheme(user::Source, usize),
    /// Fetch the server's databases and put the palette back up over them.
    SelectDatabase,
    SetDatabase(String),
    /// Put the palette back up over the catalog's schemas.
    SelectSchema,
    SetSchema(String),
    /// Put the palette back up over the font list for this slot.
    PickFont(FontSlot),
    SetFont(FontSlot, String),
    ToggleSidebar,
    ToggleRowPanel,
    ToggleRowEdit,
    ResetEditorZoom,
    OpenSettings,
    /// Put the palette up over the columns of the grid in front.
    GoToColumn,
    /// Scroll the grid in front to this column, by index into its columns.
    RevealColumn(usize),
}

struct Item {
    /// What the matcher scores, and what the row reads as. One string for both,
    /// so nothing can be found by text that is not on screen.
    label: String,
    /// Muted and monospaced, inline after the label: where a connection points.
    detail: Option<SharedString>,
    /// Muted, at the far end: what kind of object this is, or which row is the
    /// one in force.
    hint: SharedString,
    /// The chord the action holds, drawn as a keycap per key.
    keys: Option<String>,
    icon: &'static str,
    tint: Option<ConnectionColor>,
    /// A statement, drawn in the editor's face.
    mono: bool,
    scope: Scope,
    /// The header the row sits under. Empty for a list that has one group.
    group: &'static str,
    command: Command,
}

impl Item {
    fn new(label: impl Into<String>, icon: &'static str, command: Command) -> Self {
        Self {
            label: label.into(),
            detail: None,
            hint: SharedString::default(),
            keys: None,
            icon,
            tint: None,
            mono: false,
            scope: Scope::Commands,
            group: "",
            command,
        }
    }

    fn hint(mut self, hint: impl Into<SharedString>) -> Self {
        self.hint = hint.into();
        self
    }

    fn keys(mut self, keys: Option<String>) -> Self {
        self.keys = keys;
        self
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn tint(mut self, tint: Option<ConnectionColor>) -> Self {
        self.tint = tint;
        self
    }

    fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    fn scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    fn group(mut self, group: &'static str) -> Self {
        self.group = group;
        self
    }
}

/// One line of the list: a group's header, or a row by its index into the
/// palette's items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Header(&'static str),
    Item(usize),
}

/// The rows to draw for `query` over `items`, in order.
///
/// Rows are ranked by the matcher, then gathered under their groups, and the
/// groups are ordered by their best row: the first row is always the best
/// match, and a group's rows stay together beneath it. Under All a group is a
/// scope, cut to its best few. With nothing typed, the rows confirmed most
/// recently lead under Recent, and are not repeated further down -- except
/// where the list is already in an order of its own: History is newest first,
/// and Connections is grouped by project.
fn arrange(
    items: &[Item],
    scope: Option<Scope>,
    query: &Query,
    fuzzy: &mut Fuzzy,
    recents: &[(Scope, String)],
) -> Vec<Row> {
    let across = matches!(scope, None | Some(Scope::All));
    let ranked = fuzzy.rank(
        query,
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| across || Some(item.scope) == scope),
        |(_, item)| item.label.as_str(),
    );

    let recent: Vec<usize> = if query.is_empty()
        && matches!(scope, Some(Scope::All | Scope::Commands | Scope::Objects))
    {
        recents
            .iter()
            .filter_map(|(scope, label)| {
                ranked
                    .iter()
                    .map(|((index, _), _)| *index)
                    .find(|&index| items[index].scope == *scope && items[index].label == *label)
            })
            .take(RECENT_ROWS)
            .collect()
    } else {
        Vec::new()
    };

    let mut groups: Vec<(&'static str, Vec<usize>)> = Vec::new();
    if !recent.is_empty() {
        groups.push((RECENT_GROUP, recent.clone()));
    }
    for ((index, item), _) in ranked {
        if recent.contains(&index) {
            continue;
        }
        let group = match scope {
            Some(Scope::All) => item.scope.title(),
            _ => item.group,
        };
        match groups.iter_mut().find(|(name, _)| *name == group) {
            Some((_, members)) => members.push(index),
            None => groups.push((group, vec![index])),
        }
    }

    let mut rows = Vec::new();
    for (name, mut members) in groups {
        if scope == Some(Scope::All) {
            members.truncate(ALL_SCOPE_GROUP_ROWS);
        }
        if !name.is_empty() {
            rows.push(Row::Header(name));
        }
        rows.extend(members.into_iter().map(Row::Item));
    }
    rows
}

/// The row `delta` rows on from `from`, skipping headers and wrapping: a list
/// this short is faster to reach the end of from the top than by holding a key.
fn step(rows: &[Row], from: Option<usize>, delta: isize) -> Option<usize> {
    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| matches!(row, Row::Item(_)).then_some(at))
        .collect();
    if selectable.is_empty() {
        return None;
    }
    let count = selectable.len() as isize;
    let next = match from.and_then(|row| selectable.iter().position(|&at| at == row)) {
        Some(at) => (at as isize + delta).rem_euclid(count),
        None if delta < 0 => count - 1,
        None => 0,
    };
    Some(selectable[next as usize])
}

fn first_item(rows: &[Row]) -> Option<usize> {
    rows.iter().position(|row| matches!(row, Row::Item(_)))
}

/// What a stroke that opens the palette in some mode does to the one up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stroke {
    Close,
    /// Narrow to that scope, keeping what was typed.
    Rescope(Scope),
    /// Put up that list in its place.
    Replace,
}

/// The stroke the palette opened with closes it even after `tab` has moved
/// it on, as does the one for the scope it shows; the other search stroke
/// moves it to that stroke's scope, so the two are one keystroke apart.
fn stroke(opened: Mode, showing: Mode, mode: Mode) -> Stroke {
    match (showing, mode) {
        _ if mode == opened || mode == showing => Stroke::Close,
        (Mode::Search(_), Mode::Search(scope)) => Stroke::Rescope(scope),
        _ => Stroke::Replace,
    }
}

pub enum PaletteEvent {
    /// A row was confirmed. The label goes with it so the workspace can lead
    /// the next palette with it under Recent.
    Confirm {
        command: Command,
        scope: Scope,
        label: String,
    },
    Dismiss,
}

pub struct Palette {
    mode: Mode,
    items: Vec<Item>,
    rows: Vec<Row>,
    /// Each row's height, for the virtual list, rebuilt with `rows`.
    sizes: Rc<Vec<gpui::Size<Pixels>>>,
    /// Into `rows`, never a header.
    selected: Option<usize>,
    query: Query,
    fuzzy: Fuzzy,
    /// The matched characters of each item drawn so far under this query, so a
    /// repaint draws them rather than matching again.
    highlights: HashMap<usize, Rc<[Range<usize>]>>,
    recents: Vec<(Scope, String)>,
    /// The chords drawn in the footer and the field, read from the keymap so a
    /// rebind shows up here too: previous row, next row, next scope, close.
    hint_chords: [Option<String>; 4],
    /// The mode the palette opened in, for [`stroke`].
    opened: Mode,
    input: Entity<InputState>,
    scroll: VirtualListScrollHandle,
    /// The theme in force when a theme list opened, for a filter that
    /// matches nothing, or a row that is not a theme, to fall back to.
    unpreviewed: Option<Theme>,
    /// Opacity per theme as of opening, so a previewed theme paints at its own.
    theme_opacity: HashMap<String, f32>,
    fallback_opacity: f32,
    _input_events: Subscription,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Palette {
    pub fn new(
        mode: Mode,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let items = match (mode, workspace.profile()) {
            (Mode::Theme, _) => theme_items(cx),
            (Mode::ImportTheme(source), _) => import_items(source),
            (Mode::Search(_), Some(profile)) => search_items(workspace, profile, cx),
            (Mode::Font(slot), Some(_)) => font_items(slot, cx),
            (Mode::Database, Some(profile)) => database_items(profile),
            (Mode::Schema, Some(profile)) => schema_items(profile),
            (Mode::Columns, Some(profile)) => column_items(profile, cx),
            (_, None) => Vec::new(),
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(mode.placeholder()));
        let input_events = cx.subscribe_in(&input, window, Self::on_input_event);
        let mut palette = Self {
            mode,
            items,
            rows: Vec::new(),
            sizes: Rc::default(),
            selected: None,
            query: Query::new(""),
            fuzzy: Fuzzy::new(),
            highlights: HashMap::new(),
            recents: workspace.palette_recents.clone(),
            hint_chords: [
                "palette_previous",
                "palette_next",
                "palette_next_scope",
                "show_editor",
            ]
            .map(|id| chord_for(id, &workspace.settings.custom_keybindings)),
            opened: mode,
            input,
            scroll: VirtualListScrollHandle::new(),
            unpreviewed: matches!(mode, Mode::Theme | Mode::ImportTheme(_)).then(|| *theme(cx)),
            theme_opacity: workspace.settings.theme_opacity.clone(),
            fallback_opacity: workspace.settings.opacity,
            _input_events: input_events,
        };
        palette.rearrange();
        palette.preview(window, cx);
        palette
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn stroke(&self, mode: Mode) -> Stroke {
        stroke(self.opened, self.mode, mode)
    }

    /// Narrow to another scope, keeping what has been typed.
    pub fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Search(_)) || self.mode == Mode::Search(scope) {
            return;
        }
        self.mode = Mode::Search(scope);
        self.input.update(cx, |input, cx| {
            input.set_placeholder(scope.placeholder(), window, cx)
        });
        self.rearrange();
        cx.notify();
    }

    /// Put the selection on an item, by its index in the order it was built.
    pub fn select_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.iter().position(|row| *row == Row::Item(index)) {
            self.select(Some(row), true, window, cx);
        }
    }

    fn rearrange(&mut self) {
        self.rows = arrange(
            &self.items,
            self.mode.scope(),
            &self.query,
            &mut self.fuzzy,
            &self.recents,
        );
        let header = px(layout::chrome(layout::PALETTE_HEADER_HEIGHT));
        let row = px(layout::chrome(layout::PALETTE_ROW_HEIGHT));
        self.sizes = Rc::new(
            self.rows
                .iter()
                .map(|line| match line {
                    Row::Header(_) => size(px(0.), header),
                    Row::Item(_) => size(px(0.), row),
                })
                .collect(),
        );
        self.highlights.clear();
        self.selected = first_item(&self.rows);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let text = self.input.read(cx).value();
                self.query = Query::new(&text);
                self.rearrange();
                self.preview(window, cx);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.confirm(self.selected, cx),
            _ => {}
        }
    }

    fn select(
        &mut self,
        row: Option<usize>,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let upwards = matches!((self.selected, row), (Some(from), Some(to)) if to < from);
        self.selected = row;
        if let Some(row) = row.filter(|_| reveal) {
            // Going up, the first row of a group brings its header into view
            // with it. Going down, the row is what has to show: `Nearest` lines
            // the target up with the bottom edge, and a header there would
            // leave the row it heads just below it.
            let target = match row.checked_sub(1).map(|above| self.rows[above]) {
                Some(Row::Header(_)) if upwards => row - 1,
                _ => row,
            };
            self.scroll.scroll_to_item(target, ScrollStrategy::Nearest);
        }
        self.preview(window, cx);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let row = step(&self.rows, self.selected, delta);
        self.select(row, true, window, cx);
    }

    fn cycle_scope(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::Search(scope) = self.mode {
            self.set_scope(scope.step(delta), window, cx);
        }
    }

    fn confirm(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        let Some(Row::Item(index)) = row.and_then(|row| self.rows.get(row).copied()) else {
            return;
        };
        let item = &self.items[index];
        cx.emit(PaletteEvent::Confirm {
            command: item.command.clone(),
            scope: item.scope,
            label: item.label.clone(),
        });
    }

    /// The live preview. Nothing here is saved, and nothing here undoes it:
    /// `Workspace::end_theme_preview` does that on the way out.
    fn preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(unpreviewed) = self.unpreviewed else {
            return;
        };
        let command = match self.selected.and_then(|row| self.rows.get(row)) {
            Some(Row::Item(index)) => Some(&self.items[*index].command),
            _ => None,
        };
        let previewed = match command {
            Some(Command::SetTheme(candidate)) => Some(**candidate),
            Some(Command::ImportTheme(source, index)) => source.theme(*index),
            _ => Some(unpreviewed),
        };
        if let Some(previewed) = previewed {
            let opacity = opacity_for(&self.theme_opacity, self.fallback_opacity, &previewed);
            install_theme(previewed.with_opacity(opacity), window, cx);
            cx.refresh_windows();
        }
    }

    fn highlights(&mut self, index: usize) -> Rc<[Range<usize>]> {
        if let Some(ranges) = self.highlights.get(&index) {
            return ranges.clone();
        }
        let ranges: Rc<[Range<usize>]> = self
            .fuzzy
            .highlights(&self.query, &self.items[index].label)
            .into();
        self.highlights.insert(index, ranges.clone());
        ranges
    }

    fn render_row(&mut self, at: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = popup_theme(cx);
        let index = match self.rows[at] {
            Row::Header(name) => {
                return div()
                    .h(px(layout::chrome(layout::PALETTE_HEADER_HEIGHT)))
                    .px(px(layout::SPACE_LG))
                    .pb(px(layout::SPACE_XS))
                    .flex()
                    .items_end()
                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text_muted)
                    .child(name)
                    .into_any_element();
            }
            Row::Item(index) => index,
        };
        let ranges = self.highlights(index);
        let selected = self.selected == Some(at);
        let item = &self.items[index];
        let mono = fonts(cx).family(FontSlot::Editor).clone();
        let row_height = layout::chrome(layout::PALETTE_ROW_HEIGHT);
        let tile = layout::chrome(layout::PALETTE_TILE_SIZE);
        let bar = layout::chrome(layout::PALETTE_ACCENT_BAR_HEIGHT);

        div()
            .id(("palette-row", at))
            .h(px(row_height))
            .px(px(layout::SPACE_SM))
            .child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_MD))
                    .px(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_PANEL))
                    .when(selected, |row| {
                        row.bg(t.element_active).child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px((row_height - bar) / 2.))
                                .w(px(layout::PALETTE_ACCENT_BAR_WIDTH))
                                .h(px(bar))
                                .rounded_full()
                                .bg(t.accent),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(tile))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(layout::RADIUS_CONTROL))
                            .border_1()
                            .border_color(t.border)
                            .bg(t.element_hover)
                            .child(row_icon_tinted(
                                t,
                                item.icon,
                                item.tint.or_else(|| kind_color(item.icon)),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(t.text)
                                    .when(item.mono, |label| label.font_family(mono.clone()))
                                    .child(highlighted(item.label.clone(), &ranges, t)),
                            )
                            .children(item.detail.clone().map(|detail| {
                                div()
                                    .flex_shrink_0()
                                    .font_family(mono.clone())
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .text_color(t.text_faint)
                                    .child(detail)
                            })),
                    )
                    .when(!item.hint.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                .text_color(t.text_faint)
                                .child(item.hint.clone()),
                        )
                    })
                    .children(item.keys.as_deref().map(|chord| keycaps(chord, t))),
            )
            .on_mouse_move(cx.listener(move |palette, _, window, cx| {
                // Hover moves the selection only when the pointer does, so a
                // list scrolling under a resting pointer keeps the keyboard's.
                // Never over a theme list, where selecting repaints the window
                // and a pointer crossing it would flash every theme it passed.
                if palette.selected != Some(at) && palette.unpreviewed.is_none() {
                    palette.select(Some(at), false, window, cx);
                }
            }))
            .on_click(cx.listener(move |palette, _, _, cx| palette.confirm(Some(at), cx)))
            .into_any_element()
    }

    fn render_scopes(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let pills: Vec<AnyElement> = match self.mode {
            Mode::Search(current) => Scope::ALL
                .into_iter()
                .enumerate()
                .map(|(index, scope)| {
                    scope_pill(("palette-scope", index), scope.title(), scope == current, t)
                        .on_click(cx.listener(move |palette, _, window, cx| {
                            palette.set_scope(scope, window, cx);
                        }))
                        .into_any_element()
                })
                .collect(),
            mode => vec![scope_pill("palette-list", mode.title(), true, t).into_any_element()],
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .px(px(layout::SPACE_MD))
            .py(px(layout::SPACE_SM))
            .border_b_1()
            .border_color(t.border)
            .children(pills)
    }

    fn render_empty(&self, t: Theme) -> impl IntoElement {
        let (title, line) = match self.mode {
            Mode::ImportTheme(source) if self.items.is_empty() => (
                "No themes found".to_string(),
                format!("{} has no themes on this machine.", source.name()),
            ),
            // `cmd+p` opens on Objects, which is empty until the catalog loads
            // and on a connection that has none.
            Mode::Search(scope) if self.query.is_empty() && scope != Scope::All => (
                format!("No {} here", scope.title().to_lowercase()),
                "Press Tab to search another scope.".to_string(),
            ),
            Mode::Search(Scope::All) => (
                "No results".to_string(),
                "Try another word, or fewer letters of it.".to_string(),
            ),
            Mode::Search(_) => (
                "No results".to_string(),
                "Try another word, or press Tab to search another scope.".to_string(),
            ),
            _ => (
                "No results".to_string(),
                "Try another word, or fewer letters of it.".to_string(),
            ),
        };
        div()
            .py(px(layout::SPACE_LG * 2.))
            .px(px(layout::SPACE_LG))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .child(div().text_color(t.text).child(title))
            .child(
                div()
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(line),
            )
    }

    fn render_footer(&self, t: Theme) -> impl IntoElement {
        let [previous, next, scope, close] = self.hint_chords.each_ref().map(Option::as_deref);
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(layout::SPACE_LG))
            .px(px(layout::SPACE_LG))
            .py(px(layout::SPACE_SM))
            .border_t_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_LG))
                    .child(footer_hint(&[previous, next], "Navigate", t))
                    .child(footer_hint(&[Some("enter")], self.mode.verb(), t))
                    .when(matches!(self.mode, Mode::Search(_)), |hints| {
                        hints.child(footer_hint(&[scope], "Scope", t))
                    }),
            )
            .child(footer_hint(&[close], "Close", t))
    }
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = popup_theme(cx);
        let viewport = window.viewport_size();
        let top = layout::TITLEBAR_HEIGHT * 2.;
        let width = layout::PALETTE_WIDTH
            .min(viewport.width.as_f32() - 2. * layout::SPACE_LG)
            .max(0.);
        let list_height =
            (viewport.height.as_f32() - top - layout::chrome(layout::PALETTE_CHROME_HEIGHT))
                .clamp(layout::PALETTE_MIN_HEIGHT, layout::PALETTE_MAX_HEIGHT);

        let list = if self.rows.is_empty() {
            self.render_empty(t).into_any_element()
        } else {
            div()
                .relative()
                .flex()
                .flex_col()
                .max_h(px(list_height))
                .py(px(layout::SPACE_XS))
                .overflow_hidden()
                .child(
                    v_virtual_list(
                        cx.entity(),
                        "palette-rows",
                        self.sizes.clone(),
                        |palette, range, _, cx| {
                            range
                                .map(|at| palette.render_row(at, cx))
                                .collect::<Vec<_>>()
                        },
                    )
                    .with_sizing_behavior(ListSizingBehavior::Infer)
                    .track_scroll(&self.scroll),
                )
                .into_any_element()
        };

        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            .occlude()
            .bg(t.scrim())
            .backdrop_blur(px(layout::SCRIM_BLUR))
            .flex()
            .justify_center()
            // Cross-axis stretch is the flex default, and it would take the
            // card's own height with it, down to the bottom of the window.
            .items_start()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)),
            )
            .child(
                div()
                    .id("palette")
                    // Load-bearing: the arrow keys and `tab` are bound against
                    // `Palette > Input`, the only predicate deep enough to win
                    // them back from the search field.
                    .key_context("Palette")
                    .occlude()
                    // Below the titlebar rather than centred vertically: the eye
                    // is already at the top of the window, and the list grows
                    // downwards from a fixed line.
                    .mt(px(top))
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .text_size(px(layout::chrome(layout::TEXT_MD)))
                    .text_color(t.text)
                    .border_1()
                    .border_color(t.border_strong)
                    .rounded(px(layout::RADIUS_CARD))
                    .map(|card| raised_card(card, t))
                    .overflow_hidden()
                    .on_action(cx.listener(|palette, _: &PaletteNext, window, cx| {
                        palette.move_selection(1, window, cx)
                    }))
                    .on_action(cx.listener(|palette, _: &PalettePrevious, window, cx| {
                        palette.move_selection(-1, window, cx)
                    }))
                    .on_action(cx.listener(|palette, _: &PaletteNextScope, window, cx| {
                        palette.cycle_scope(1, window, cx)
                    }))
                    .on_action(
                        cx.listener(|palette, _: &PalettePreviousScope, window, cx| {
                            palette.cycle_scope(-1, window, cx)
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_MD))
                            .min_h(px(layout::chrome(layout::PALETTE_INPUT_HEIGHT)))
                            .px(px(layout::SPACE_LG))
                            .border_b_1()
                            .border_color(t.border)
                            .child(
                                icon(icon::SEARCH)
                                    .size(px(layout::chrome(layout::ICON_SIZE)))
                                    .text_color(t.text_muted),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.input)
                                        .appearance(false)
                                        .p_0()
                                        // After the input's own size, which
                                        // would set the library's text size.
                                        .text_size(px(layout::chrome(layout::TEXT_LG))),
                                ),
                            )
                            .children(
                                self.hint_chords[3]
                                    .as_deref()
                                    .map(|chord| keycaps(chord, t)),
                            ),
                    )
                    .child(self.render_scopes(t, cx))
                    .child(list)
                    .child(self.render_footer(t)),
            )
    }
}

/// A scope, as a pill: the one in force tinted with the accent, the rest plain.
fn scope_pill(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    selected: bool,
    t: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT)))
        .px(px(layout::SPACE_MD))
        .rounded_full()
        .border_1()
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .whitespace_nowrap()
        .map(|pill| {
            if selected {
                pill.bg(t.accent.alpha(layout::ACCENT_WASH))
                    .border_color(t.accent.alpha(layout::ACCENT_EDGE))
                    .text_color(t.accent)
                    .font_weight(FontWeight::MEDIUM)
            } else {
                pill.border_color(gpui::transparent_black())
                    .text_color(t.text_muted)
                    .cursor_pointer()
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(label)
}

fn footer_hint(chords: &[Option<&str>], label: &'static str, t: Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .text_size(px(layout::chrome(layout::TEXT_XS)))
        .text_color(t.text_muted)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(layout::KEYCAP_GAP))
                .children(
                    chords
                        .iter()
                        .flatten()
                        .flat_map(|chord| keycap_parts(chord))
                        .map(|key| key_chip(key, t)),
                ),
        )
        .child(label)
}

/// Everything the search palette reaches, in the order All lists its groups
/// with nothing typed: what somebody wrote first, then the database's own
/// objects, then the verbs, the other connections and the history.
fn search_items(workspace: &Workspace, profile: &Profile, cx: &App) -> Vec<Item> {
    let mut items = query_items(profile);
    items.extend(object_items(profile));
    items.extend(command_items(workspace, profile, cx));
    items.extend(connection_items(workspace));
    // Recalling a statement writes it into a buffer, which the import dialog
    // covers, for the reason `beside_import` keeps the commands it does.
    if profile.session.import_rows.is_none() {
        items.extend(history_items(profile));
    }
    items
}

/// The scratch buffer and every saved query.
fn query_items(profile: &Profile) -> Vec<Item> {
    let scratch = Item::new("Scratch query", icon::SCRATCH_QUERY, Command::OpenScratch)
        .hint("scratch")
        .scope(Scope::Queries);
    std::iter::once(scratch)
        .chain(profile.session.saved_queries.iter().map(|name| {
            Item::new(
                name.clone(),
                icon::SAVED_QUERY,
                Command::OpenQuery(name.clone()),
            )
            .hint("query")
            .scope(Scope::Queries)
        }))
        .collect()
}

/// Every relation and routine in the catalog, schema by schema.
fn object_items(profile: &Profile) -> Vec<Item> {
    let CatalogState::Loaded(catalog, _) = &profile.catalog else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for (schema_index, schema) in catalog.by_name() {
        items.extend(
            schema
                .relations
                .iter()
                .enumerate()
                .map(|(relation_index, relation)| {
                    let kind = ObjectKind::Relation(relation.kind);
                    // Qualified, so a name that appears in three schemas is three
                    // rows that can be told apart, and "pub acc" reaches one of them.
                    Item::new(
                        format!("{}.{}", schema.name, relation.name),
                        object_icon(kind),
                        Command::OpenObject(ExplorerTarget::Relation {
                            schema_index,
                            relation_index,
                        }),
                    )
                    .hint(kind_label(kind))
                    .scope(Scope::Objects)
                }),
        );
        items.extend(
            schema
                .routines
                .iter()
                .enumerate()
                .map(|(routine_index, routine)| {
                    let kind = ObjectKind::Routine(routine.kind);
                    Item::new(
                        format!("{}.{}", schema.name, routine_name(routine)),
                        object_icon(kind),
                        Command::OpenObject(ExplorerTarget::Routine {
                            schema_index,
                            routine_index,
                        }),
                    )
                    .hint(kind_label(kind))
                    .scope(Scope::Objects)
                }),
        );
    }
    items
}

/// The other connections in the current project, each where it points.
fn connection_items(workspace: &Workspace) -> Vec<Item> {
    workspace
        .profiles
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != workspace.active && workspace.in_current_group(*index))
        .map(|(index, other)| {
            Item::new(
                other.name.clone(),
                icon::DATABASE,
                Command::SwitchProfile(index),
            )
            .detail(other.config.endpoint())
            .tint(other.color)
            .scope(Scope::Connections)
        })
        .collect()
}

/// The columns of the grid in front, in their order, with their types.
fn column_items(profile: &Profile, cx: &App) -> Vec<Item> {
    let Some(results) = profile.session.active_results() else {
        return Vec::new();
    };
    results
        .read(cx)
        .delegate()
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            Item::new(
                column.name.clone(),
                icon::STRUCTURE,
                Command::RevealColumn(index),
            )
            .hint(column.data_type.clone().unwrap_or_default())
        })
        .collect()
}

/// Every statement this profile has run, newest first.
fn history_items(profile: &Profile) -> Vec<Item> {
    profile
        .session
        .history
        .iter()
        // The label is what the matcher scores as well as what the row reads
        // as, so collapsing the whitespace is also what makes `select from
        // accounts` find a statement whose `FROM` was on its own line.
        .map(|sql| {
            Item::new(
                one_line(sql),
                icon::HISTORY,
                Command::RecallStatement(sql.clone()),
            )
            .mono()
            .scope(Scope::History)
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
        .map(|name| {
            let hint = if name.as_str() == current.as_ref() {
                "current"
            } else {
                ""
            };
            Item::new(name.clone(), icon::FONT, Command::SetFont(slot, name)).hint(hint)
        })
        .collect()
}

/// Every theme, in [`Theme::all`] order, the one in force marked the way the
/// font list marks its family. The ways to import one follow under their own
/// header, so the list opens on a theme and reads as themes first.
fn theme_items(cx: &App) -> Vec<Item> {
    let current = theme(cx).name;
    let themes = Theme::all().into_iter().map(|candidate| {
        let hint = if candidate.name == current {
            "current"
        } else {
            ""
        };
        Item::new(
            candidate.name,
            icon::THEME,
            Command::SetTheme(Box::new(candidate)),
        )
        .hint(hint)
        .group("Themes")
    });
    let imports = user::Source::ALL.into_iter().map(|source| {
        Item::new(
            format!("Import from {}…", source.name()),
            icon::PLUS,
            Command::BrowseThemes(source),
        )
        .group("Import")
    });
    themes.chain(imports).collect()
}

/// Every theme one app has, without the app's name every one of them is
/// prefixed with, since the list is that app's alone. One already imported is
/// marked, since importing it again only writes the same file over itself.
fn import_items(source: user::Source) -> Vec<Item> {
    source
        .found()
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let label = candidate
                .name
                .split_once(": ")
                .map_or(candidate.name, |(_, name)| name);
            let hint = if user::is_imported(candidate.name) {
                "imported"
            } else {
                ""
            };
            Item::new(label, icon::THEME, Command::ImportTheme(source, index)).hint(hint)
        })
        .collect()
}

/// The server's databases in the order it listed them, its own after the
/// user's, the one this connection is in marked the way the theme list marks
/// its theme.
fn database_items(profile: &Profile) -> Vec<Item> {
    let databases = &profile.databases;
    let engine = profile.config.engine();
    databases
        .names
        .iter()
        .map(|name| {
            let hint = if databases.current.as_ref() == Some(name) {
                "current"
            } else if engine.is_system_database(name) {
                "system"
            } else {
                ""
            };
            Item::new(
                name.clone(),
                icon::DATABASE,
                Command::SetDatabase(name.clone()),
            )
            .hint(hint)
        })
        .collect()
}

/// Where [`schema_items`] lists the active schema, for the palette to open on.
pub(crate) fn active_schema_row(profile: &Profile) -> Option<usize> {
    schema_items(profile)
        .iter()
        .position(|item| item.hint.as_ref() == "active")
}

/// The catalog's schemas by name, the active one marked as the current
/// database is.
fn schema_items(profile: &Profile) -> Vec<Item> {
    let CatalogState::Loaded(catalog, _) = &profile.catalog else {
        return Vec::new();
    };
    let active = profile.active_schema();
    catalog
        .by_name()
        .into_iter()
        .map(|(_, schema)| {
            let hint = if active == Some(schema.name.as_str()) {
                "active"
            } else {
                ""
            };
            Item::new(
                schema.name.clone(),
                icon::PROJECT,
                Command::SetSchema(schema.name.clone()),
            )
            .hint(hint)
        })
        .collect()
}

const QUERY: &str = "Query";
const ROWS: &str = "Rows";
const CELL: &str = "Cell";
const EDITS: &str = "Edits";
const RESULTS: &str = "Results";
const TAB: &str = "Tab";
const CONNECTION: &str = "Connection";
const APPEARANCE: &str = "Appearance";
const WINDOW: &str = "Window";

/// The verbs, filtered to the ones that mean something from where the user is
/// standing. A palette that lists what it cannot do is a palette to read past.
fn command_items(workspace: &Workspace, profile: &Profile, cx: &App) -> Vec<Item> {
    let session = &profile.session;
    // Every cap here is the chord the action actually holds, so a rebind shows
    // up in the palette rather than only in settings.
    let overrides = &workspace.settings.custom_keybindings;
    let command = |label: &str, icon: &'static str, command: Command, group: &'static str| {
        Item::new(label, icon, command).group(group)
    };
    let bound = |item: Item, id: &str| item.keys(chord_for(id, overrides));
    let runnable = session.editor(session.active).is_some();
    let mut items = vec![bound(
        command("New query", icon::SCRATCH_QUERY, Command::NewQuery, QUERY),
        "new_query",
    )];

    if runnable {
        items.push(bound(
            command("Run query", icon::RUN, Command::RunQuery, QUERY),
            "run_query",
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
                    let item =
                        command(mode.label(), icon::PLAN, Command::ExplainQuery(mode), QUERY);
                    match mode {
                        ExplainMode::Plan => bound(item, "explain_query"),
                        ExplainMode::Analyze => item,
                    }
                }),
        );
        items.push(bound(
            command("Format query", icon::STRUCTURE, Command::FormatQuery, QUERY),
            "format_query",
        ));
        if let Some(tab) = session.active_query_tab()
            && tab.flips_to_plan()
        {
            items.push(match tab.showing_plan {
                true => command("Show data", icon::TABLE, Command::ShowPlan(false), QUERY),
                false => command("Show plan", icon::PLAN, Command::ShowPlan(true), QUERY),
            });
        }
        if session.open_query().is_some() {
            items.push(command(
                "Rename query…",
                icon::RENAME,
                Command::RenameQuery,
                QUERY,
            ));
        } else {
            items.push(bound(
                command("Save query…", icon::SAVE, Command::SaveQuery, QUERY),
                "rename_query_tab",
            ));
        }
    }

    // Not gated on the query tab being in front: recalling a statement brings
    // it forward, which is where the statement is going anyway.
    if !session.history.is_empty() {
        items.push(command(
            "Query history…",
            icon::HISTORY,
            Command::QueryHistory,
            QUERY,
        ));
    }

    // A query's rows that are written back to, which take a new row on every
    // engine, and a deleted one where the engine deletes them there as well
    // as on a relation's.
    if session.active_query_tab().is_some() {
        if let Some((_, table, _)) = session.query_insert_target(cx) {
            items.push(command(
                &format!("New row in {table}…"),
                icon::PLUS,
                Command::NewRow,
                ROWS,
            ));
        }
        if profile.config.engine().edits_query_rows() && workspace.has_nameable_row(cx) {
            items.push(command(
                "Delete row…",
                icon::DELETE,
                Command::DeleteRow,
                ROWS,
            ));
        }
    }

    if let Some(tab) = session.active_object() {
        if matches!(tab.body, ObjectBody::Structure { .. }) {
            items.push(bound(
                command("Show data", icon::TABLE, Command::ShowStructure(false), TAB),
                "toggle_structure",
            ));
            items.push(bound(
                command(
                    "Refresh structure",
                    icon::RUN,
                    Command::RefreshRelation(tab.id),
                    TAB,
                ),
                "refresh_relation",
            ));
        }
        if let ObjectBody::Relation {
            query,
            filter,
            limit,
            offset,
            count,
            ..
        } = &tab.body
        {
            items.push(bound(
                command(
                    "Show structure",
                    icon::STRUCTURE,
                    Command::ShowStructure(true),
                    TAB,
                ),
                "toggle_structure",
            ));
            items.push(bound(
                command(
                    "Refresh rows",
                    icon::RUN,
                    Command::RefreshRelation(tab.id),
                    ROWS,
                ),
                "refresh_relation",
            ));
            // The same gates the pager buttons stand behind: forward only off
            // a full page, backwards only off a page that is not the first.
            if matches!(
                query,
                QueryState::Complete { rows, .. } if limit.is_some_and(|limit| *rows >= limit)
            ) {
                items.push(command(
                    "Next page",
                    icon::CHEVRON_RIGHT,
                    Command::NextPage,
                    ROWS,
                ));
            }
            if *offset > 0 {
                items.push(command(
                    "Previous page",
                    icon::CHEVRON_LEFT,
                    Command::PreviousPage,
                    ROWS,
                ));
            }
            if !count.answers(filter) {
                items.push(command(
                    "Count rows",
                    icon::TABLE,
                    Command::CountRows(tab.id),
                    ROWS,
                ));
            }
            items.push(command(
                "Filter rows…",
                icon::SEARCH,
                Command::FilterRows,
                ROWS,
            ));
            // A row that would do nothing is worse than no row at all.
            if !filter.is_empty() {
                items.push(command(
                    "Clear filter",
                    icon::CLOSE,
                    Command::ClearFilter,
                    ROWS,
                ));
            }
            if tab.takes_inserts(profile.config.engine()) {
                items.push(command("New row…", icon::PLUS, Command::NewRow, ROWS));
            }
            if tab.takes_imports(profile.config.engine()) {
                items.push(command(
                    &format!("Import Rows into {}…", tab.name),
                    icon::PLUS,
                    Command::ImportRows,
                    ROWS,
                ));
            }
            // Only on a row dbdelve can name by its primary key -- the same
            // condition that makes a cell of it editable.
            if workspace.has_nameable_row(cx) {
                items.push(command(
                    "Delete row…",
                    icon::DELETE,
                    Command::DeleteRow,
                    ROWS,
                ));
            }
        }
        items.push(bound(
            command("Close tab", icon::CLOSE, Command::CloseObject(tab.id), TAB),
            "close_tab",
        ));
    }

    // Only over a grid that has a result set behind it. A surface that has run
    // nothing has nothing to write out.
    if workspace.has_results(cx) {
        items.push(bound(
            command(
                "Go to column…",
                icon::STRUCTURE,
                Command::GoToColumn,
                RESULTS,
            ),
            "go_to_column",
        ));
        items.push(command(
            "Export results as CSV…",
            icon::SAVE,
            Command::ExportResults(Format::Csv),
            RESULTS,
        ));
        items.push(command(
            "Export results as JSON…",
            icon::SAVE,
            Command::ExportResults(Format::Json),
            RESULTS,
        ));
        items.push(bound(
            command(
                "Copy results as TSV",
                icon::COPY,
                Command::CopyResults(Format::Tsv),
                RESULTS,
            ),
            "copy_results",
        ));
        items.push(command(
            "Copy results as CSV",
            icon::COPY,
            Command::CopyResults(Format::Csv),
            RESULTS,
        ));
    }

    if workspace.has_active_cell(cx) {
        items.push(bound(
            command("Copy row", icon::COPY, Command::CopyRow, RESULTS),
            "copy_row",
        ));
    }

    // Only where the ring is on a cell that can actually take one.
    if workspace.has_editable_cell(cx) {
        items.push(bound(
            command("Set cell to NULL", icon::RENAME, Command::SetNull, CELL),
            "set_null",
        ));
        items.push(command(
            "Set cell to empty",
            icon::RENAME,
            Command::SetEmpty,
            CELL,
        ));
        // Offered on every editable cell rather than only where a default is
        // known. The palette is reached from anywhere and knows no column; the
        // cell menu is the surface that hides what would be refused.
        items.push(command(
            "Set cell to default",
            icon::RENAME,
            Command::SetDefault,
            CELL,
        ));
    }

    // Only while there is something to write back, for the reason the footer's
    // pair of buttons is conditional too.
    if workspace.has_pending_edits(cx) {
        items.push(command(
            "Apply edits",
            icon::SAVE,
            Command::ApplyEdits,
            EDITS,
        ));
        items.push(command(
            "Discard edits",
            icon::DELETE,
            Command::DiscardEdits,
            EDITS,
        ));
        items.push(command(
            "Next edit",
            icon::CHEVRON_RIGHT,
            Command::NextEdit,
            EDITS,
        ));
        items.push(command(
            "Previous edit",
            icon::CHEVRON_LEFT,
            Command::PreviousEdit,
            EDITS,
        ));
    }

    if workspace.current_group_members().len() > 1 {
        items.push(bound(
            command(
                "Next connection",
                icon::DATABASE,
                Command::NextProfile,
                CONNECTION,
            ),
            "next_profile",
        ));
        items.push(bound(
            command(
                "Previous connection",
                icon::DATABASE,
                Command::PreviousProfile,
                CONNECTION,
            ),
            "previous_profile",
        ));
    }

    items.push(bound(
        command(
            "New connection…",
            icon::PLUS,
            Command::NewConnection,
            CONNECTION,
        ),
        "new_connection",
    ));
    items.push(bound(
        command(
            "New project…",
            icon::ADD_TO_PROJECT,
            Command::NewProject,
            CONNECTION,
        ),
        "new_project",
    ));
    if Source::DBeaver.found() {
        items.push(bound(
            command(
                "Import connections from DBeaver…",
                icon::PLUS,
                Command::ImportConnections(Source::DBeaver),
                CONNECTION,
            ),
            "import_dbeaver",
        ));
    }
    if Source::TablePlus.found() {
        items.push(bound(
            command(
                "Import connections from TablePlus…",
                icon::PLUS,
                Command::ImportConnections(Source::TablePlus),
                CONNECTION,
            ),
            "import_tableplus",
        ));
    }
    items.push(bound(
        command(
            "Refresh connection",
            icon::DATABASE,
            Command::RefreshConnection,
            CONNECTION,
        ),
        "refresh_connection",
    ));
    if profile.config.engine().switches_database() {
        items.push(bound(
            command(
                "Select database…",
                icon::DATABASE,
                Command::SelectDatabase,
                CONNECTION,
            ),
            "select_database",
        ));
    }
    if profile.config.engine().picks_schema() {
        items.push(command(
            "Select schema…",
            icon::PROJECT,
            Command::SelectSchema,
            CONNECTION,
        ));
    }
    items.push(bound(
        command(
            "Select theme…",
            icon::THEME,
            Command::SelectTheme,
            APPEARANCE,
        ),
        "cycle_theme",
    ));
    for (label, slot) in [
        ("Chrome font…", FontSlot::Chrome),
        ("Editor font…", FontSlot::Editor),
        ("Grid font…", FontSlot::Grid),
    ] {
        items.push(command(
            label,
            icon::FONT,
            Command::PickFont(slot),
            APPEARANCE,
        ));
    }
    items.push(bound(
        command("Settings", icon::SETTINGS, Command::OpenSettings, WINDOW),
        "open_settings",
    ));
    // One row rather than a Show/Hide pair: the palette is built from the
    // session, which does not know whether the column is folded.
    items.push(bound(
        command(
            "Toggle sidebar",
            icon::SIDEBAR,
            Command::ToggleSidebar,
            WINDOW,
        ),
        "toggle_sidebar",
    ));
    items.push(bound(
        command(
            "Toggle row panel",
            icon::ROW_PANEL,
            Command::ToggleRowPanel,
            WINDOW,
        ),
        "toggle_row_panel",
    ));
    items.push(bound(
        command(
            "Toggle row panel edit mode",
            icon::EDIT,
            Command::ToggleRowEdit,
            WINDOW,
        ),
        "toggle_row_edit",
    ));
    if matches!(session.active, Tab::Query(_)) {
        items.push(bound(
            command(
                "Reset editor zoom",
                icon::SEARCH,
                Command::ResetEditorZoom,
                WINDOW,
            ),
            "reset_editor_zoom",
        ));
    }
    if session.import_rows.is_some() {
        items.retain(|item| beside_import(&item.command));
    }
    items
}

/// Whether `command` acts on the app or a connection rather than the tab the
/// import dialog covers, where it would act out of sight or open under it.
fn beside_import(command: &Command) -> bool {
    matches!(
        command,
        Command::SwitchProfile(_)
            | Command::NextProfile
            | Command::PreviousProfile
            | Command::NewConnection
            | Command::NewProject
            | Command::ImportConnections(_)
            | Command::RefreshConnection
            | Command::SelectTheme
            | Command::PickFont(_)
            | Command::OpenSettings
            | Command::ToggleSidebar
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, scope: Scope, group: &'static str) -> Item {
        Item::new(label, icon::TABLE, Command::NewQuery)
            .scope(scope)
            .group(group)
    }

    fn labels(items: &[Item], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Header(name) => format!("# {name}"),
                Row::Item(index) => items[*index].label.clone(),
            })
            .collect()
    }

    fn arranged(
        items: &[Item],
        scope: Option<Scope>,
        query: &str,
        recents: &[&str],
    ) -> Vec<String> {
        let recents: Vec<(Scope, String)> = recents
            .iter()
            .map(|label| (Scope::Commands, label.to_string()))
            .collect();
        let rows = arrange(
            items,
            scope,
            &Query::new(query),
            &mut Fuzzy::new(),
            &recents,
        );
        labels(items, &rows)
    }

    fn sample() -> Vec<Item> {
        vec![
            item("Scratch query", Scope::Queries, ""),
            item("public.accounts", Scope::Objects, ""),
            item("public.events", Scope::Objects, ""),
            item("analytics.events", Scope::Objects, ""),
            item("Run query", Scope::Commands, QUERY),
            item("Format query", Scope::Commands, QUERY),
            item("Close tab", Scope::Commands, TAB),
            item("Toggle sidebar", Scope::Commands, WINDOW),
            item("staging", Scope::Connections, ""),
        ]
    }

    #[test]
    fn with_nothing_typed_a_scope_lists_its_groups_in_the_order_they_were_built() {
        assert_eq!(
            arranged(&sample(), Some(Scope::Commands), "", &[]),
            [
                "# Query",
                "Run query",
                "Format query",
                "# Tab",
                "Close tab",
                "# Window",
                "Toggle sidebar"
            ]
        );
    }

    #[test]
    fn a_scope_keeps_only_its_own_rows() {
        assert_eq!(
            arranged(&sample(), Some(Scope::Objects), "events", &[]),
            ["public.events", "analytics.events"]
        );
    }

    #[test]
    fn all_groups_by_scope_and_puts_the_group_holding_the_best_match_first() {
        assert_eq!(
            arranged(&sample(), Some(Scope::All), "query", &[]),
            [
                "# Commands",
                "Run query",
                "Format query",
                "# Queries",
                "Scratch query"
            ]
        );
        assert_eq!(
            arranged(&sample(), Some(Scope::All), "acc", &[]),
            ["# Objects", "public.accounts"]
        );
    }

    #[test]
    fn all_cuts_each_group_to_its_best_few() {
        let items: Vec<Item> = (0..12)
            .map(|n| item(&format!("public.table_{n}"), Scope::Objects, ""))
            .collect();
        let rows = arranged(&items, Some(Scope::All), "table", &[]);
        assert_eq!(rows.len(), 1 + ALL_SCOPE_GROUP_ROWS);
        assert_eq!(
            arranged(&items, Some(Scope::Objects), "table", &[]).len(),
            12
        );
    }

    #[test]
    fn rows_run_last_lead_under_recent_until_something_is_typed() {
        let items = sample();
        assert_eq!(
            arranged(&items, Some(Scope::Commands), "", &["Close tab", "gone"]),
            [
                "# Recent",
                "Close tab",
                "# Query",
                "Run query",
                "Format query",
                "# Window",
                "Toggle sidebar"
            ]
        );
        assert_eq!(
            arranged(&items, Some(Scope::Commands), "tab", &["Close tab"]),
            ["# Tab", "Close tab"]
        );
    }

    #[test]
    fn a_recent_row_comes_back_from_the_scope_it_was_run_in() {
        let items = [
            item("New query", Scope::Queries, ""),
            item("New query", Scope::Commands, QUERY),
        ];
        let recents = [(Scope::Commands, "New query".to_string())];
        let rows = arrange(
            &items,
            Some(Scope::All),
            &Query::new(""),
            &mut Fuzzy::new(),
            &recents,
        );
        assert_eq!(rows[..2], [Row::Header(RECENT_GROUP), Row::Item(1)]);
    }

    #[test]
    fn cmd_p_opens_on_objects_and_closes_from_there() {
        let objects = Mode::Search(Scope::Objects);
        let commands = Mode::Search(Scope::Commands);
        let all = Mode::Search(Scope::All);
        assert_eq!(stroke(objects, objects, objects), Stroke::Close);
        assert_eq!(stroke(objects, all, objects), Stroke::Close);
        assert_eq!(stroke(commands, objects, objects), Stroke::Close);
        assert_eq!(
            stroke(commands, all, objects),
            Stroke::Rescope(Scope::Objects)
        );
        assert_eq!(
            stroke(objects, objects, commands),
            Stroke::Rescope(Scope::Commands)
        );
    }

    #[test]
    fn recent_objects_lead_the_objects_scope_until_something_is_typed() {
        let items = sample();
        let recents = [(Scope::Objects, "public.events".to_string())];
        let rows = arrange(
            &items,
            Some(Scope::Objects),
            &Query::new(""),
            &mut Fuzzy::new(),
            &recents,
        );
        assert_eq!(
            labels(&items, &rows),
            [
                "# Recent",
                "public.events",
                "public.accounts",
                "analytics.events"
            ]
        );
    }

    #[test]
    fn the_stroke_that_opened_the_palette_closes_it_from_any_scope() {
        let all = Mode::Search(Scope::All);
        let commands = Mode::Search(Scope::Commands);
        let objects = Mode::Search(Scope::Objects);
        assert_eq!(stroke(all, all, all), Stroke::Close);
        assert_eq!(stroke(all, objects, all), Stroke::Close);
        assert_eq!(
            stroke(all, objects, commands),
            Stroke::Rescope(Scope::Commands)
        );
        assert_eq!(stroke(all, commands, commands), Stroke::Close);
        assert_eq!(stroke(commands, all, commands), Stroke::Close);
        assert_eq!(stroke(all, all, commands), Stroke::Rescope(Scope::Commands));
        assert_eq!(stroke(Mode::Theme, Mode::Theme, all), Stroke::Replace);
        assert_eq!(stroke(all, objects, Mode::Theme), Stroke::Replace);
        assert_eq!(stroke(Mode::Theme, Mode::Theme, Mode::Theme), Stroke::Close);
    }

    #[test]
    fn the_selection_skips_headers_and_wraps_both_ways() {
        let rows = [
            Row::Header("A"),
            Row::Item(0),
            Row::Item(1),
            Row::Header("B"),
            Row::Item(2),
        ];
        assert_eq!(first_item(&rows), Some(1));
        assert_eq!(step(&rows, Some(1), 1), Some(2));
        assert_eq!(step(&rows, Some(2), 1), Some(4));
        assert_eq!(step(&rows, Some(4), 1), Some(1));
        assert_eq!(step(&rows, Some(1), -1), Some(4));
        assert_eq!(step(&rows, None, -1), Some(4));
        assert_eq!(step(&[Row::Header("A")], None, 1), None);
    }

    #[test]
    fn the_scopes_cycle_with_tab_and_wrap() {
        assert_eq!(Scope::All.step(1), Scope::Commands);
        assert_eq!(Scope::All.step(-1), Scope::Connections);
        assert_eq!(Scope::Connections.step(1), Scope::All);
    }

    #[test]
    fn a_statement_written_across_lines_reads_and_matches_as_one() {
        let sql = "SELECT *\n  FROM accounts\n WHERE id = 1;";
        assert_eq!(one_line(sql), "SELECT * FROM accounts WHERE id = 1;");
        let items = [item(&one_line(sql), Scope::History, "")];
        assert_eq!(
            arranged(&items, Some(Scope::History), "from accounts", &[]).len(),
            1
        );
    }

    #[test]
    fn the_filter_rows_are_found_by_the_word_the_user_would_type() {
        // A palette row nobody can find by typing the obvious word is a row
        // that is not in the palette.
        let items = [
            item("Filter rows…", Scope::Commands, ROWS),
            item("Clear filter", Scope::Commands, ROWS),
            item("Next page", Scope::Commands, ROWS),
        ];
        assert_eq!(
            arranged(&items, Some(Scope::Commands), "filter", &[]),
            ["# Rows", "Filter rows…", "Clear filter"]
        );
    }

    #[test]
    fn a_schema_prefix_narrows_to_one_schema() {
        assert_eq!(
            arranged(&sample(), Some(Scope::Objects), "pub ev", &[]),
            ["public.events"]
        );
    }
}

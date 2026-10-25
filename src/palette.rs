//! `cmd+p` and `cmd+shift+p`: one palette, one matcher, several scopes.
//!
//! The palette searches everything the active profile can reach -- its verbs,
//! its tables and routines, its saved queries, the statements it has run and
//! the other connections -- and the scope pills narrow it to one of those.
//! `cmd+p` opens on Objects, `cmd+shift+p` on Commands, and the titlebar's
//! connection name on Connections, a tree of every project and the
//! connections in it; `tab` moves between scopes, and so do `left` and
//! `right` once the caret is at the field's edge. The pickers a command opens (themes, fonts, databases,
//! schemas, columns, projects) are the same card over a list of their own,
//! and so is what can be done to one connection or one project, which
//! `shift-enter` puts up from its row.
//!
//! The command scope offers only the verbs that apply to what is on screen: a
//! Run on a tab with no buffer is a row to read past.
//!
//! Neither surface decides anything. A row carries a [`Command`], the workspace
//! runs it through the same methods the buttons and keystrokes call, and the
//! palette is gone by the time it happens.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement, IntoElement, ListSizingBehavior, MouseButton, ParentElement,
    Pixels, Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Subscription,
    WeakEntity, Window, div, prelude::FluentBuilder, px, size,
};
use gpui_component::{
    VirtualListScrollHandle,
    input::{InputEvent, InputState},
    v_virtual_list,
};

use crate::{
    Workspace,
    actions::{
        PaletteNext, PaletteNextScope, PalettePrevious, PalettePreviousScope, PaletteRowActions,
        PaletteScopeLeft, PaletteScopeRight,
    },
    db::{ConnectionConfig, Engine, ExplainMode, RelationKind, RoutineKind},
    explorer::{ExplorerTarget, ObjectKind},
    export::Format,
    fuzzy::{Fuzzy, Query},
    icons::icon,
    import::Source,
    session::{CatalogState, ObjectBody, Profile, QueryState, Tab, routine_name},
    store::StoredProject,
    theme::{
        Appearance, ConnectionColor, FontSlot, Theme, fonts, install_theme, layout, popup_theme,
        theme, user,
    },
    ui::{
        Control, Tone, chord_for, highlighted, icon_button, key_chip, keycap_parts, keycaps,
        kind_color, object_icon, one_line, raised_card, row_icon_tinted, search_field, status_dot,
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
/// The hover group of every row, for its actions button to show by.
const ROW_GROUP: &str = "palette-row";

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
    /// theme is installed as it moves, and only a confirmed one is kept. With
    /// a side, only that side's themes, for the theme worn while the system
    /// is light or dark.
    Theme(Option<Appearance>),
    /// Every theme one app has on this machine, or one side's, previewed as
    /// the theme list previews, and imported when confirmed.
    ImportTheme(user::Source, Option<Appearance>),
    /// The databases on the active profile's server, as last fetched.
    Database,
    /// The schemas in the active profile's catalog, to pick the active one.
    Schema,
    /// The columns of the grid in front, to jump to one of them.
    Columns,
    /// Every engine, to pick the connection form's.
    Engine,
    /// What can be done to one connection, by index into the profiles, and to
    /// the project it is in.
    ConnectionActions(usize),
    /// What can be done to one project, by index into the projects.
    ProjectActions(usize),
    /// The projects one connection can move into, and No project.
    MoveToProject(usize),
    /// The field takes a project's name, and the one row confirms it: a new
    /// project, or the one at this index renamed.
    NameProject(Option<usize>),
    /// Every project, to rename or delete one.
    Projects(ProjectVerb),
    /// The one row that removes a connection or deletes a project, so a
    /// keystroke aimed at something else never does.
    Confirm(Target),
}

/// What a confirmation is about, by index into the profiles or the projects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Connection(usize),
    Project(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProjectVerb {
    Rename,
    Delete,
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
            Mode::Theme(None) => "Pick a theme…",
            Mode::Theme(Some(Appearance::Light)) => "Pick a light theme…",
            Mode::Theme(Some(Appearance::Dark)) => "Pick a dark theme…",
            Mode::ImportTheme(..) => "Import a theme…",
            Mode::Database => "Switch database…",
            Mode::Schema => "Switch schema…",
            Mode::Columns => "Go to a column…",
            Mode::Engine => "Pick an engine…",
            Mode::ConnectionActions(_) | Mode::ProjectActions(_) => "Pick an action…",
            Mode::MoveToProject(_) => "Move to a project…",
            Mode::NameProject(None) => "Name the new project…",
            Mode::NameProject(Some(_)) => "Rename the project…",
            Mode::Projects(ProjectVerb::Rename) => "Rename a project…",
            Mode::Projects(ProjectVerb::Delete) => "Delete a project…",
            Mode::Confirm(Target::Connection(_)) => "Remove this connection?",
            Mode::Confirm(Target::Project(_)) => "Delete this project?",
        }
    }

    /// The one pill a picker shows in place of the scopes, naming its list.
    fn title(self) -> &'static str {
        match self {
            Mode::Search(scope) => scope.title(),
            Mode::Font(FontSlot::Chrome) => "Chrome font",
            Mode::Font(FontSlot::Editor) => "Editor font",
            Mode::Font(FontSlot::Grid) => "Grid font",
            Mode::Theme(None) => "Themes",
            Mode::Theme(Some(side)) => side.theme_label(),
            Mode::ImportTheme(..) => "Import theme",
            Mode::Database => "Databases",
            Mode::Schema => "Schemas",
            Mode::Columns => "Columns",
            Mode::Engine => "Engines",
            Mode::ConnectionActions(_) => "Connection",
            Mode::ProjectActions(_) => "Project",
            Mode::MoveToProject(_) => "Move to project",
            Mode::NameProject(None) => "New project",
            Mode::NameProject(Some(_)) | Mode::Projects(ProjectVerb::Rename) => "Rename project",
            Mode::Projects(ProjectVerb::Delete) | Mode::Confirm(Target::Project(_)) => {
                "Delete project"
            }
            Mode::Confirm(Target::Connection(_)) => "Remove connection",
        }
    }

    /// What `enter` does to the row, for the footer.
    fn verb(self) -> &'static str {
        match self {
            Mode::Search(Scope::Commands)
            | Mode::ConnectionActions(_)
            | Mode::ProjectActions(_) => "Run",
            Mode::Search(Scope::Connections) => "Switch",
            Mode::Search(_) | Mode::Columns => "Open",
            Mode::MoveToProject(_) => "Move",
            Mode::NameProject(None) => "Create",
            Mode::NameProject(Some(_)) => "Rename",
            Mode::Confirm(Target::Connection(_)) => "Remove",
            Mode::Confirm(Target::Project(_)) => "Delete",
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
    SetEngine(Engine),
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
    /// Open the "Insert" form over the preview. Insertion needs no primary
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
    /// Narrow the palette to the Connections scope.
    SwitchConnection,
    /// Put the palette back up over what can be done to this connection.
    ConnectionActions(usize),
    /// Fold or unfold a project's connections in the Connections scope. The
    /// one row the palette stays up for, since it changes only the list.
    ToggleProject(usize),
    /// Put the palette back up over what can be done to this project.
    ProjectActions(usize),
    EditConnection(usize),
    DuplicateConnection(usize),
    /// Put the palette back up over the projects this connection can join.
    ChooseProject(usize),
    /// `None` is No project.
    MoveToProject(usize, Option<String>),
    /// Put the palette back up over the one row that does it.
    Confirm(Target),
    RemoveConnection(usize),
    NextProfile,
    PreviousProfile,
    NewConnection,
    /// A new connection that joins this project, or none.
    NewConnectionIn(Option<String>),
    NewProject,
    /// Put the palette back up over the projects, to rename or delete one.
    PickProject(ProjectVerb),
    /// Put the palette back up to take a project's name.
    NameProject(Option<usize>),
    CreateProject(String),
    /// From the first name to the second.
    RenameProject(String, String),
    DeleteProject(String),
    ImportConnections(Source),
    RefreshConnection,
    /// Put the palette back up over the theme list.
    SelectTheme,
    /// For the side a light or dark theme list was opened for, if any.
    SetTheme(Option<Appearance>, Box<Theme>),
    /// Put the palette back up over the themes an app has, to import one.
    BrowseThemes(user::Source, Option<Appearance>),
    /// By index into [`user::Source::found`], which stands for the process.
    ImportTheme(user::Source, usize, Option<Appearance>),
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
    /// What else the matcher scores, each on its own, for a row also found by
    /// what is drawn beside it: a connection by its host, which its detail
    /// shows. Built once per open.
    also: Vec<String>,
    /// Muted and monospaced, inline after the label: where a connection points.
    detail: Option<SharedString>,
    /// Muted, at the far end: what kind of object this is, or which row is the
    /// one in force.
    hint: SharedString,
    /// The chord the action holds, drawn as a keycap per key.
    keys: Option<String>,
    icon: &'static str,
    tint: Option<ConnectionColor>,
    /// The profile, by index, whose state the row draws as the dot the status
    /// bar draws for it. Read as the row is drawn, so a connection that comes
    /// up while the palette is open says so.
    status: Option<usize>,
    /// A statement, drawn in the editor's face.
    mono: bool,
    scope: Scope,
    /// Listed under All as well as in its own scope.
    in_all: bool,
    /// The header the row sits under.
    group: Group,
    command: Command,
}

/// Which group a row is in. Told apart by what the group is, never by what its
/// header reads, so a project called "Manage" is not the Manage group.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    /// The list's one group, which needs no header of its own -- until another
    /// group stands beside it, when it is headed by its scope.
    Bare,
    /// A header the palette names itself.
    Named(&'static str),
    /// A project, by its index, under its name.
    Project(usize, SharedString),
}

impl Item {
    fn new(label: impl Into<String>, icon: &'static str, command: Command) -> Self {
        Self {
            label: label.into(),
            also: Vec::new(),
            detail: None,
            hint: SharedString::default(),
            keys: None,
            icon,
            tint: None,
            status: None,
            mono: false,
            scope: Scope::Commands,
            in_all: true,
            group: Group::Bare,
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

    fn group(mut self, group: Group) -> Self {
        self.group = group;
        self
    }

    fn also(mut self, also: Vec<String>) -> Self {
        self.also = also;
        self
    }

    fn status(mut self, profile: usize) -> Self {
        self.status = Some(profile);
        self
    }

    fn beyond_all(mut self) -> Self {
        self.in_all = false;
        self
    }
}

/// One line of the list: a group's header, or a row by its index into the
/// palette's items.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Header(SharedString),
    Item(usize),
}

/// The list's rows, and the one to select as it opens.
struct Arranged {
    rows: Vec<Row>,
    lead: Option<usize>,
}

/// The rows to draw for `query` over `items`, in order.
///
/// Rows are ranked by the matcher, then gathered under their groups, and the
/// groups are ordered by their best row: the first row is always the best
/// match, and a group's rows stay together beneath it. Under All a group is a
/// scope, cut to its best few. With nothing typed, the rows confirmed most
/// recently lead under Recent, and are not repeated further down -- except
/// where the list is already in an order of its own: History is newest first,
/// and Connections is a tree of projects ([`tree`]).
///
/// A row is scored on its label and on each of its other texts apart, the
/// best of them counting, so a match never runs from one into the next.
fn arrange(
    items: &[Item],
    scope: Option<Scope>,
    query: &Query,
    fuzzy: &mut Fuzzy,
    recents: &[(Scope, String)],
    collapsed: &HashSet<String>,
) -> Arranged {
    if scope == Some(Scope::Connections) {
        return tree(items, query, fuzzy, collapsed);
    }
    let ranked = fuzzy.rank(
        query,
        items.iter().enumerate().filter(|(_, item)| match scope {
            None | Some(Scope::All) => item.in_all,
            Some(scope) => item.scope == scope,
        }),
        |(_, item)| &item.label,
        |(_, item)| &item.also,
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

    let mut groups: Vec<(Group, Vec<usize>)> = Vec::new();
    if !recent.is_empty() {
        groups.push((Group::Named(RECENT_GROUP), recent.clone()));
    }
    for ((index, item), _) in ranked {
        if recent.contains(&index) {
            continue;
        }
        let group = match scope {
            Some(Scope::All) => Group::Named(item.scope.title()),
            _ => item.group.clone(),
        };
        match groups.iter_mut().find(|(key, _)| *key == group) {
            Some((_, members)) => members.push(index),
            None => groups.push((group, vec![index])),
        }
    }

    // Rows with no header under another group's would read as that group's.
    let alone = groups.len() == 1;
    let mut rows = Vec::new();
    for (group, mut members) in groups {
        if scope == Some(Scope::All) {
            members.truncate(ALL_SCOPE_GROUP_ROWS);
        }
        let header = match group {
            Group::Bare if alone => None,
            Group::Bare => scope.map(|scope| scope.title().into()),
            Group::Named(name) => Some(name.into()),
            Group::Project(_, name) => Some(name),
        };
        rows.extend(header.map(Row::Header));
        rows.extend(members.into_iter().map(Row::Item));
    }
    let lead = first_item(&rows);
    Arranged { rows, lead }
}

/// Where a row of the Connections scope stands in its tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    /// A connection in no project.
    Loose,
    /// A project's own row, or a row beneath it.
    Project(usize),
    Manage,
}

fn branch(item: &Item) -> Branch {
    match item.group {
        Group::Project(at, _) => Branch::Project(at),
        Group::Named(MANAGE) => Branch::Manage,
        _ => Branch::Loose,
    }
}

/// The Connections scope as a tree: the connections in no project, then each
/// project's own row with its connections beneath it, then the ways to add
/// one or a project, which stay at the bottom whatever is typed.
///
/// With nothing typed, a folded project shows its row alone. With something
/// typed nothing is folded, so a match is never hidden: a connection that
/// matches comes with its project's row above it, and a project that matches
/// with every connection it holds. The project holding the best match comes
/// first, and the loose connections go above the projects or below them by
/// the same rule. The best match is where the list opens, which is not its
/// first row when a project's row stands above it as context.
fn tree(items: &[Item], query: &Query, fuzzy: &mut Fuzzy, collapsed: &HashSet<String>) -> Arranged {
    let ranked: Vec<usize> = fuzzy
        .rank(
            query,
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.scope == Scope::Connections),
            |(_, item)| &item.label,
            |(_, item)| &item.also,
        )
        .into_iter()
        .map(|((index, _), _)| index)
        .collect();
    let mut rank: Vec<Option<usize>> = vec![None; items.len()];
    for (at, &index) in ranked.iter().enumerate() {
        rank[index] = Some(at);
    }
    let searching = !query.is_empty();
    let in_branch = |wanted: Branch| -> Vec<usize> {
        ranked
            .iter()
            .copied()
            .filter(|&index| branch(&items[index]) == wanted)
            .collect()
    };
    let loose = in_branch(Branch::Loose);
    let manage = in_branch(Branch::Manage);

    // Each project as its row and the rows beneath it, with the rank of the
    // best of them.
    let mut projects: Vec<(usize, Vec<usize>)> = Vec::new();
    for (head, item) in items.iter().enumerate() {
        let Command::ToggleProject(at) = item.command else {
            continue;
        };
        let members = (0..items.len())
            .filter(|&index| index != head && branch(&items[index]) == Branch::Project(at));
        let (best, beneath): (Option<usize>, Vec<usize>) = if !searching {
            let open = !collapsed.contains(&item.label);
            (Some(head), members.filter(|_| open).collect())
        } else if let Some(hit) = rank[head] {
            let beneath: Vec<usize> = members.collect();
            let best = beneath.iter().filter_map(|&index| rank[index]).min();
            (Some(best.map_or(hit, |best| best.min(hit))), beneath)
        } else {
            let mut hits: Vec<usize> = members.filter(|&index| rank[index].is_some()).collect();
            hits.sort_by_key(|&index| rank[index]);
            (hits.first().and_then(|&index| rank[index]), hits)
        };
        if let Some(best) = best {
            projects.push((best, std::iter::once(head).chain(beneath).collect()));
        }
    }
    projects.sort_by_key(|(best, _)| *best);

    let loose_header = loose.first().and_then(|&index| match items[index].group {
        Group::Named(name) => Some(SharedString::from(name)),
        _ if !projects.is_empty() || !manage.is_empty() => Some(Scope::Connections.title().into()),
        _ => None,
    });
    let loose_rows = loose_header
        .map(Row::Header)
        .into_iter()
        .chain(loose.iter().map(|&index| Row::Item(index)));
    let project_rows = (!projects.is_empty())
        .then(|| Row::Header(PROJECTS.into()))
        .into_iter()
        .chain(
            projects
                .iter()
                .flat_map(|(_, rows)| rows.iter().map(|&index| Row::Item(index))),
        );
    let loose_first = match (loose.first(), projects.first()) {
        (Some(&index), Some((best, _))) => !searching || rank[index] < Some(*best),
        _ => true,
    };
    let mut rows: Vec<Row> = if loose_first {
        loose_rows.chain(project_rows).collect()
    } else {
        project_rows.chain(loose_rows).collect()
    };
    if !manage.is_empty() {
        rows.push(Row::Header(MANAGE.into()));
        rows.extend(manage.into_iter().map(Row::Item));
    }
    let lead = match ranked.first() {
        Some(&best) if searching => rows.iter().position(|row| *row == Row::Item(best)),
        _ => first_item(&rows),
    };
    Arranged { rows, lead }
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

/// Where the selection starts: on the list's lead, except over the one row
/// that removes something, which opens with nothing selected so that an
/// `enter` held down, or pressed once too often, to get there does not also
/// confirm it. `down` or a click selects it.
fn opening_selection(mode: Mode, lead: Option<usize>) -> Option<usize> {
    match mode {
        Mode::Confirm(_) => None,
        _ => lead,
    }
}

/// Whether a confirmed row leads the next palette under Recent. Never one
/// that removes or deletes, which would sit selected there an `enter` or two
/// from doing it again. Nor a connection: Recent lists one only under All and
/// only from the project in front, so most would push out rows it does show.
pub fn remembered(scope: Scope, command: &Command) -> bool {
    scope != Scope::Connections
        && !matches!(
            command,
            Command::Confirm(_)
                | Command::PickProject(ProjectVerb::Delete)
                | Command::RemoveConnection(_)
                | Command::DeleteProject(_)
        )
}

/// Whether a row is left out while an import asks which project its
/// connections join: every connection's row, and whatever names, moves or
/// removes a project or a connection, which would change the answer under it
/// or take away the project it names.
pub fn waits_for_import(scope: Scope, command: &Command) -> bool {
    scope == Scope::Connections
        || matches!(
            command,
            Command::SwitchConnection
                | Command::ConnectionActions(_)
                | Command::ProjectActions(_)
                | Command::ChooseProject(_)
                | Command::MoveToProject(..)
                | Command::Confirm(_)
                | Command::RemoveConnection(_)
                | Command::NewConnectionIn(_)
                | Command::NewProject
                | Command::PickProject(_)
                | Command::NameProject(_)
                | Command::CreateProject(_)
                | Command::RenameProject(..)
                | Command::DeleteProject(_)
        )
}

/// What `left` or `right` does in the search field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arrow {
    /// Moves the caret, or collapses the selection, as it would in any field.
    Caret,
    /// Steps to the neighbouring scope, as `shift-tab` or `tab` would.
    Scope,
}

/// Only a bare caret at the edge the key points past switches scope, so
/// editing what is typed never jumps the list: an empty field is at both.
fn arrow(selection: Range<usize>, len: usize, delta: isize) -> Arrow {
    let edge = if delta < 0 { 0 } else { len };
    if selection.is_empty() && selection.start == edge {
        Arrow::Scope
    } else {
        Arrow::Caret
    }
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
    /// A row's secondary action was asked for. Not a confirmation, so it
    /// leads nothing under Recent.
    Act {
        command: Command,
        scope: Scope,
    },
    Dismiss,
}

/// What the name field of [`Mode::NameProject`] is checked against.
struct Naming {
    /// The project's name as it was, on a rename.
    renaming: Option<String>,
    taken: Vec<String>,
    /// Why what is typed cannot be the name, drawn where the row would be.
    refusal: Option<String>,
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
    /// The projects the Connections scope shows folded, by name, as the
    /// workspace keeps them.
    collapsed: HashSet<String>,
    /// The chords drawn in the footer and the field, read from the keymap so a
    /// rebind shows up here too: previous row, next row, scope left, scope
    /// right, close, row actions.
    hint_chords: [Option<String>; 6],
    naming: Option<Naming>,
    /// The mode the palette opened in, for [`stroke`].
    opened: Mode,
    /// Where each connection row reads its state from as it is drawn.
    workspace: WeakEntity<Workspace>,
    /// Whether the catalog was still on its way the last time it was read, so
    /// an empty Objects scope can say it is coming rather than that there is
    /// nothing.
    catalog_loading: bool,
    /// Whether an import was asking where its connections go as the palette
    /// opened, which leaves the Connections scope empty.
    import_pending: bool,
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
        handle: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut items = match (mode, workspace.profile()) {
            (Mode::Theme(side), _) => theme_items(side, picked_theme(workspace, side, cx)),
            (Mode::ImportTheme(source, side), _) => import_items(source, side),
            (Mode::ConnectionActions(index), _) => connection_action_items(workspace, index),
            (Mode::ProjectActions(at), _) => workspace
                .projects
                .get(at)
                .map(|project| project_action_items(at, &project.name, Group::Bare))
                .unwrap_or_default(),
            (Mode::MoveToProject(index), _) => workspace
                .profiles
                .get(index)
                .map(|profile| move_items(index, &profile.id, &workspace.projects))
                .unwrap_or_default(),
            (Mode::Projects(verb), _) => project_items(verb, &workspace.projects),
            (Mode::Confirm(target), _) => confirm_items(workspace, target),
            (Mode::NameProject(_), _) => Vec::new(),
            (Mode::Search(_), Some(profile)) => search_items(workspace, profile, cx),
            (Mode::Font(slot), Some(_)) => font_items(slot, cx),
            (Mode::Database, Some(profile)) => database_items(profile),
            (Mode::Schema, Some(profile)) => schema_items(profile),
            (Mode::Columns, Some(profile)) => column_items(profile, cx),
            (Mode::Engine, _) => engine_items(workspace),
            (_, None) => Vec::new(),
        };
        let import_pending = workspace.pending_import.is_some();
        if import_pending {
            items.retain(|item| !waits_for_import(item.scope, &item.command));
        }
        let naming = match mode {
            Mode::NameProject(renaming) => Some(Naming {
                renaming: renaming
                    .and_then(|at| workspace.projects.get(at))
                    .map(|project| project.name.clone()),
                taken: workspace
                    .projects
                    .iter()
                    .map(|project| project.name.clone())
                    .collect(),
                refusal: None,
            }),
            _ => None,
        };
        let prefill = naming
            .as_ref()
            .and_then(|naming| naming.renaming.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(mode.placeholder());
            input.set_value(prefill, window, cx);
            input
        });
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
            collapsed: workspace.collapsed_projects.clone(),
            hint_chords: [
                "palette_previous",
                "palette_next",
                "palette_scope_left",
                "palette_scope_right",
                "show_editor",
                "palette_row_actions",
            ]
            .map(|id| chord_for(id, &workspace.settings.custom_keybindings)),
            naming,
            opened: mode,
            workspace: handle,
            catalog_loading: workspace
                .profile()
                .is_some_and(|profile| matches!(profile.catalog, CatalogState::Loading)),
            import_pending,
            input,
            scroll: VirtualListScrollHandle::new(),
            unpreviewed: matches!(mode, Mode::Theme(_) | Mode::ImportTheme(..)).then(|| *theme(cx)),
            theme_opacity: workspace.settings.theme_opacity.clone(),
            fallback_opacity: workspace.settings.opacity,
            _input_events: input_events,
        };
        let typed = palette.input.read(cx).value();
        palette.name_row(&typed);
        palette.rearrange();
        palette.preview(window, cx);
        palette
    }

    /// The name field's one row for what is typed, or why there is none.
    fn name_row(&mut self, typed: &str) {
        if let Some(naming) = &mut self.naming {
            let (items, refusal) = naming_items(typed, naming);
            self.items = items;
            naming.refusal = refusal;
        }
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
        let lead = self.rebuild();
        self.selected = opening_selection(self.mode, lead);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// The rows for what is typed, leaving the selection and the scroll to
    /// the caller. Returns the row the list would open on.
    fn rebuild(&mut self) -> Option<usize> {
        // The name field is not a filter: what is typed is the name.
        let Arranged { rows, lead } = if self.naming.is_some() {
            let rows: Vec<Row> = (0..self.items.len()).map(Row::Item).collect();
            let lead = first_item(&rows);
            Arranged { rows, lead }
        } else {
            arrange(
                &self.items,
                self.mode.scope(),
                &self.query,
                &mut self.fuzzy,
                &self.recents,
                &self.collapsed,
            )
        };
        self.rows = rows;
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
        lead
    }

    /// The projects folded away as the workspace now has them. The row
    /// selected stays selected and in view: it is the project's own row,
    /// which never folds away.
    pub fn set_collapsed(&mut self, collapsed: HashSet<String>, cx: &mut Context<Self>) {
        let selected = self.selected_item();
        self.collapsed = collapsed;
        let lead = self.rebuild();
        self.selected = selected
            .and_then(|index| self.rows.iter().position(|row| *row == Row::Item(index)))
            .or(lead);
        if let Some(row) = self.selected {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn selected_item(&self) -> Option<usize> {
        match self.rows.get(self.selected?) {
            Some(&Row::Item(index)) => Some(index),
            _ => None,
        }
    }

    /// The catalog's objects as they stand now, for a palette opened before
    /// they arrived. The row selected stays selected.
    pub fn catalog_changed(&mut self, profile: &Profile, cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Search(_)) {
            return;
        }
        let selected = self
            .selected
            .and_then(|row| match self.rows.get(row) {
                Some(&Row::Item(index)) => Some(&self.items[index]),
                _ => None,
            })
            .map(|item| (item.scope, item.label.clone()));
        self.items.retain(|item| item.scope != Scope::Objects);
        // Where `search_items` puts them: after the queries.
        let at = self
            .items
            .iter()
            .take_while(|item| item.scope == Scope::Queries)
            .count();
        self.items.splice(at..at, object_items(profile));
        self.catalog_loading = matches!(profile.catalog, CatalogState::Loading);
        self.rearrange();
        if let Some((scope, label)) = selected
            && let Some(row) = self.rows.iter().position(|row| {
                matches!(row, Row::Item(index)
                    if self.items[*index].scope == scope && self.items[*index].label == label)
            })
        {
            self.selected = Some(row);
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
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
                self.name_row(&text);
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
            let target = match row.checked_sub(1).map(|above| &self.rows[above]) {
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

    /// `left` and `right` switch scope from the edge of the field and are the
    /// caret's keys everywhere else. Handing a keystroke on is what lets the
    /// field's own binding for it, the next in line, take it.
    fn arrow(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let at = arrow(input.selected_range(), input.value().len(), delta);
        match (self.mode, at) {
            (Mode::Search(_), Arrow::Scope) => self.cycle_scope(delta, window, cx),
            _ => cx.propagate(),
        }
    }

    fn confirm(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        let Some(&Row::Item(index)) = row.and_then(|row| self.rows.get(row)) else {
            return;
        };
        let item = &self.items[index];
        cx.emit(PaletteEvent::Confirm {
            command: item.command.clone(),
            scope: item.scope,
            label: item.label.clone(),
        });
    }

    /// Over the one row that removes something, a click on it unselected only
    /// selects it, for the reason it opens unselected: the click that opened
    /// the list lands where its row is, and a second one would confirm it.
    fn click(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Confirm(_)) && self.selected != Some(at) {
            self.select(Some(at), false, window, cx);
        } else {
            self.confirm(Some(at), cx);
        }
    }

    /// A row's secondary action, if it has one.
    fn action_at(&self, row: usize) -> Option<Command> {
        if !matches!(self.mode, Mode::Search(_)) {
            return None;
        }
        let &Row::Item(index) = self.rows.get(row)? else {
            return None;
        };
        match self.items[index].command {
            Command::SwitchProfile(profile) => Some(Command::ConnectionActions(profile)),
            Command::ToggleProject(at) => Some(Command::ProjectActions(at)),
            _ => None,
        }
    }

    fn row_action(&self) -> Option<Command> {
        self.action_at(self.selected?)
    }

    fn act_on(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(command) = self.action_at(row) else {
            return;
        };
        let &Row::Item(index) = &self.rows[row] else {
            return;
        };
        let scope = self.items[index].scope;
        cx.emit(PaletteEvent::Act { command, scope });
    }

    /// `shift-enter` on a row with no actions, and in the name field, is the
    /// `enter` the field would have taken it for.
    fn act(&mut self, cx: &mut Context<Self>) {
        match self.selected.filter(|&row| self.action_at(row).is_some()) {
            Some(row) => self.act_on(row, cx),
            None => self.confirm(self.selected, cx),
        }
    }

    /// The system changed side under an open theme list, so a row that is not
    /// a theme falls back to the theme for the side it is on now.
    pub fn set_unpreviewed(&mut self, theme: Theme, window: &mut Window, cx: &mut Context<Self>) {
        if self.unpreviewed.is_some() {
            self.unpreviewed = Some(theme);
            self.preview(window, cx);
        }
    }

    /// The live preview. Nothing here is saved, and nothing here undoes it:
    /// `Workspace::end_theme_preview` does that on the way out.
    fn preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(unpreviewed) = self.unpreviewed else {
            return;
        };
        let command = match self.selected.and_then(|row| self.rows.get(row)) {
            Some(&Row::Item(index)) => Some(&self.items[index].command),
            _ => None,
        };
        let previewed = match command {
            Some(Command::SetTheme(_, candidate)) => Some(**candidate),
            Some(Command::ImportTheme(source, index, _)) => source.theme(*index),
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
        // The row echoes the name being typed, and every letter of it would
        // light up.
        if self.naming.is_some() {
            return Rc::from([]);
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
        let index = match &self.rows[at] {
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
                    .child(name.clone())
                    .into_any_element();
            }
            &Row::Item(index) => index,
        };
        let ranges = self.highlights(index);
        let selected = self.selected == Some(at);
        let acts = self.action_at(at).is_some();
        let item = &self.items[index];
        let tree = self.mode == Mode::Search(Scope::Connections);
        // Only drawn in the Connections scope: under All the same rows are a
        // flat list of the project in front.
        let folded = match item.command {
            Command::ToggleProject(_) if tree => Some(self.collapsed.contains(&item.label)),
            _ => None,
        };
        let nested = tree && folded.is_none() && matches!(item.group, Group::Project(..));
        let status = item.status.and_then(|profile| {
            let workspace = self.workspace.upgrade()?;
            let state = &workspace.read(cx).profiles.get(profile)?.state;
            Some(status_dot(t, state))
        });
        let mono = fonts(cx).family(FontSlot::Editor).clone();
        let row_height = layout::chrome(layout::PALETTE_ROW_HEIGHT);
        let tile = layout::chrome(layout::PALETTE_TILE_SIZE);
        let bar = layout::chrome(layout::PALETTE_ACCENT_BAR_HEIGHT);
        // A project's connections start where its name does, and the guide
        // runs down from the middle of its tile.
        let indent = tile + layout::SPACE_MD;
        let guide = 2. * layout::SPACE_SM + tile / 2.;

        div()
            .id(("palette-row", at))
            .group(ROW_GROUP)
            .relative()
            .h(px(row_height))
            .px(px(layout::SPACE_SM))
            .when(nested, |row| {
                row.pl(px(layout::SPACE_SM + indent)).child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(guide))
                        .border_l_1()
                        .border_color(t.border),
                )
            })
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
                    .children(status)
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
                                    .when(folded.is_some(), |label| {
                                        label.font_weight(FontWeight::MEDIUM)
                                    })
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
                    .children(item.keys.as_deref().map(|chord| keycaps(chord, t)))
                    .children(folded.map(|folded| {
                        icon(if folded {
                            icon::CHEVRON_RIGHT
                        } else {
                            icon::CHEVRON_DOWN
                        })
                        .flex_shrink_0()
                        .size(px(layout::chrome(layout::ICON_SIZE)))
                        .text_color(t.text_muted)
                    }))
                    .when(acts, |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .when(!selected, |more| {
                                    more.opacity(0.)
                                        .group_hover(ROW_GROUP, |style| style.opacity(1.))
                                })
                                .child(
                                    icon_button(
                                        ("palette-row-actions", at),
                                        icon::MORE,
                                        Tone::Quiet,
                                        Control::Inline,
                                        t,
                                    )
                                    .tooltip("Actions")
                                    .on_click(cx.listener(
                                        move |palette, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            palette.act_on(at, cx);
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .on_mouse_move(cx.listener(move |palette, _, window, cx| {
                // Hover moves the selection only when the pointer does, so a
                // list scrolling under a resting pointer keeps the keyboard's.
                // Never over a theme list, where selecting repaints the window
                // and a pointer crossing it would flash every theme it passed,
                // nor over the row that removes something, which a click only
                // selects while it is not selected.
                if palette.selected != Some(at)
                    && palette.unpreviewed.is_none()
                    && !matches!(palette.mode, Mode::Confirm(_))
                {
                    palette.select(Some(at), false, window, cx);
                }
            }))
            .when(acts, |row| {
                row.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |palette, _, _, cx| {
                        cx.stop_propagation();
                        palette.act_on(at, cx);
                    }),
                )
            })
            .on_click(cx.listener(move |palette, _, window, cx| palette.click(at, window, cx)))
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
            Mode::NameProject(_) => match self.naming.as_ref().and_then(|n| n.refusal.clone()) {
                Some(refusal) => ("Pick another name".to_string(), refusal),
                None => (
                    "Name the project".to_string(),
                    "Type its name, then press Enter.".to_string(),
                ),
            },
            Mode::ImportTheme(source, side) if self.items.is_empty() => (
                "No themes found".to_string(),
                match side {
                    Some(side) => format!(
                        "{} has no {} themes on this machine.",
                        source.name(),
                        side.name()
                    ),
                    None => format!("{} has no themes on this machine.", source.name()),
                },
            ),
            Mode::Search(Scope::Objects) if self.catalog_loading => (
                "Loading database objects…".to_string(),
                "They are listed here as soon as they arrive.".to_string(),
            ),
            Mode::Search(Scope::Connections) if self.import_pending => (
                "Connections are on hold".to_string(),
                "Choose where the imported connections go first.".to_string(),
            ),
            // `cmd+p` opens on Objects, which is empty on a connection that
            // has none.
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

    /// What `enter` does to the row selected, for the footer.
    fn verb(&self) -> &'static str {
        let item = self.selected_item().map(|index| &self.items[index]);
        match item {
            Some(item) if matches!(item.command, Command::ToggleProject(_)) => {
                if self.collapsed.contains(&item.label) {
                    "Expand"
                } else {
                    "Collapse"
                }
            }
            _ => self.mode.verb(),
        }
    }

    fn render_footer(&self, t: Theme) -> impl IntoElement {
        let [previous, next, left, right, close, actions] =
            self.hint_chords.each_ref().map(Option::as_deref);
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
                    .child(footer_hint(&[Some("enter")], self.verb(), t))
                    .when(self.row_action().is_some(), |hints| {
                        hints.child(footer_hint(&[actions], "Actions", t))
                    })
                    .when(matches!(self.mode, Mode::Search(_)), |hints| {
                        hints.child(footer_hint(&[left, right], "Scope", t))
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
                    .on_action(cx.listener(|palette, _: &PaletteScopeLeft, window, cx| {
                        palette.arrow(-1, window, cx)
                    }))
                    .on_action(cx.listener(|palette, _: &PaletteScopeRight, window, cx| {
                        palette.arrow(1, window, cx)
                    }))
                    .on_action(cx.listener(|palette, _: &PaletteRowActions, _, cx| palette.act(cx)))
                    .child(
                        search_field(
                            icon(icon::SEARCH)
                                .size(px(layout::chrome(layout::ICON_SIZE)))
                                .text_color(t.text_muted),
                            &self.input,
                            t,
                        )
                        .children(
                            self.hint_chords[4]
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

const NO_PROJECT: &str = "No project";
const PROJECTS: &str = "Projects";
const MANAGE: &str = "Manage";

/// A connection as the Connections scope lists it, read off its profile so
/// the listing can be worked out without a workspace.
struct Listed<'a> {
    id: &'a str,
    name: &'a str,
    /// Never a SQLite path, which under a home directory holds the name of
    /// every folder on the way and would match nearly anything.
    host: String,
    endpoint: String,
    logo: &'static str,
    color: Option<ConnectionColor>,
}

/// Every connection, by project, then the ways to add one or a project.
fn connection_items(workspace: &Workspace) -> Vec<Item> {
    let listed: Vec<Listed> = workspace
        .profiles
        .iter()
        .map(|profile| Listed {
            id: &profile.id,
            name: &profile.name,
            host: match &profile.config {
                ConnectionConfig::Snowflake(snowflake) => snowflake.host(),
                config => config
                    .server()
                    .map(|server| server.host.clone())
                    .unwrap_or_default(),
            },
            endpoint: profile.config.endpoint(),
            logo: profile.config.engine().logo(),
            color: profile.color,
        })
        .collect();
    let overrides = &workspace.settings.custom_keybindings;
    let mut items = listed_connections(&listed, &workspace.projects, workspace.active);
    items.push(
        Item::new("New connection…", icon::PLUS, Command::NewConnection)
            .keys(chord_for("new_connection", overrides))
            .scope(Scope::Connections)
            .group(Group::Named(MANAGE))
            .beyond_all(),
    );
    items.push(
        Item::new("New project…", icon::ADD_TO_PROJECT, Command::NewProject)
            .keys(chord_for("new_project", overrides))
            .scope(Scope::Connections)
            .group(Group::Named(MANAGE))
            .beyond_all(),
    );
    items
}

/// The connections by project: No project first, then each project in its
/// order, its own row ahead of its members in the order the profiles are
/// kept, which [`tree`] draws beneath it. An empty project keeps its place
/// with a row that adds a connection to it. Under All only the other
/// connections in the project in front are listed, which is all the palette
/// ever offered there.
fn listed_connections(
    connections: &[Listed],
    projects: &[StoredProject],
    active: usize,
) -> Vec<Item> {
    let project_of: Vec<Option<usize>> = connections
        .iter()
        .map(|connection| {
            projects.iter().position(|project| {
                project
                    .connections
                    .iter()
                    .any(|member| member == connection.id)
            })
        })
        .collect();
    let current = project_of.get(active).copied().flatten();
    let mut items = Vec::new();
    for group in std::iter::once(None).chain((0..projects.len()).map(Some)) {
        let header = match group {
            Some(at) => Group::Project(at, projects[at].name.clone().into()),
            None if projects.is_empty() => Group::Bare,
            None => Group::Named(NO_PROJECT),
        };
        if let Some(at) = group {
            let held = project_of.iter().filter(|of| **of == group).count();
            items.push(
                Item::new(
                    projects[at].name.clone(),
                    icon::PROJECT,
                    Command::ToggleProject(at),
                )
                .hint(connection_count(held))
                .scope(Scope::Connections)
                .group(header.clone())
                .beyond_all(),
            );
        }
        let before = items.len();
        for (index, connection) in connections.iter().enumerate() {
            if project_of[index] != group {
                continue;
            }
            let item = Item::new(
                connection.name,
                connection.logo,
                Command::SwitchProfile(index),
            )
            .detail(connection.endpoint.clone())
            .tint(connection.color)
            .status(index)
            .hint(if index == active { "current" } else { "" })
            .also(vec![connection.host.clone()])
            .scope(Scope::Connections)
            .group(header.clone());
            items.push(if index == active || group != current {
                item.beyond_all()
            } else {
                item
            });
        }
        if let Some(at) = group
            && items.len() == before
        {
            let name = &projects[at].name;
            items.push(
                Item::new(
                    format!("New connection in {name}…"),
                    icon::PLUS,
                    Command::NewConnectionIn(Some(name.clone())),
                )
                .scope(Scope::Connections)
                .group(header)
                .beyond_all(),
            );
        }
    }
    items
}

/// Everything that can be done to one connection, then to the project it is
/// in.
fn connection_action_items(workspace: &Workspace, index: usize) -> Vec<Item> {
    let Some(profile) = workspace.profiles.get(index) else {
        return Vec::new();
    };
    let name = &profile.name;
    let project = workspace
        .projects
        .iter()
        .position(|project| project.connections.contains(&profile.id));
    let row = |label: String, icon: &'static str, command: Command, group: &'static str| {
        Item::new(label, icon, command).group(Group::Named(group))
    };
    let mut items = Vec::new();
    if index != workspace.active {
        items.push(
            row(
                format!("Switch to {name}"),
                profile.config.engine().logo(),
                Command::SwitchProfile(index),
                CONNECTION,
            )
            .tint(profile.color),
        );
    }
    items.push(row(
        format!("Edit {name}…"),
        icon::RENAME,
        Command::EditConnection(index),
        CONNECTION,
    ));
    items.push(row(
        format!("Duplicate {name}…"),
        icon::COPY,
        Command::DuplicateConnection(index),
        CONNECTION,
    ));
    if !workspace.projects.is_empty() {
        items.push(row(
            format!("Move {name} to a project…"),
            icon::ADD_TO_PROJECT,
            Command::ChooseProject(index),
            CONNECTION,
        ));
    }
    if let Some(at) = project {
        items.push(row(
            format!("Remove {name} from {}", workspace.projects[at].name),
            icon::LEAVE_PROJECT,
            Command::MoveToProject(index, None),
            CONNECTION,
        ));
    }
    items.push(row(
        format!("Remove {name}…"),
        icon::DELETE,
        Command::Confirm(Target::Connection(index)),
        CONNECTION,
    ));
    if let Some(at) = project {
        items.extend(project_action_items(
            at,
            &workspace.projects[at].name,
            Group::Named(PROJECT),
        ));
    }
    items
}

/// Everything that can be done to one project.
fn project_action_items(at: usize, name: &str, group: Group) -> Vec<Item> {
    [
        Item::new(
            format!("New connection in {name}…"),
            icon::PLUS,
            Command::NewConnectionIn(Some(name.to_string())),
        ),
        Item::new(
            format!("Rename {name}…"),
            icon::RENAME,
            Command::NameProject(Some(at)),
        ),
        Item::new(
            format!("Delete {name}…"),
            icon::DELETE,
            Command::Confirm(Target::Project(at)),
        ),
    ]
    .into_iter()
    .map(|item| item.group(group.clone()))
    .collect()
}

/// No project, then every project, the one the connection is in marked the
/// way the theme list marks its theme.
fn move_items(index: usize, id: &str, projects: &[StoredProject]) -> Vec<Item> {
    let current = projects
        .iter()
        .position(|project| project.connections.iter().any(|member| member == id));
    let hint = |here: bool| if here { "current" } else { "" };
    std::iter::once(
        Item::new(
            NO_PROJECT,
            icon::LEAVE_PROJECT,
            Command::MoveToProject(index, None),
        )
        .hint(hint(current.is_none())),
    )
    .chain(projects.iter().enumerate().map(|(at, project)| {
        Item::new(
            project.name.clone(),
            icon::PROJECT,
            Command::MoveToProject(index, Some(project.name.clone())),
        )
        .hint(hint(current == Some(at)))
    }))
    .collect()
}

/// Where [`move_items`] lists the connection's own project, for the palette
/// to open on.
pub(crate) fn current_project_row(
    index: usize,
    id: &str,
    projects: &[StoredProject],
) -> Option<usize> {
    move_items(index, id, projects)
        .iter()
        .position(|item| item.hint.as_ref() == "current")
}

/// Every project, with how many connections it holds.
fn project_items(verb: ProjectVerb, projects: &[StoredProject]) -> Vec<Item> {
    projects
        .iter()
        .enumerate()
        .map(|(at, project)| {
            let command = match verb {
                ProjectVerb::Rename => Command::NameProject(Some(at)),
                ProjectVerb::Delete => Command::Confirm(Target::Project(at)),
            };
            Item::new(project.name.clone(), icon::PROJECT, command)
                .hint(connection_count(project.connections.len()))
        })
        .collect()
}

fn connection_count(count: usize) -> String {
    match count {
        0 => "empty".to_string(),
        1 => "1 connection".to_string(),
        count => format!("{count} connections"),
    }
}

/// The one row that does it, saying what goes with it and what stays.
fn confirm_items(workspace: &Workspace, target: Target) -> Vec<Item> {
    let (label, command, hint) = match target {
        Target::Connection(index) => {
            let Some(profile) = workspace.profiles.get(index) else {
                return Vec::new();
            };
            (
                format!("Remove {}", profile.name),
                Command::RemoveConnection(index),
                "its saved queries go too",
            )
        }
        Target::Project(at) => {
            let Some(project) = workspace.projects.get(at) else {
                return Vec::new();
            };
            (
                format!("Delete {}", project.name),
                Command::DeleteProject(project.name.clone()),
                "its connections are kept",
            )
        }
    };
    vec![
        Item::new(label, icon::DELETE, command)
            .hint(hint)
            .tint(Some(ConnectionColor::Red)),
    ]
}

/// The one row that names the project what is typed, or why there is none:
/// nothing typed yet, a name another project has, or on a rename the name it
/// has now, which would rename it to itself.
fn naming_items(typed: &str, naming: &Naming) -> (Vec<Item>, Option<String>) {
    let name = typed.trim();
    if name.is_empty() {
        return (Vec::new(), None);
    }
    if let Some(refusal) = crate::workspace::project_name_refusal(
        name,
        naming.renaming.as_deref(),
        naming.taken.iter().map(String::as_str),
    ) {
        return (Vec::new(), Some(refusal));
    }
    if naming.renaming.as_deref() == Some(name) {
        return (Vec::new(), Some(format!("Type a new name for {name}.")));
    }
    let item = match &naming.renaming {
        Some(old) => Item::new(
            format!("Rename {old} to {name}"),
            icon::RENAME,
            Command::RenameProject(old.clone(), name.to_string()),
        ),
        None => Item::new(
            format!("Create project {name}"),
            icon::ADD_TO_PROJECT,
            Command::CreateProject(name.to_string()),
        ),
    };
    (vec![item], None)
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

/// The theme a theme list marks as picked, and opens on: the one on screen,
/// or the one worn on the side the list picks for, which may be the side the
/// system is not on.
pub(crate) fn picked_theme<'a>(
    workspace: &'a Workspace,
    side: Option<Appearance>,
    cx: &App,
) -> &'a str {
    match side {
        Some(side) => workspace.settings.theme_for(side),
        None => theme(cx).name,
    }
}

fn on_side(side: Option<Appearance>, candidate: &Theme) -> bool {
    side.is_none_or(|side| candidate.appearance == side)
}

pub(crate) fn listed_themes(side: Option<Appearance>) -> impl Iterator<Item = Theme> {
    Theme::all()
        .into_iter()
        .filter(move |candidate| on_side(side, candidate))
}

/// Every theme [`listed_themes`] lists, the one picked marked the way the
/// font list marks its family. The ways to import one follow under their own
/// header, so the list opens on a theme and reads as themes first.
fn theme_items(side: Option<Appearance>, picked: &str) -> Vec<Item> {
    let themes = listed_themes(side).map(|candidate| {
        let hint = if candidate.name == picked {
            "current"
        } else {
            ""
        };
        Item::new(
            candidate.name,
            icon::THEME,
            Command::SetTheme(side, Box::new(candidate)),
        )
        .hint(hint)
        .group(Group::Named("Themes"))
    });
    let imports = user::Source::ALL.into_iter().map(|source| {
        Item::new(
            format!("Import from {}…", source.name()),
            icon::PLUS,
            Command::BrowseThemes(source, side),
        )
        .group(Group::Named("Import"))
    });
    themes.chain(imports).collect()
}

pub(crate) fn listed_imports(
    source: user::Source,
    side: Option<Appearance>,
) -> impl Iterator<Item = (usize, &'static Theme)> {
    source
        .found()
        .iter()
        .enumerate()
        .filter(move |(_, candidate)| on_side(side, candidate))
}

/// Every theme one app has, without the app's name every one of them is
/// prefixed with, since the list is that app's alone. One already imported is
/// marked, since importing it again only writes the same file over itself.
fn import_items(source: user::Source, side: Option<Appearance>) -> Vec<Item> {
    listed_imports(source, side)
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
            Item::new(
                label,
                icon::THEME,
                Command::ImportTheme(source, index, side),
            )
            .hint(hint)
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

/// Every engine, the one the connection form is set to marked as current.
fn engine_items(workspace: &Workspace) -> Vec<Item> {
    let current = workspace.form.as_ref().map(|form| form.engine);
    Engine::ALL
        .into_iter()
        .map(|engine| {
            let hint = if current == Some(engine) {
                "current"
            } else {
                ""
            };
            Item::new(engine.label(), engine.logo(), Command::SetEngine(engine)).hint(hint)
        })
        .collect()
}

/// Where [`engine_items`] lists the form's current engine, for the palette to
/// open on.
pub(crate) fn active_engine_row(workspace: &Workspace) -> Option<usize> {
    engine_items(workspace)
        .iter()
        .position(|item| item.hint.as_ref() == "current")
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
const PROJECT: &str = "Project";
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
        Item::new(label, icon, command).group(Group::Named(group))
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
                &format!("Insert in {table}…"),
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
                items.push(command("Insert…", icon::PLUS, Command::NewRow, ROWS));
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
    // The active connection's share of what the Connections scope offers
    // every connection on its row's actions, for whoever looks for the verb.
    let active = workspace.active;
    items.push(bound(
        command(
            "Switch connection…",
            icon::DATABASE,
            Command::SwitchConnection,
            CONNECTION,
        ),
        "switch_connection",
    ));
    items.push(command(
        "Edit connection…",
        icon::RENAME,
        Command::EditConnection(active),
        CONNECTION,
    ));
    items.push(command(
        "Duplicate connection…",
        icon::COPY,
        Command::DuplicateConnection(active),
        CONNECTION,
    ));
    if !workspace.projects.is_empty() {
        items.push(command(
            "Move connection to a project…",
            icon::ADD_TO_PROJECT,
            Command::ChooseProject(active),
            CONNECTION,
        ));
    }
    items.push(command(
        "Remove connection…",
        icon::DELETE,
        Command::Confirm(Target::Connection(active)),
        CONNECTION,
    ));
    if !workspace.projects.is_empty() {
        items.push(command(
            "Rename project…",
            icon::RENAME,
            Command::PickProject(ProjectVerb::Rename),
            PROJECT,
        ));
        items.push(command(
            "Delete project…",
            icon::DELETE,
            Command::PickProject(ProjectVerb::Delete),
            PROJECT,
        ));
    }
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
            | Command::SwitchConnection
            | Command::EditConnection(_)
            | Command::DuplicateConnection(_)
            | Command::ChooseProject(_)
            | Command::Confirm(_)
            | Command::NextProfile
            | Command::PreviousProfile
            | Command::NewConnection
            | Command::NewProject
            | Command::PickProject(_)
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
            .group(match group {
                "" => Group::Bare,
                name => Group::Named(name),
            })
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
        let arranged = arrange(
            items,
            scope,
            &Query::new(query),
            &mut Fuzzy::new(),
            &recents,
            &HashSet::new(),
        );
        labels(items, &arranged.rows)
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
            &HashSet::new(),
        )
        .rows;
        assert_eq!(rows[..2], [Row::Header(RECENT_GROUP.into()), Row::Item(1)]);
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
        assert_eq!(stroke(commands, objects, all), Stroke::Rescope(Scope::All));
        assert_eq!(stroke(objects, all, all), Stroke::Close);
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
            &HashSet::new(),
        )
        .rows;
        assert_eq!(
            labels(&items, &rows),
            [
                "# Recent",
                "public.events",
                "# Objects",
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
        let themes = Mode::Theme(None);
        let light = Mode::Theme(Some(Appearance::Light));
        assert_eq!(stroke(themes, themes, all), Stroke::Replace);
        assert_eq!(stroke(all, objects, themes), Stroke::Replace);
        assert_eq!(stroke(themes, themes, themes), Stroke::Close);
        assert_eq!(stroke(light, light, themes), Stroke::Replace);
    }

    #[test]
    fn a_side_list_holds_that_side_and_imports_for_it() {
        let light = Some(Appearance::Light);
        let items = theme_items(light, "GitHub Light");
        for item in &items {
            match &item.command {
                Command::SetTheme(side, candidate) => {
                    assert_eq!(*side, light);
                    assert_eq!(candidate.appearance, Appearance::Light);
                }
                Command::BrowseThemes(_, side) => assert_eq!(*side, light),
                _ => panic!("{} is neither a theme nor an import", item.label),
            }
        }
        let picked: Vec<&str> = items
            .iter()
            .filter(|item| item.hint.as_ref() == "current")
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(picked, ["GitHub Light"]);
    }

    #[test]
    fn the_selection_skips_headers_and_wraps_both_ways() {
        let rows = [
            Row::Header("A".into()),
            Row::Item(0),
            Row::Item(1),
            Row::Header("B".into()),
            Row::Item(2),
        ];
        assert_eq!(first_item(&rows), Some(1));
        assert_eq!(step(&rows, Some(1), 1), Some(2));
        assert_eq!(step(&rows, Some(2), 1), Some(4));
        assert_eq!(step(&rows, Some(4), 1), Some(1));
        assert_eq!(step(&rows, Some(1), -1), Some(4));
        assert_eq!(step(&rows, None, -1), Some(4));
        assert_eq!(step(&[Row::Header("A".into())], None, 1), None);
    }

    #[test]
    fn the_scopes_cycle_with_tab_and_wrap() {
        assert_eq!(Scope::All.step(1), Scope::Commands);
        assert_eq!(Scope::All.step(-1), Scope::Connections);
        assert_eq!(Scope::Connections.step(1), Scope::All);
    }

    #[test]
    fn the_arrows_switch_scope_only_from_the_edge_the_caret_is_at() {
        assert_eq!(arrow(0..0, 0, -1), Arrow::Scope);
        assert_eq!(arrow(0..0, 0, 1), Arrow::Scope);
        assert_eq!(arrow(0..0, 5, -1), Arrow::Scope);
        assert_eq!(arrow(0..0, 5, 1), Arrow::Caret);
        assert_eq!(arrow(5..5, 5, 1), Arrow::Scope);
        assert_eq!(arrow(5..5, 5, -1), Arrow::Caret);
        assert_eq!(arrow(2..2, 5, -1), Arrow::Caret);
        assert_eq!(arrow(2..2, 5, 1), Arrow::Caret);
        // A selection reaching the edge collapses first, as in any field.
        assert_eq!(arrow(0..5, 5, -1), Arrow::Caret);
        assert_eq!(arrow(0..5, 5, 1), Arrow::Caret);
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

    fn listed<'a>(id: &'a str, name: &'a str, host: &str) -> Listed<'a> {
        Listed {
            id,
            name,
            host: host.to_string(),
            endpoint: host.to_string(),
            logo: Engine::Postgres.logo(),
            color: None,
        }
    }

    fn project(name: &str, connections: &[&str]) -> StoredProject {
        StoredProject {
            name: name.into(),
            connections: connections.iter().map(|id| id.to_string()).collect(),
        }
    }

    fn connections() -> (Vec<Listed<'static>>, Vec<StoredProject>) {
        (
            vec![
                listed("orders", "Orders", "db.internal"),
                listed("scratch", "Scratch", "localhost"),
                listed("ledger", "Ledger", "localhost"),
                listed("warehouse", "Warehouse", "localhost"),
            ],
            vec![
                project("Billing", &["ledger", "orders"]),
                project("Empty", &[]),
                project("Analytics", &["warehouse"]),
            ],
        )
    }

    #[test]
    fn a_connection_row_wears_its_engines_logo() {
        let (mut listed, projects) = connections();
        listed[1].logo = Engine::Sqlite.logo();
        let items = listed_connections(&listed, &projects, 0);
        for item in &items {
            if let Command::SwitchProfile(index) = item.command {
                assert_eq!(item.icon, listed[index].logo, "{}", item.label);
            }
        }
    }

    /// The Connections scope's rows, each child of a project marked with
    /// `|`, and the row it opens on.
    fn tree_of(items: &[Item], query: &str, collapsed: &[&str]) -> (Vec<String>, Option<String>) {
        let collapsed = collapsed.iter().map(|name| name.to_string()).collect();
        let Arranged { rows, lead } = arrange(
            items,
            Some(Scope::Connections),
            &Query::new(query),
            &mut Fuzzy::new(),
            &[],
            &collapsed,
        );
        let drawn = rows
            .iter()
            .map(|row| match row {
                Row::Header(name) => format!("# {name}"),
                Row::Item(index) => {
                    let item = &items[*index];
                    let child = matches!(item.group, Group::Project(..))
                        && !matches!(item.command, Command::ToggleProject(_));
                    format!("{}{}", if child { "| " } else { "" }, item.label)
                }
            })
            .collect();
        let lead = lead.map(|at| labels(items, &rows)[at].clone());
        (drawn, lead)
    }

    fn with_manage(mut items: Vec<Item>) -> Vec<Item> {
        items.push(item("New connection…", Scope::Connections, MANAGE));
        items.push(item("New project…", Scope::Connections, MANAGE));
        items
    }

    fn hint_of<'a>(items: &'a [Item], label: &str) -> &'a str {
        items
            .iter()
            .find(|item| item.label == label)
            .map(|item| item.hint.as_ref())
            .unwrap()
    }

    #[test]
    fn the_connections_scope_is_a_tree_of_projects_with_manage_below() {
        let (listed, projects) = connections();
        let items = with_manage(listed_connections(&listed, &projects, 0));
        let (rows, lead) = tree_of(&items, "", &[]);
        assert_eq!(
            rows,
            [
                "# No project",
                "Scratch",
                "# Projects",
                "Billing",
                "| Orders",
                "| Ledger",
                "Empty",
                "| New connection in Empty…",
                "Analytics",
                "| Warehouse",
                "# Manage",
                "New connection…",
                "New project…"
            ]
        );
        assert_eq!(lead.as_deref(), Some("Scratch"));
        assert_eq!(hint_of(&items, "Orders"), "current");
        assert_eq!(hint_of(&items, "Billing"), "2 connections");
        assert_eq!(hint_of(&items, "Analytics"), "1 connection");
        assert_eq!(hint_of(&items, "Empty"), "empty");
    }

    #[test]
    fn a_folded_project_hides_its_connections_until_something_is_typed() {
        let (listed, projects) = connections();
        let items = with_manage(listed_connections(&listed, &projects, 0));
        let (rows, _) = tree_of(&items, "", &["Billing", "Empty"]);
        assert_eq!(
            rows[2..7],
            ["# Projects", "Billing", "Empty", "Analytics", "| Warehouse"]
        );
        let (rows, lead) = tree_of(&items, "ledg", &["Billing"]);
        assert_eq!(rows, ["# Projects", "Billing", "| Ledger"]);
        assert_eq!(lead.as_deref(), Some("Ledger"));
    }

    #[test]
    fn a_matching_connection_brings_its_projects_row_unmarked() {
        let (listed, projects) = connections();
        let items = with_manage(listed_connections(&listed, &projects, 1));
        let (rows, lead) = tree_of(&items, "internal", &[]);
        assert_eq!(rows, ["# Projects", "Billing", "| Orders"]);
        assert_eq!(lead.as_deref(), Some("Orders"));
        let mut fuzzy = Fuzzy::new();
        assert!(
            fuzzy
                .highlights(&Query::new("internal"), "Billing")
                .is_empty()
        );
        assert!(
            fuzzy
                .highlights(&Query::new("internal"), "Orders")
                .is_empty()
        );
    }

    #[test]
    fn a_matching_project_brings_every_connection_it_holds() {
        let (listed, projects) = connections();
        let items = with_manage(listed_connections(&listed, &projects, 1));
        let (rows, lead) = tree_of(&items, "billing", &["Billing"]);
        assert_eq!(rows, ["# Projects", "Billing", "| Orders", "| Ledger"]);
        assert_eq!(lead.as_deref(), Some("Billing"));
    }

    #[test]
    fn the_project_holding_the_best_match_comes_first() {
        let (listed, projects) = connections();
        let items = with_manage(listed_connections(&listed, &projects, 1));
        let (rows, _) = tree_of(&items, "warehouse", &[]);
        assert_eq!(rows, ["# Projects", "Analytics", "| Warehouse"]);
        let (rows, lead) = tree_of(&items, "an", &[]);
        assert_eq!(rows[..3], ["# Projects", "Analytics", "| Warehouse"]);
        assert_eq!(lead.as_deref(), Some("Analytics"));
    }

    #[test]
    fn an_empty_project_holds_one_row_that_adds_a_connection_to_it() {
        let (listed, projects) = connections();
        let items = listed_connections(&listed, &projects, 0);
        let (rows, _) = tree_of(&items, "empty", &[]);
        assert_eq!(rows, ["# Projects", "Empty", "| New connection in Empty…"]);
        let child = items
            .iter()
            .find(|item| item.label == "New connection in Empty…")
            .unwrap();
        assert!(matches!(
            &child.command,
            Command::NewConnectionIn(Some(name)) if name == "Empty"
        ));
    }

    #[test]
    fn a_projects_row_folds_on_enter_and_offers_its_actions() {
        let (listed, projects) = connections();
        let items = listed_connections(&listed, &projects, 0);
        let billing = items.iter().find(|item| item.label == "Billing").unwrap();
        assert!(matches!(billing.command, Command::ToggleProject(0)));
        assert!(!billing.in_all);
        let actions: Vec<String> = project_action_items(0, "Billing", Group::Bare)
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(
            actions,
            [
                "New connection in Billing…",
                "Rename Billing…",
                "Delete Billing…"
            ]
        );
        assert!(waits_for_import(
            Scope::Commands,
            &Command::ProjectActions(0)
        ));
    }

    #[test]
    fn with_no_projects_the_connections_are_listed_bare() {
        let (listed, _) = connections();
        let items = listed_connections(&listed, &[], 1);
        assert_eq!(
            arranged(&items, Some(Scope::Connections), "", &[]),
            ["Orders", "Scratch", "Ledger", "Warehouse"]
        );
    }

    #[test]
    fn all_lists_only_the_other_connections_in_the_project_in_front() {
        let (listed, projects) = connections();
        let items = listed_connections(&listed, &projects, 0);
        assert_eq!(
            arranged(&items, Some(Scope::All), "", &[]),
            ["# Connections", "Ledger"]
        );
    }

    #[test]
    fn a_connection_moves_to_no_project_or_any_project_its_own_marked() {
        let (_, projects) = connections();
        let items = move_items(2, "ledger", &projects);
        let rows: Vec<(&str, &str)> = items
            .iter()
            .map(|item| (item.label.as_str(), item.hint.as_ref()))
            .collect();
        assert_eq!(
            rows,
            [
                ("No project", ""),
                ("Billing", "current"),
                ("Empty", ""),
                ("Analytics", "")
            ]
        );
        assert!(matches!(
            &items[3].command,
            Command::MoveToProject(2, Some(name)) if name == "Analytics"
        ));
        assert!(matches!(items[0].command, Command::MoveToProject(2, None)));
        assert_eq!(current_project_row(2, "ledger", &projects), Some(1));
        assert_eq!(current_project_row(1, "scratch", &projects), Some(0));
    }

    #[test]
    fn the_name_field_offers_one_row_unless_the_name_is_empty_or_taken() {
        let naming = |renaming: Option<&str>| Naming {
            renaming: renaming.map(str::to_string),
            taken: vec!["Billing".into(), "Analytics".into()],
            refusal: None,
        };
        let (items, refusal) = naming_items("  ", &naming(None));
        assert!(items.is_empty() && refusal.is_none());
        let (items, refusal) = naming_items("Billing", &naming(None));
        assert!(items.is_empty());
        assert_eq!(
            refusal.as_deref(),
            Some("A project named Billing already exists.")
        );
        let (items, _) = naming_items(" Ops ", &naming(None));
        assert!(matches!(&items[0].command, Command::CreateProject(name) if name == "Ops"));
        let (items, _) = naming_items("Ops", &naming(Some("Billing")));
        assert!(matches!(
            &items[0].command,
            Command::RenameProject(old, new) if old == "Billing" && new == "Ops"
        ));
    }

    #[test]
    fn a_rename_to_the_name_it_has_offers_no_row() {
        let naming = Naming {
            renaming: Some("Billing".into()),
            taken: vec!["Billing".into()],
            refusal: None,
        };
        let (items, refusal) = naming_items(" Billing ", &naming);
        assert!(items.is_empty());
        assert_eq!(refusal.as_deref(), Some("Type a new name for Billing."));
    }

    #[test]
    fn rows_that_remove_or_switch_connections_are_not_remembered() {
        assert!(remembered(Scope::Commands, &Command::NewQuery));
        assert!(remembered(
            Scope::Commands,
            &Command::PickProject(ProjectVerb::Rename)
        ));
        assert!(!remembered(Scope::Connections, &Command::SwitchProfile(1)));
        assert!(!remembered(Scope::Connections, &Command::NewConnection));
        for command in [
            Command::Confirm(Target::Connection(0)),
            Command::Confirm(Target::Project(0)),
            Command::PickProject(ProjectVerb::Delete),
            Command::RemoveConnection(0),
            Command::DeleteProject("Billing".into()),
        ] {
            assert!(!remembered(Scope::Commands, &command));
        }
    }

    #[test]
    fn the_row_that_removes_something_opens_unselected() {
        let rows = [Row::Item(0)];
        assert_eq!(
            opening_selection(Mode::Confirm(Target::Connection(0)), first_item(&rows)),
            None
        );
        assert_eq!(
            opening_selection(Mode::Projects(ProjectVerb::Delete), first_item(&rows)),
            Some(0)
        );
        assert_eq!(step(&rows, None, 1), Some(0));
    }

    #[test]
    fn with_no_projects_the_connections_get_a_header_beside_manage() {
        let (mut listed, _) = connections();
        listed.push(listed_newsroom());
        let mut items = listed_connections(&listed, &[], 0);
        items.push(item("New connection…", Scope::Connections, MANAGE));
        assert_eq!(
            arranged(&items, Some(Scope::Connections), "new", &[]),
            ["# Connections", "Newsroom", "# Manage", "New connection…"]
        );
        assert_eq!(
            arranged(&items, Some(Scope::Connections), "newsroom", &[]),
            ["Newsroom"]
        );
    }

    fn listed_newsroom() -> Listed<'static> {
        listed("newsroom", "Newsroom", "localhost")
    }

    #[test]
    fn a_project_named_like_a_group_stays_a_group_of_its_own() {
        let (listed, _) = connections();
        let projects = [
            project(NO_PROJECT, &["ledger"]),
            project(MANAGE, &["warehouse"]),
        ];
        let mut items = listed_connections(&listed, &projects, 0);
        items.push(item("New connection…", Scope::Connections, MANAGE));
        assert_eq!(
            arranged(&items, Some(Scope::Connections), "", &[]),
            [
                "# No project",
                "Orders",
                "Scratch",
                "# Projects",
                "No project",
                "Ledger",
                "Manage",
                "Warehouse",
                "# Manage",
                "New connection…"
            ]
        );
    }

    #[test]
    fn a_match_never_runs_from_a_connections_name_into_its_host() {
        let (listed, projects) = connections();
        let items = listed_connections(&listed, &projects, 1);
        // "Ledger localhost" as one text would take the r, then the l and o.
        assert!(arranged(&items, Some(Scope::Connections), "rlo", &[]).is_empty());
        assert_eq!(
            arranged(&items, Some(Scope::Connections), "ledg", &[]),
            ["# Projects", "Billing", "Ledger"]
        );
    }

    #[test]
    fn while_an_import_asks_nothing_that_names_a_project_or_a_connection_is_listed() {
        let (listed, projects) = connections();
        let mut items = listed_connections(&listed, &projects, 0);
        items.extend([
            Item::new("Run query", icon::RUN, Command::RunQuery),
            Item::new("New connection…", icon::PLUS, Command::NewConnection),
            Item::new("New project…", icon::PLUS, Command::NewProject),
            Item::new(
                "Switch connection…",
                icon::DATABASE,
                Command::SwitchConnection,
            ),
            Item::new(
                "Remove connection…",
                icon::DELETE,
                Command::Confirm(Target::Connection(0)),
            ),
            Item::new(
                "Delete project…",
                icon::DELETE,
                Command::PickProject(ProjectVerb::Delete),
            ),
            Item::new(
                "Remove Orders from Billing",
                icon::LEAVE_PROJECT,
                Command::MoveToProject(0, None),
            ),
            Item::new(
                "New connection in Billing…",
                icon::PLUS,
                Command::NewConnectionIn(Some("Billing".into())),
            ),
            Item::new(
                "Rename Billing…",
                icon::RENAME,
                Command::NameProject(Some(0)),
            ),
        ]);
        items.retain(|item| !waits_for_import(item.scope, &item.command));
        let left: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(left, ["Run query", "New connection…"]);
    }

    #[test]
    fn a_schema_prefix_narrows_to_one_schema() {
        assert_eq!(
            arranged(&sample(), Some(Scope::Objects), "pub ev", &[]),
            ["public.events"]
        );
    }
}

//! A connection and everything it owns: the profile, its session, and the tabs
//! and objects open inside it.
//!
//! The editor, results, explorer and query state live here rather than on
//! `Workspace` deliberately (spec §3.1). A profile is replaced wholesale when
//! the connection changes, so a buffer written against one database cannot be
//! retargeted at another -- it does not exist outside its profile.
//!
//! These were plain types at the crate root. They moved out whole; nothing
//! changed but their visibility.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{App, AppContext, Context, Entity, Window};
use gpui_component::{
    input::{EditorState, InputEvent, InputState},
    resizable::ResizableState,
    table::TableState,
    tree::TreeState,
};

use crate::{
    Workspace, completion,
    db::{
        CancelToken, Catalog, Connection, ConnectionConfig, DbError, Engine, ExplainMode, Relation,
        RelationKind, Routine, Structure,
    },
    explain::Plan,
    explorer::{ExplorerLeaf, ObjectKind},
    filter::{
        Conjunction, FilterBar, FilterRow, applied_filters, filter_bars, restored_filter,
        stored_filter,
    },
    result_grid,
    result_grid::{NewValue, ResultGrid},
    sql::{Destructive, Mode, SortKey, Verdict},
    store,
    theme::ConnectionColor,
    ui::row_readout,
};

/// A connection and everything it owns.
///
/// The editor, results, explorer and query state live here rather than on
/// `Workspace` deliberately (spec §3.1). A profile is replaced wholesale when
/// the connection changes, so a buffer written against one database cannot be
/// retargeted at another — it does not exist outside its profile.
pub(crate) struct Profile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) config: ConnectionConfig,
    pub(crate) color: Option<ConnectionColor>,
    pub(crate) mode: Mode,
    pub(crate) confirmed: Vec<Destructive>,
    /// Whether this connection has silenced the prompt before editing rows
    /// restored from an earlier session. Stored as [`STALE_ROWS`] among
    /// `confirmed`'s slugs, which a build without it drops as unknown.
    pub(crate) confirmed_stale: bool,
    pub(crate) generation: u64,
    pub(crate) state: ProfileState,
    pub(crate) catalog: CatalogState,
    pub(crate) session: Session,
}

impl Profile {
    pub(crate) fn connection(&self) -> Option<Connection> {
        match &self.state {
            ProfileState::Connected(connection) => Some((**connection).clone()),
            _ => None,
        }
    }

    pub(crate) fn stored(&self) -> store::StoredProfile {
        let engine = self.config.engine();
        // Tabs read back from disk that the catalog has not named yet are still
        // the truth about this profile: writing the live list instead would
        // drop every restored object the first time anything else is saved.
        let active = self.session.active;
        let mut open_objects = self
            .session
            .objects
            .iter()
            .map(|tab| store::StoredObject {
                active: active == Tab::Object(tab.id),
                ..tab.stored(engine)
            })
            .collect::<Vec<_>>();
        // Both lists, because a restore now opens the relations first and
        // leaves the routines pending: writing either one alone would drop the
        // other half of the session.
        open_objects.extend(self.session.pending_objects.iter().cloned());

        // A file engine writes no server fields and a server engine writes no
        // path, rather than either writing a blank the loader would have to
        // decide the meaning of.
        let server = self.config.server();
        let snowflake = match &self.config {
            ConnectionConfig::Snowflake(account) => Some(account),
            _ => None,
        };
        store::StoredProfile {
            id: self.id.clone(),
            name: self.name.clone(),
            host: server
                .map(|server| server.host.clone())
                .or_else(|| snowflake.and_then(|account| account.host.clone()))
                .unwrap_or_default(),
            port: server.and_then(|server| server.port),
            database: server
                .map(|server| server.database.clone())
                .or_else(|| snowflake.map(|account| account.database.clone()))
                .unwrap_or_default(),
            user: server
                .map(|server| server.user.clone())
                .or_else(|| snowflake.map(|account| account.user.clone()))
                .unwrap_or_default(),
            sslmode: server.map(|server| server.sslmode.as_str().to_string()),
            root_certificate: server.and_then(|server| server.root_certificate.clone()),
            engine: Some(self.config.engine().as_str().to_string()),
            path: match &self.config {
                ConnectionConfig::Sqlite { path, .. } => Some(path.clone()),
                _ => None,
            },
            account: snowflake.map(|account| account.account.clone()),
            private_key: snowflake.map(|account| account.private_key.clone()),
            warehouse: snowflake.and_then(|account| account.warehouse.clone()),
            role: snowflake.and_then(|account| account.role.clone()),
            // App-wide now, in `[settings]`. Kept on the stored shape and left
            // unwritten so the value an older build put here is still there for
            // the migration to read on the next upgrade.
            editor_font_size: None,
            statement_timeout: Some(self.config.statement_timeout()),
            next_query_id: Some(self.session.next_query_id),
            color: self.color.map(|color| color.slug().to_string()),
            mode: Some(self.mode.slug().to_string()),
            confirmed: self
                .confirmed
                .iter()
                .map(|kind| kind.slug())
                .chain(self.confirmed_stale.then_some(STALE_ROWS))
                .map(str::to_string)
                .collect(),
            // Nothing writes the legacy scalar any more; a buffer's name is a
            // property of its tab now. Kept on the stored shape only so a
            // profile written by an older build still loads with its buffer.
            open_query: None,
            ssh: server.and_then(|server| server.ssh.clone()),
            open_queries: self
                .session
                .queries
                .iter()
                .map(|tab| tab.stored(self.session.active == Tab::Query(tab.id)))
                .collect(),
            open_objects,
        }
    }
}

pub(crate) enum ProfileState {
    Idle,
    Connecting,
    // Boxed: a server profile's `ssh` tunnel made `Connection` far larger than
    // `Failed`'s `String`, and this variant is the one every idle profile pays
    // for.
    Connected(Box<Connection>),
    Failed(String),
}

/// The per-profile view state.
///
/// Separate from `Profile` only because GPUI entities need a `&mut Window` to
/// create, and the connection resolves on a task that has none — so this is
/// built before the spawn and moved in once the connection opens.
pub(crate) struct Session {
    /// The open query buffers, in strip order. Empty when the last one has been
    /// closed; `active` may then name a tab that no longer exists, which every
    /// lookup by id already answers with `None`.
    pub(crate) queries: Vec<QueryTab>,
    pub(crate) objects: Vec<ObjectTab>,
    pub(crate) active: Tab,
    /// The order the strip was last left in, by a drag or by a tab opening at
    /// its end. Chips it does not mention -- every tab restored at launch,
    /// until either happens -- follow it, in their default order.
    pub(crate) tab_order: Vec<TabKey>,
    pub(crate) next_query_id: u64,
    pub(crate) next_object_id: u64,
    /// Object tabs read back from disk, held until the catalog can name them.
    pub(crate) pending_objects: Vec<store::StoredObject>,
    /// What completion has learned about this connection's columns.
    ///
    /// Not in the catalog, deliberately: fetching every column of every
    /// relation at connect is unbounded work for a database nobody has asked a
    /// question about yet. A relation lands here the first time a statement
    /// names it, so what is held is bounded by what was written.
    pub(crate) completion_columns: completion::ColumnCache,
    pub(crate) explorer_filter: Entity<InputState>,
    pub(crate) explorer_tree: Entity<TreeState>,
    pub(crate) explorer_leaves: Arc<HashMap<String, ExplorerLeaf>>,
    /// `cmd+enter` reaches the workspace only through the focused element's
    /// dispatch path, so an unfocused editor makes the primary keystroke dead.
    pub(crate) editor_needs_focus: bool,
    /// The same hazard for the name field: an unfocused input asks for a name
    /// nobody can type into.
    pub(crate) save_name_needs_focus: bool,
    pub(crate) saved_queries: Vec<String>,
    /// The statements this profile has run, newest first. Held rather than read
    /// off disk when the palette opens, for the reason `saved_queries` is: the
    /// list is wanted while a list is being built, which is a frame.
    pub(crate) history: Vec<String>,
    pub(crate) save_name: Entity<InputState>,
    /// The page in front, and where to jump to once it is typed over: see
    /// `sync_page_input`. One field for the window, because only the relation
    /// in front can be paged.
    pub(crate) page_input: Entity<InputState>,
    /// The page last written into `page_input`, to tell a page that moved
    /// from one that is being typed over.
    pub(crate) page_shown: usize,
    pub(crate) naming: bool,
    pub(crate) pending_delete: Option<String>,
    /// The saved query `cmd+w` is asking about.
    ///
    /// A saved query has no closed state — it is in the strip while its file
    /// exists and gone when it does not — so closing its tab is deleting it,
    /// and it is the one tab that says so before it goes. Separate from
    /// `pending_delete`, which is the chip's own quieter two-click arming.
    pub(crate) pending_close: Option<String>,
    /// The close `cmd+w` is asking about, because the tab it names holds cell
    /// edits nobody has applied. Held as the target rather than the tab, so
    /// confirming runs exactly the close the keystroke had decided on --
    /// including a saved query's own second question.
    pub(crate) pending_discard: Option<CloseTarget>,
    pub(crate) notice: Option<String>,
    /// The generated batch a relation tab is showing before it runs. That tab
    /// has no buffer to put SQL in, so the modal is where the statement is on
    /// screen — and nothing runs until Run.
    pub(crate) apply_review: Option<ApplyReview>,
    /// The "New row" form a preview tab is filling in. Beside `apply_review`
    /// because the two are halves of one flow: the form collects, the review
    /// shows the statement it generated, and only Run sends it.
    pub(crate) insert_form: Option<InsertForm>,
    /// The statement the mode check stopped, held until the user answers.
    pub(crate) pending_run: Option<PendingRun>,
    /// The query tab whose queue stopped on a failed statement, held until
    /// the user says whether the rest of it still runs. Nothing after the
    /// failure is sent from here without an explicit Continue.
    pub(crate) queue_failure: Option<Tab>,
    /// The edit held until the user accepts that a restored grid's rows may
    /// be stale.
    pub(crate) stale_edit: Option<StaleEdit>,
    /// The structure request each relation tab is waiting on, by tab id.
    ///
    /// A refresh asks for the definition again, and nothing stops a second
    /// refresh starting while the first is in flight -- the engine reads the
    /// catalog in several queries, so the older request can finish last and
    /// put the older definition back. A completion that is not the newest
    /// issued for its tab is dropped.
    pub(crate) structure_requests: HashMap<u64, u64>,
}

/// A generated statement waiting to be read and run.
pub(crate) struct ApplyReview {
    /// The tab the edits came from, so the modal is shown over that surface
    /// and a run cannot land in another tab's grid.
    pub(crate) tab: Tab,
    /// What this panel is confirming. An `UPDATE` batch and an `INSERT` both
    /// arrive here, and a panel calling either of them "Apply edits" would be
    /// the confirmation lying about what it confirms.
    pub(crate) title: &'static str,
    pub(crate) sql: String,
}

/// The row a table has not got yet, one field per column (spec §4).
///
/// Insertion needs a schema and a table and no primary key, so this is offered
/// on relations the grid refuses to edit.
pub(crate) struct InsertForm {
    pub(crate) tab: Tab,
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) fields: Vec<InsertField>,
}

pub(crate) struct InsertField {
    pub(crate) column: String,
    pub(crate) data_type: String,
    pub(crate) input: Entity<InputState>,
    /// The `NULL` chip. Wins over typed text, because the chip is the later
    /// word.
    pub(crate) nulled: bool,
    /// Whether this field has been typed into. The reason `insert_value`
    /// answers three ways rather than two.
    pub(crate) touched: bool,
}

/// What one field contributes to the `INSERT`. The outer `None` is a column
/// left out of the statement entirely, so the server's default applies; the
/// inner `None` is a `NULL` the user asked for.
///
/// Three answers and not two: a field nobody touched and a field deliberately
/// emptied are different intents, and collapsing them would make `''`
/// unreachable from the form for every text column in the database.
pub(crate) fn insert_value(nulled: bool, touched: bool, typed: &str) -> Option<Option<String>> {
    match (nulled, touched) {
        (true, _) => Some(None),
        (false, true) => Some(Some(typed.to_string())),
        (false, false) => None,
    }
}

/// Everything `execute_and_then` needs to run a statement it was stopped from
/// running.
pub(crate) struct Resume {
    pub(crate) sql: String,
    pub(crate) tab: Tab,
    pub(crate) refresh: Option<Refresh>,
    pub(crate) keep_rows: bool,
    pub(crate) explain: Option<ExplainMode>,
}

/// The statement the mode check stopped, held until the user answers. Nothing
/// runs from here without an explicit Run.
pub(crate) struct PendingRun {
    /// `None` when the mode refused an inline edit rather than a statement --
    /// there is nothing to resume, and the dialog offers only the mode change.
    pub(crate) resume: Option<Resume>,
    pub(crate) verdict: Verdict,
    /// The "don't ask again" tick, which only the Confirm shape shows.
    pub(crate) dont_ask: bool,
}

/// The `confirmed` slug that silences [`StaleEdit`]'s prompt.
pub(crate) const STALE_ROWS: &str = "stale-rows";

/// An edit on a restored grid, stopped until the user answers whether to edit
/// rows fetched in an earlier session.
pub(crate) struct StaleEdit {
    /// The edit to carry on with, or `None` when the prompt was opened from
    /// the status bar only to refresh, with no edit waiting.
    pub(crate) resume: Option<StaleResume>,
    pub(crate) dont_ask: bool,
}

#[derive(Clone)]
pub(crate) enum StaleResume {
    Open,
    Stage(NewValue),
    Delete,
}

impl Session {
    pub(crate) fn new(
        id: String,
        stored_queries: Vec<store::StoredQueryTab>,
        stored_next_query_id: u64,
        pending_objects: Vec<store::StoredObject>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let explorer_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter database objects…"));
        cx.subscribe(&explorer_filter, {
            let id = id.clone();
            move |workspace, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    workspace.refresh_explorer(&id, cx);
                }
            }
        })
        .detach();

        let save_name = cx.new(|cx| InputState::new(window, cx).placeholder("Query name"));
        // Subscribed with the window, because confirming a save can swap the
        // editor's buffer and that cannot be done without one.
        cx.subscribe_in(
            &save_name,
            window,
            |workspace, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    workspace.confirm_save(window, cx);
                }
            },
        )
        .detach();

        let page_input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(
            &page_input,
            |workspace, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => workspace.go_to_page(cx),
                // A page typed and abandoned goes back to the one in front.
                InputEvent::Blur => cx.notify(),
                _ => {}
            },
        )
        .detach();

        let saved_queries = store::saved_queries(&id);
        // A tab naming a query whose file has gone comes back as the unsaved
        // buffer it now is, rather than as a tab pointing at nothing.
        let stored_queries = stored_queries
            .into_iter()
            .map(|mut stored| {
                stored.name = stored.name.filter(|name| saved_queries.contains(name));
                stored
            })
            .collect::<Vec<_>>();
        // No tab is a state a profile can be in: closing the last one leaves
        // the strip empty, and it comes back empty.
        let active = stored_queries
            .iter()
            .find(|stored| stored.active)
            .or(stored_queries.first())
            .map_or(0, |stored| stored.id);
        let next_query_id = next_query_id(stored_next_query_id, &stored_queries);

        let mut notice = None;
        let queries = stored_queries
            .iter()
            .map(|stored| {
                let (tab, failure) = QueryTab::restore(&id, stored, window, cx);
                notice = notice.take().or(failure);
                tab
            })
            .collect();

        Self {
            queries,
            objects: Vec::new(),
            active: Tab::Query(active),
            tab_order: Vec::new(),
            next_query_id,
            next_object_id: 0,
            pending_objects,
            completion_columns: completion::ColumnCache::default(),
            explorer_filter,
            explorer_tree: cx.new(|cx| TreeState::new(cx)),
            explorer_leaves: Arc::new(HashMap::new()),
            editor_needs_focus: true,
            save_name_needs_focus: false,
            saved_queries,
            history: store::history(&id),
            save_name,
            page_input,
            page_shown: 0,
            naming: false,
            pending_delete: None,
            pending_close: None,
            pending_discard: None,
            notice,
            apply_review: None,
            insert_form: None,
            pending_run: None,
            queue_failure: None,
            stale_edit: None,
            structure_requests: HashMap::new(),
        }
    }

    pub(crate) fn query_tab(&self, id: u64) -> Option<&QueryTab> {
        self.queries.iter().find(|tab| tab.id == id)
    }

    pub(crate) fn query_tab_mut(&mut self, id: u64) -> Option<&mut QueryTab> {
        self.queries.iter_mut().find(|tab| tab.id == id)
    }

    /// The query buffer in front, or `None` when an object tab is.
    pub(crate) fn active_query_tab(&self) -> Option<&QueryTab> {
        match self.active {
            Tab::Query(id) => self.query_tab(id),
            Tab::Object(_) => None,
        }
    }

    /// Every chip the strip draws, left to right: the dragged order first,
    /// then anything newer than it in the default one -- unsaved buffers, saved
    /// queries, objects.
    pub(crate) fn strip_order(&self) -> Vec<TabKey> {
        let chips: Vec<TabKey> = self
            .queries
            .iter()
            .filter(|tab| tab.open_query.is_none())
            .map(|tab| TabKey::Unsaved(tab.id))
            .chain(self.saved_queries.iter().cloned().map(TabKey::Saved))
            .chain(self.objects.iter().map(|tab| TabKey::Object(tab.id)))
            .collect();
        strip_order(chips, &self.tab_order)
    }

    /// Put a chip just opened at the right end of the strip as it is drawn.
    ///
    /// The whole order is written down first, not just the new chip: the tabs
    /// restored at launch are in no order but the default one, and pushing
    /// onto that would put the newest chip ahead of all of them.
    pub(crate) fn place_last(&mut self, key: TabKey) {
        self.tab_order = placed_last(self.strip_order(), key);
    }

    /// Give a chip its new key where it stands, for a buffer whose name is
    /// changing: saving or renaming it is not moving it.
    pub(crate) fn rekey(&mut self, from: &TabKey, to: TabKey) {
        let mut order = self.strip_order();
        if let Some(key) = order.iter_mut().find(|key| *key == from) {
            *key = to;
        }
        self.tab_order = order;
    }

    /// The tabs behind the strip's chips, left to right. A saved query with
    /// no buffer open is a chip with no tab, and is not here.
    pub(crate) fn strip_tabs(&self) -> Vec<Tab> {
        self.strip_order()
            .iter()
            .filter_map(|key| self.tab_of(key))
            .collect()
    }

    /// The tab that comes to the front when `closing` goes. Asked before it
    /// goes, while the strip still says where it stood, and brought forward
    /// through `activate_tab` as a click on its chip would be: a tab not
    /// looked at since launch has its snapshot read and its rows refreshed
    /// there and nowhere else.
    pub(crate) fn fallback(&self, closing: Tab) -> Option<Tab> {
        neighbour(&self.strip_tabs(), closing)
    }

    /// The tab a chip opens or shows, where it has one.
    pub(crate) fn tab_of(&self, key: &TabKey) -> Option<Tab> {
        match key {
            TabKey::Unsaved(id) => Some(Tab::Query(*id)),
            TabKey::Saved(name) => self.tab_holding(name).map(Tab::Query),
            TabKey::Object(id) => Some(Tab::Object(*id)),
        }
    }

    /// The tab a buffer holding `name` is in, if one is open.
    pub(crate) fn tab_holding(&self, name: &str) -> Option<u64> {
        self.queries
            .iter()
            .find(|tab| tab.open_query.as_deref() == Some(name))
            .map(|tab| tab.id)
    }

    /// The name of the buffer in front, when it has one.
    pub(crate) fn open_query(&self) -> Option<&str> {
        self.active_query_tab()
            .and_then(|tab| tab.open_query.as_deref())
    }

    pub(crate) fn active_object(&self) -> Option<&ObjectTab> {
        match self.active {
            Tab::Object(id) => self.objects.iter().find(|tab| tab.id == id),
            Tab::Query(_) => None,
        }
    }

    /// The query state behind the visible surface, or `None` for a surface that
    /// runs nothing — a routine is read, never executed by being opened.
    pub(crate) fn active_query(&self) -> Option<&QueryState> {
        match self.active_object() {
            None => self.active_query_tab().map(|tab| tab.shown().0),
            Some(tab) => match &tab.body {
                ObjectBody::Relation { query, .. } => Some(query),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// The grid the visible surface is showing. A routine's tab has none: it is
    /// read, not run.
    ///
    /// A queue's selected result, not the tab's own slot, because this is what
    /// copying, exporting and editing act on and all three mean the grid in
    /// front of the user. `slot` is the other half of that split: it is where a
    /// run's rows land, which stays the tab's slot whatever is being read.
    pub(crate) fn active_results(&self) -> Option<&Entity<TableState<ResultGrid>>> {
        match self.active_object() {
            None => self.active_query_tab().map(|tab| tab.shown().1),
            Some(tab) => match &tab.body {
                ObjectBody::Relation { results, .. } => Some(results),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// The buffer a run reads from, which only the query tab has. An object tab
    /// shows an object: there is no SQL in front of the user to run.
    pub(crate) fn editor(&self, tab: Tab) -> Option<Entity<EditorState>> {
        match tab {
            Tab::Query(id) => self.query_tab(id).map(|tab| tab.editor.clone()),
            Tab::Object(_) => None,
        }
    }

    /// The grid one named tab is showing. `slot` answers the same question but
    /// needs a `&mut`, and asking what a tab holds changes nothing.
    pub(crate) fn results(&self, tab: Tab) -> Option<&Entity<TableState<ResultGrid>>> {
        match tab {
            Tab::Query(id) => self.query_tab(id).map(|tab| &tab.results),
            Tab::Object(id) => match &self.objects.iter().find(|tab| tab.id == id)?.body {
                ObjectBody::Relation { results, .. } => Some(results),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// Every live grid this session holds, across both tab strips. `results`
    /// and `slot` reach one grid by tab; this reaches all of them, for a
    /// setting that belongs to the connection rather than to a run --
    /// `Workspace::set_mode` is the caller.
    /// A queue's finished results are in here too: they stay on screen through
    /// the switcher, so a mode that did not reach them would leave an older
    /// result editable after the connection stopped allowing it.
    pub(crate) fn grids(&self) -> impl Iterator<Item = &Entity<TableState<ResultGrid>>> {
        self.queries
            .iter()
            .flat_map(|tab| {
                std::iter::once(&tab.results).chain(
                    tab.queue
                        .iter()
                        .flat_map(|queue| queue.done.iter().map(|finished| &finished.grid)),
                )
            })
            .chain(self.objects.iter().filter_map(|tab| match &tab.body {
                ObjectBody::Relation { results, .. } => Some(results),
                ObjectBody::Routine(_) => None,
            }))
    }

    /// Where a run's state and rows belong. Returning both together is what
    /// keeps a result from landing in one tab's grid with another tab's status.
    pub(crate) fn slot(
        &mut self,
        tab: Tab,
    ) -> Option<(&mut QueryState, Entity<TableState<ResultGrid>>)> {
        match tab {
            Tab::Query(id) => {
                let tab = self.query_tab_mut(id)?;
                let results = tab.results.clone();
                Some((&mut tab.query, results))
            }
            Tab::Object(id) => match &mut self.objects.iter_mut().find(|tab| tab.id == id)?.body {
                ObjectBody::Relation { query, results, .. } => Some((query, results.clone())),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// Drop every confirmation and half-finished prompt this session is holding.
    ///
    /// All four name the buffer or tab they were raised over, so leaving one
    /// standing across a context change offers to delete one query while another
    /// is on screen. One method rather than a clear at each site, because the
    /// two callers had drifted apart: switching profiles left `naming` set with
    /// `save_name_needs_focus` already spent, which renders the name prompt and
    /// then hands focus to nothing at all — and a window with nothing focused
    /// has no dispatch path, so the keyboard goes dead until something is
    /// clicked.
    pub(crate) fn clear_prompts(&mut self) {
        self.pending_delete = None;
        self.pending_close = None;
        self.pending_discard = None;
        self.naming = false;
        self.save_name_needs_focus = false;
        // A stopped statement must not survive a tab or profile switch and get
        // confirmed against a connection it was never aimed at.
        self.pending_run = None;
        self.stale_edit = None;
    }
}

/// What takes focus when a surface comes to the front. A buffer, a field and a
/// grid are all focusable and no two of them share a type.
pub(crate) enum Focus {
    Buffer(Entity<EditorState>),
    /// A single-line field — the save-name prompt. Not an [`EditorState`]:
    /// 0.6.4 splits the code editor off from the plain input.
    Field(Entity<InputState>),
    Grid(Entity<TableState<ResultGrid>>),
    /// The window itself, for a surface with nothing in it to type into. Not a
    /// no-op: a keystroke only reaches the workspace along the focused
    /// element's dispatch path, so focusing nothing at all is what makes every
    /// binding dead until something is clicked.
    Window,
}

pub(crate) enum CatalogState {
    Loading,
    /// The relations, and how far the routines behind them have got.
    Loaded(Catalog, Routines),
    Failed(String),
}

/// The second half of a loaded catalog. A catalog whose routines are not
/// `Loaded` holds none, and a routine missing from it says nothing about the
/// database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Routines {
    Loading,
    Loaded,
    Failed,
}

/// Which surface the main pane is showing, and what a run targets. Both kinds
/// of tab are addressed by id rather than by index, so closing one cannot land
/// an in-flight result in its neighbour's grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Query(u64),
    Object(u64),
}

impl Tab {
    /// A key unique to this tab in this profile, for scroll state
    /// (`scroller::smooth_scoped`) that must not bleed into another tab or
    /// profile reusing the same id -- `next_query_id` and `next_object_id`
    /// each count from zero per profile, so a bare id collides across both
    /// kinds and across profiles.
    pub(crate) fn scroll_scope(&self, profile_id: &str) -> String {
        format!("{profile_id}-{self:?}")
    }
}

/// A chip of the tab strip, which is not the same set as `Tab`: a saved query
/// is listed whether or not a buffer holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TabKey {
    Unsaved(u64),
    Saved(String),
    Object(u64),
}

/// What `cmd+w` has to do with the surface in front of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    /// Close it. It is a view onto something the database still holds, and
    /// reopening it costs a click.
    Object(u64),
    /// Ask first. A saved query is listed while its file exists and gone when
    /// it does not, so closing its tab is deleting it.
    SavedQuery(String),
    /// Close it. An unsaved buffer is a scratch pad someone is done with; its text goes with it, which is what closing an
    /// unnamed buffer means everywhere else.
    Buffer(u64),
}

impl CloseTarget {
    /// The tab this close takes out of the strip. A saved query names its file
    /// rather than its tab, so the strip is what says which tab that is -- and
    /// `None` is a saved query with no tab open, which no close comes from.
    pub(crate) fn tab(&self, session: &Session) -> Option<Tab> {
        match self {
            Self::Object(id) => Some(Tab::Object(*id)),
            Self::Buffer(id) => Some(Tab::Query(*id)),
            Self::SavedQuery(name) => session.tab_holding(name).map(Tab::Query),
        }
    }
}

/// `chips` in default order, rearranged by the order the strip was last left
/// in. A chip that order does not mention follows it.
fn strip_order(mut chips: Vec<TabKey>, order: &[TabKey]) -> Vec<TabKey> {
    let mut ordered: Vec<TabKey> = order
        .iter()
        .filter(|key| chips.contains(key))
        .cloned()
        .collect();
    chips.retain(|key| !ordered.contains(key));
    ordered.extend(chips);
    ordered
}

fn placed_last(mut order: Vec<TabKey>, key: TabKey) -> Vec<TabKey> {
    order.retain(|candidate| *candidate != key);
    order.push(key);
    order
}

/// The tab beside `closing` on the left, or on the right when it was the
/// first: the chip the eye is already next to.
fn neighbour(tabs: &[Tab], closing: Tab) -> Option<Tab> {
    let at = tabs.iter().position(|tab| *tab == closing)?;
    at.checked_sub(1)
        .and_then(|left| tabs.get(left))
        .or(tabs.get(at + 1))
        .copied()
}

pub(crate) fn close_target(active: Tab, open_query: Option<&str>) -> CloseTarget {
    match (active, open_query) {
        (Tab::Object(id), _) => CloseTarget::Object(id),
        (Tab::Query(_), Some(name)) => CloseTarget::SavedQuery(name.to_string()),
        (Tab::Query(id), None) => CloseTarget::Buffer(id),
    }
}

/// One query buffer, and everything that belongs to it.
///
/// There used to be exactly one of these per profile, held directly on
/// `Session`, and opening a saved query swapped its text in place. That is why
/// `cmd+t` on a dirty scratch buffer persisted it and then cleared it: there
/// was nowhere else for it to be. A buffer is a tab now, the same way an object
/// is, and for the same reason -- addressed by id, so closing one cannot land
/// another's result in its grid.
pub(crate) struct QueryTab {
    pub(crate) id: u64,
    pub(crate) editor: Entity<EditorState>,
    pub(crate) results: Entity<TableState<ResultGrid>>,
    pub(crate) query: QueryState,
    /// The saved query this buffer holds, or `None` while it is unsaved.
    ///
    /// It is also which file the buffer persists to: a name means the query
    /// file, no name means this tab's own scratch file.
    pub(crate) open_query: Option<String>,
    /// The statement behind this tab's grid.
    ///
    /// Held rather than derived from the buffer, unlike the sort path, and a
    /// deliberate exception to that rule (in-grid editing spec, §4): applying
    /// edits appends the `UPDATE` to the buffer, so the cursor no longer sits on
    /// the `SELECT` and the text can no longer say where these rows came from.
    pub(crate) last_query: Option<String>,
    /// Where in the buffer the statement last sent from it began, and its
    /// text. Taken by the run it was sent for once that run starts, so a run
    /// the mode gate stopped, or one refused while another was in flight,
    /// cannot re-label the error already on screen.
    pub(crate) sent_from: Option<(usize, String)>,
    /// The same for the statement behind `query`: what places an error's
    /// position in the buffer. `None` when the buffer did not supply it, as
    /// for a grid edit's or a sort's.
    pub(crate) ran_from: Option<(usize, String)>,
    /// Whether this tab's snapshot has been looked for yet. Set on the first
    /// attempt whether or not one was found, so a tab reached a second time
    /// cannot read the disk again and put stale rows over live ones.
    pub(crate) hydrated: bool,
    /// The last plan this tab asked for, or `None` until it asks for one.
    ///
    /// Kept beside the grid rather than over it, so flipping to the plan and
    /// back does not cost a re-run of either. A failed `EXPLAIN` is not here:
    /// it is a `QueryState::Failed` like any other, shown where every other
    /// statement's error is shown.
    pub(crate) plan: Option<Explained>,
    /// Which of the two the results pane is showing. Deliberately not derived
    /// from `plan.is_some()`: a plan that has been read and flipped away from
    /// is still worth keeping to flip back to.
    pub(crate) showing_plan: bool,
    /// The multi-statement run this tab is part way through, or `None` for an
    /// ordinary single-statement run. `query` and `results` stay what they
    /// always were -- the slot being run or shown right now -- and the
    /// statements already finished are in `queue.done`.
    pub(crate) queue: Option<Queue>,
    /// How many queued results this tab was written with, until it is
    /// hydrated and `queue` holds them. Taken by `hydrate_tab`, so a tab the
    /// user never looks at still reports what is on disk and keeps the prune
    /// off its files.
    pub(crate) queued_results: usize,
    /// Whether this tab's row-inspector panel is folded away. Per tab, like
    /// the panel itself (see `RowPanel`), and not persisted.
    pub(crate) row_panel_folded: bool,
    /// The row-inspector split's state, per tab: a width dragged to in one
    /// tab must not resize another's. Not persisted -- a fresh tab always
    /// starts at the built-in default.
    pub(crate) row_panel_split: Entity<ResizableState>,
}

/// A plan, and what it is a plan of.
pub(crate) struct Explained {
    pub(crate) plan: Plan,
    pub(crate) mode: ExplainMode,
    /// The statement that was explained. Held because it is not necessarily
    /// what the buffer says any more -- the user is free to keep typing, and a
    /// plan that silently re-labels itself against edited text would be
    /// describing a statement nobody ran.
    pub(crate) sql: String,
}

/// The statements a selection covers, run one at a time, and what each has
/// produced so far.
///
/// The buffer text is held rather than read back when a statement is sent:
/// the user is free to keep typing while the queue runs, and every range in
/// `remaining` is an offset into the text as it was when Run was
/// pressed.
pub(crate) struct Queue {
    pub(crate) sql: String,
    /// Statements not yet sent, in order.
    pub(crate) remaining: Vec<Range<usize>>,
    /// One per statement already run, in order.
    pub(crate) done: Vec<Finished>,
    /// Which of `done` is the result on screen, or `done.len()` for the
    /// statement still in flight, whose result is in the tab's own slot.
    ///
    /// A queue's results are read from `done` rather than swapped into the
    /// tab's `query`/`results`, so the slot a run writes to is never the slot
    /// the user is looking at. That is the whole reason an earlier result can
    /// be read while a later statement is still running.
    pub(crate) showing: usize,
    /// Whether a statement this queue sent is still out.
    ///
    /// Load-bearing: a finished queue stays on the tab so its results can be
    /// switched between, and every completion on that tab reaches the queue.
    /// Without this an ordinary Run, a header sort or a grid edit on such a
    /// tab would land in `done` as another of the queue's statements.
    pub(crate) awaiting: bool,
    /// One empty grid per statement still to run, built when the run started.
    ///
    /// Every statement needs its own, or the result stored in `done` is
    /// overwritten by the statement after it -- and the completion that lands
    /// a result runs on the executor, where there is no `Window` to build one
    /// with. ponytail: the whole queue's grids are allocated up front, which
    /// is the same count they reach anyway; take a window into
    /// `execute_unchecked` if one ever needs to be built later than this.
    pub(crate) spare: Vec<Entity<TableState<ResultGrid>>>,
}

/// One statement of a queue that has run, with the result it produced.
pub(crate) struct Finished {
    /// The statement that produced this result, which is both what the chip is
    /// labelled from and what the snapshot carries. Held rather than sliced out
    /// of [`Queue::sql`] on demand because a queue restored from disk has no
    /// text to slice: the statements are gone and the results are not.
    pub(crate) sql: String,
    /// Where the statement begins in [`Queue::sql`].
    pub(crate) start: usize,
    pub(crate) state: QueryState,
    pub(crate) grid: Entity<TableState<ResultGrid>>,
}

const QUERY_LABEL_LIMIT: usize = 32;

/// A statement as a switcher chip reads it: whitespace collapsed, clipped to
/// what a chip can hold.
pub(crate) fn query_label(statement: &str) -> String {
    let flat = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(QUERY_LABEL_LIMIT) {
        Some((end, _)) => format!("{}…", &flat[..end]),
        None => flat,
    }
}

/// What a finished statement leaves a queue to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Next(Range<usize>),
    /// The statement failed with more to run, so the user decides whether the
    /// rest still runs.
    Ask,
    Finished,
}

/// Split out of the completion handler so the one branch the queue turns on is
/// checkable without a window.
pub(crate) fn next_step(failed: bool, remaining: &[Range<usize>]) -> Step {
    match remaining.first() {
        // Nothing follows the failure, so there is nothing to decide.
        None => Step::Finished,
        Some(_) if failed => Step::Ask,
        Some(next) => Step::Next(next.clone()),
    }
}

impl QueryTab {
    /// The result on screen: whichever of a queue's finished statements the
    /// switcher has selected, else the tab's own slot, which is what a run
    /// writes into.
    pub(crate) fn shown(&self) -> (&QueryState, &Entity<TableState<ResultGrid>>) {
        match self
            .queue
            .as_ref()
            .and_then(|queue| queue.done.get(queue.showing))
        {
            Some(finished) => (&finished.state, &finished.grid),
            None => (&self.query, &self.results),
        }
    }

    /// Read one stored buffer back off disk.
    ///
    /// Returns the message rather than reporting it: several tabs are restored
    /// at once and the session shows one notice, so the caller decides which
    /// failure is the one worth saying. A buffer that could not be read is
    /// empty either way, and silence would make it look like it never held
    /// anything.
    pub(crate) fn restore(
        profile_id: &str,
        stored: &store::StoredQueryTab,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> (Self, Option<String>) {
        let text = match &stored.name {
            Some(name) => store::read_query(profile_id, name),
            None => store::read_scratch(profile_id, stored.id),
        };
        let (sql, notice) = match text {
            Ok(sql) => (sql.unwrap_or_default(), None),
            Err(message) => (String::new(), Some(message)),
        };

        let tab = Self {
            id: stored.id,
            editor: cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("sql")
                    .soft_wrap(false)
                    .placeholder("Write SQL…")
                    .default_value(sql)
            }),
            results: result_grid::new_grid(window, cx),
            query: QueryState::Idle,
            open_query: stored.name.clone(),
            last_query: None,
            sent_from: None,
            ran_from: None,
            hydrated: false,
            plan: None,
            // A restored queue is rebuilt in `hydrate_tab`, which has the
            // window its grids need; it is never part way through, since
            // nothing of a run in flight is kept.
            queue: None,
            queued_results: stored.queued_results,
            showing_plan: false,
            row_panel_folded: false,
            row_panel_split: cx.new(|_| ResizableState::default()),
        };
        (tab, notice)
    }

    pub(crate) fn stored(&self, active: bool) -> store::StoredQueryTab {
        store::StoredQueryTab {
            id: self.id,
            name: self.open_query.clone(),
            active,
            // The live queue is the truth once there is one; until the tab is
            // hydrated, what was written last time is.
            queued_results: self
                .queue
                .as_ref()
                .map_or(self.queued_results, |queue| queue.done.len()),
        }
    }
}

/// An opened database object. It stays in the tab strip until it is closed, so
/// coming back to a table does not mean finding it in the explorer again.
pub(crate) struct ObjectTab {
    pub(crate) id: u64,
    pub(crate) schema: String,
    /// A relation's name, or a routine's name with its argument types — which
    /// is the only thing that tells two overloads of one function apart.
    pub(crate) name: String,
    pub(crate) kind: ObjectKind,
    pub(crate) body: ObjectBody,
}

impl ObjectTab {
    /// The `WHERE` this tab reads the relation under, and `""` for a routine
    /// and for an unfiltered relation -- which is what makes an unfiltered tab
    /// dedupe exactly as it did before the filter joined the key.
    pub(crate) fn filter(&self) -> &str {
        match &self.body {
            ObjectBody::Relation { filter, .. } => filter,
            ObjectBody::Routine(_) => "",
        }
    }

    /// The bars this tab's `WHERE` was derived from, the ones that narrow
    /// something only.
    pub(crate) fn filters(&self, engine: Engine) -> Vec<store::StoredFilter> {
        match &self.body {
            ObjectBody::Relation { filters, .. } => applied_filters(engine, &filter_bars(filters))
                .iter()
                .map(stored_filter)
                .collect(),
            ObjectBody::Routine(_) => Vec::new(),
        }
    }

    pub(crate) fn stored(&self, engine: Engine) -> store::StoredObject {
        store::StoredObject {
            schema: self.schema.clone(),
            name: self.name.clone(),
            filter: self.filter().to_string(),
            filter_engine: (!self.filter().is_empty()).then(|| engine.as_str().to_string()),
            // Nothing writes the two-field rows an older build did; they are
            // read once on the way in and superseded by `bars` on this save.
            filters: Vec::new(),
            bars: self.filters(engine),
            routine: matches!(self.kind, ObjectKind::Routine(_)),
            kind: match self.kind {
                ObjectKind::Relation(kind) => kind,
                ObjectKind::Routine(_) => RelationKind::default(),
            },
            active: false,
        }
    }
}

/// What the explorer -- or a session read back from disk -- hands over to open
/// a tab. A routine arrives whole, because its body is already in the catalog.
pub(crate) enum OpenedObject {
    Relation {
        schema: String,
        name: String,
        kind: RelationKind,
        /// The `WHERE` the tab opens under, empty for the whole relation. Part
        /// of the tab's identity (spec §6.3), so following a key opens a tab
        /// beside the relation's own rather than taking it over.
        filter: String,
        /// The bars `filter` was derived from, so the tab opens with the
        /// controls that produced it rather than with an expression nothing can
        /// edit. Derived and expression travel together for the length of the
        /// open: nothing parses one back into the other.
        filters: Vec<FilterBar>,
    },
    Routine {
        schema: String,
        routine: Routine,
    },
}

impl OpenedObject {
    pub(crate) fn schema(&self) -> &str {
        match self {
            Self::Relation { schema, .. } | Self::Routine { schema, .. } => schema,
        }
    }

    pub(crate) fn name(&self) -> String {
        match self {
            Self::Relation { name, .. } => name.clone(),
            Self::Routine { routine, .. } => routine_name(routine),
        }
    }

    pub(crate) fn kind(&self) -> ObjectKind {
        match self {
            Self::Relation { kind, .. } => ObjectKind::Relation(*kind),
            Self::Routine { routine, .. } => ObjectKind::Routine(routine.kind),
        }
    }

    pub(crate) fn filter(&self) -> &str {
        match self {
            Self::Relation { filter, .. } => filter,
            Self::Routine { .. } => "",
        }
    }

    pub(crate) fn resolve(
        engine: Engine,
        catalog: &Catalog,
        stored: &store::StoredObject,
    ) -> Option<Self> {
        let schema = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == stored.schema)?;
        if stored.routine {
            let routine = schema
                .routines
                .iter()
                .find(|routine| routine_name(routine) == stored.name)?;
            Some(Self::Routine {
                schema: schema.name.clone(),
                routine: routine.clone(),
            })
        } else {
            let relation = schema
                .relations
                .iter()
                .find(|relation| relation.name == stored.name)?;
            let (filter, filters) = restored_filter(engine, stored);
            Some(Self::Relation {
                schema: schema.name.clone(),
                name: relation.name.clone(),
                kind: relation.kind,
                filter,
                filters,
            })
        }
    }
}

/// The tab an object would reuse, if it has one. Dedup is on
/// `(schema, name, filter)` rather than `(schema, name)` (spec §6.3), so
/// `customers` and `customers WHERE id = 42` are two tabs. Fed an iterator
/// rather than a session, so the invariant has a test that needs no window.
pub(crate) fn matching_tab<'a>(
    open: impl Iterator<Item = (u64, &'a str, &'a str, &'a str)>,
    schema: &str,
    name: &str,
    filter: &str,
) -> Option<u64> {
    open.filter(|(_, candidate_schema, candidate_name, candidate_filter)| {
        *candidate_schema == schema && *candidate_name == name && *candidate_filter == filter
    })
    .map(|(id, ..)| id)
    .next()
}

/// What the catalog says a relation is. The only authority on it: a stored
/// tab's kind is a cache of this, and can be a default rather than a kind.
pub(crate) fn relation_kind(catalog: &Catalog, schema: &str, name: &str) -> Option<RelationKind> {
    catalog_relation(catalog, schema, name).map(|relation| relation.kind)
}

pub(crate) fn catalog_relation<'a>(
    catalog: &'a Catalog,
    schema: &str,
    name: &str,
) -> Option<&'a Relation> {
    catalog
        .schemas
        .iter()
        .find(|candidate| candidate.name == schema)?
        .relations
        .iter()
        .find(|relation| relation.name == name)
}

/// A routine's name carries its argument types, because a schema can hold
/// several routines with the same name and nothing else to tell them apart.
pub(crate) fn routine_name(routine: &Routine) -> String {
    format!("{}({})", routine.name, routine.identity_arguments)
}

// One per open tab, and a routine tab is the rarity: boxing the relation's
// fields would put an indirection on every one of them to save a few hundred
// bytes per tab.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ObjectBody {
    /// An opened relation: its rows, full height, with the relation's
    /// definition behind the Structure toggle (spec §3.2).
    ///
    /// No editor. A generated `SELECT` shown above the grid read as a query the
    /// user had written and invited edits to a buffer that then stopped being a
    /// view of the relation at all. The SQL dbdelve runs here is its own, and the
    /// only thing the user changes about it is the sort.
    Relation {
        showing_structure: bool,
        structure: StructureState,
        results: Entity<TableState<ResultGrid>>,
        query: QueryState,
        /// The `ORDER BY` the header clicks have built up. dbdelve owns this
        /// statement, so sorting regenerates it rather than editing text.
        sort: Vec<SortKey>,
        /// The `WHERE` expression this preview narrows the relation by, without
        /// the keyword; empty means none. The fifth control of the same kind as
        /// the sort, the limit and the offset: a change regenerates the
        /// statement rather than patching it. Unlike `offset`, it persists in
        /// the tab's snapshot — a tab that forgot its filter would come back as
        /// a different tab.
        ///
        /// Derived from `filters` on every apply, never edited directly: it is
        /// what runs, what keys the snapshot and what the tab strip shows.
        filter: String,
        /// The filter bars, stacked above the grid, which are the editable
        /// state (spec §2.4). Per tab, because the filter is.
        filters: Vec<FilterRow>,
        /// The joiner the next bar added will carry, shown on the "Add filter"
        /// row. Not persisted: it is a choice about a bar that does not exist
        /// yet, and a restart that forgot it has forgotten nothing.
        next_join: Conjunction,
        /// How many rows this preview asks for. Every result set is capped
        /// (spec §4.3); this is the tab's own copy of the cap, so raising it
        /// for one wide table does not raise it everywhere.
        limit: usize,
        /// How far into the relation this preview's page starts, in rows.
        /// Always a multiple of `limit`: paging moves it by one page, and a
        /// change of sort or limit puts it back to zero, because a window into
        /// an ordering that no longer exists is not a page of anything.
        offset: usize,
        /// Whether the rows on screen are owed a refresh that keeps them on
        /// screen until it lands: rows that came off disk or over a connection
        /// since replaced, which are refreshed on the tab's next activation
        /// rather than at startup or reconnect -- a session of restored tabs
        /// would otherwise open by firing one query per tab at a database
        /// nobody has looked at yet -- and rows a refresh has just been asked
        /// for. Spent by the run that refreshes them, or by the refusal of one
        /// that never could.
        stale: bool,
        /// Whether this tab's snapshot has been looked for yet. See
        /// [`QueryTab::hydrated`].
        hydrated: bool,
        /// Whether this tab's row-inspector panel is folded away. Per tab:
        /// see `RowPanel`.
        row_panel_folded: bool,
        /// The row-inspector split's state, per tab. See
        /// [`QueryTab::row_panel_split`].
        row_panel_split: Entity<ResizableState>,
        /// `COUNT(*)` under the current `filter`, run only when the user asks:
        /// on a large table it is a full scan holding the connection.
        count: RowCount,
    },
    Routine(Routine),
}

/// A relation's row count, each state carrying the filter it was asked under so
/// that an answer for an older filter is never shown under a newer one.
pub(crate) enum RowCount {
    Unasked,
    /// In flight. `started` tells this run's answer from an earlier one's, and
    /// `cancelling` says a Cancel has gone out, as `QueryState::Running`'s does.
    Counting {
        filter: String,
        started: std::time::Instant,
        cancel: CancelToken,
        cancelling: bool,
    },
    Counted(String, u64),
}

impl RowCount {
    pub(crate) fn answers(&self, filter: &str) -> bool {
        match self {
            Self::Unasked => false,
            Self::Counting { filter: asked, .. } | Self::Counted(asked, _) => asked == filter,
        }
    }
}

/// What the status bar says a relation tab's relation holds, or `None` to leave
/// it to the rows on screen: a count the user asked for under the current
/// filter, else the catalog's estimate for an unfiltered tab.
///
/// No estimate when the first page came back short (`whole`), since that page
/// is the whole relation and its own readout is exact; and none of zero unless
/// it is `exact`, because zero is what a table never analyzed reads as on
/// several engines, and claiming an empty table is the misleading way to be
/// wrong.
pub(crate) fn relation_rows(
    count: &RowCount,
    filter: &str,
    estimate: Option<u64>,
    exact: bool,
    whole: bool,
) -> Option<String> {
    if let RowCount::Counted(asked, rows) = count
        && asked == filter
    {
        return Some(row_readout(*rows as usize, *rows as usize));
    }
    let rows = estimate.filter(|_| filter.trim().is_empty() && !whole)? as usize;
    match exact {
        true => Some(row_readout(rows, rows)),
        false => (rows > 0).then(|| format!("\u{2248}{}", row_readout(rows, rows))),
    }
}

pub(crate) enum StructureState {
    Loading,
    Loaded(Structure),
    Failed(String),
}

/// Put a snapshot's rows on screen.
pub(crate) fn show_snapshot(
    results: &Entity<TableState<ResultGrid>>,
    grid: &store::StoredGrid,
    mode: Mode,
    engine: Engine,
    cx: &mut Context<Workspace>,
) {
    results.update(cx, |table, cx| {
        *table.delegate_mut() = ResultGrid::restored(grid, mode).with_engine(engine);
        table.refresh(cx);
    });
}

/// The state a tab restored from a snapshot is in.
///
/// `Complete` rather than `Idle`, for two reasons: the rows are a result and
/// the status bar has to be able to count them, and it is what makes
/// `load_relation` leave a restored tab's rows alone instead of re-querying
/// them the moment the tab is reached. The cost fields are zero because a
/// snapshot knows none of them -- it is not the run, it is what the run left --
/// and the status readout says "snapshot" rather than reporting the zeroes.
///
/// `rows` is the result's own size, which can be larger than the rows the grid
/// holds: a snapshot is capped. `row_readout` is what says so, and
/// `export_results` refuses rather than writing a short file.
pub(crate) fn restored_state(grid: &store::StoredGrid) -> QueryState {
    QueryState::Complete {
        rows: grid.total_rows,
        bytes: 0,
        elapsed: std::time::Duration::ZERO,
        rows_affected: None,
    }
}

/// What runs once a generated batch has succeeded.
pub(crate) enum Refresh {
    /// The query tab's stashed `SELECT`.
    Statement(String),
    /// A relation tab, refreshed the way its own controls refresh it — so it
    /// picks up whatever sort and row limit the tab is now set to.
    Relation(u64),
}

/// `Clone` because a queue keeps the state of every statement it has run
/// beside the one on screen; the clone is a finished slot, never a `Running`
/// one.
#[derive(Clone)]
pub(crate) enum QueryState {
    Idle,
    /// `cancelling` says when a cancel was *sent* for this slot, and nothing
    /// more: the statement is still in flight, so this is still `Running` to
    /// everything that asks. It leaves the flag behind when it leaves the
    /// variant, which is why nothing resets it. `cancel` is what the run went
    /// out under, and all a cancel for this slot may stop.
    Running {
        started: std::time::Instant,
        cancelling: Option<std::time::Instant>,
        cancel: CancelToken,
    },
    Complete {
        rows: usize,
        bytes: usize,
        elapsed: std::time::Duration,
        rows_affected: Option<u64>,
    },
    /// An `EXPLAIN` came back. Its own variant rather than a `Complete` because
    /// the rows it returned are not results and never reach the grid: the grid
    /// still holds whatever the last real run put there, and a readout claiming
    /// this many rows had just arrived would be describing the plan while
    /// pointing at someone else's data.
    Explained {
        elapsed: std::time::Duration,
        mode: ExplainMode,
    },
    Failed(DbError),
}

/// Write every one of a profile's buffers back to whichever file it came from.
///
/// All of them rather than the one in front: a buffer that is not visible is
/// still someone's unsaved work, and a tab switch is no longer the moment it
/// gets written. The first failure is the one reported and the rest are still
/// attempted — a full disk must not cost more buffers than it has to.
pub(crate) fn write_buffer(profile: &Profile, cx: &App) -> Result<(), String> {
    let mut failure = None;
    for tab in &profile.session.queries {
        let sql = tab.editor.read(cx).value().to_string();
        let written = match &tab.open_query {
            Some(name) => store::write_query(&profile.id, name, &sql),
            None => store::write_scratch(&profile.id, tab.id, &sql),
        };
        if let Err(message) = written {
            failure = failure.or(Some(message));
        }
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Snapshot every tab's grid, so reopening a profile shows the rows it was
/// showing rather than an empty grid waiting on a re-run.
///
/// Only a `Complete` tab is written: an empty or failed grid is not a result,
/// and writing one would replace a good snapshot with nothing. Failures are
/// dropped rather than reported, unlike the buffers this runs beside -- a cache
/// that did not land costs a re-run, not somebody's unsaved work.
///
/// A queue's results are written one per key beside the tab's own, each
/// carrying its statement as `last_query`, which is what a restored chip is
/// labelled from. They do not wait on the tab's own state: a statement that
/// finished is a result whether or not the one after it is still running.
pub(crate) fn write_grids(profile: &Profile, cx: &App) {
    for tab in &profile.session.queries {
        for (index, finished) in tab
            .queue
            .iter()
            .flat_map(|queue| queue.done.iter().enumerate())
        {
            let grid = finished.grid.read(cx).delegate().stored();
            if grid.columns.is_empty() {
                continue;
            }
            let _ = store::write_grid(
                &profile.id,
                &store::queued_grid_key(tab.id, index),
                &store::StoredGrid {
                    last_query: Some(finished.sql.clone()),
                    ..grid
                },
            );
        }
        if !matches!(tab.query, QueryState::Complete { .. }) {
            continue;
        }
        let grid = tab.results.read(cx).delegate().stored();
        // A statement that returned no columns produced no grid to keep --
        // which is every `UPDATE` and `DELETE` the buffer has run.
        if grid.columns.is_empty() {
            continue;
        }
        let _ = store::write_grid(
            &profile.id,
            &store::query_grid_key(tab.id),
            &store::StoredGrid {
                last_query: tab.last_query.clone(),
                ..grid
            },
        );
    }

    for tab in &profile.session.objects {
        let ObjectBody::Relation {
            results,
            query,
            sort,
            filter,
            limit,
            showing_structure,
            ..
        } = &tab.body
        else {
            continue;
        };
        if !matches!(query, QueryState::Complete { .. }) {
            continue;
        }
        let grid = results.read(cx).delegate().stored();
        if grid.columns.is_empty() {
            continue;
        }
        let _ = store::write_grid(
            &profile.id,
            &store::object_grid_key(&tab.schema, &tab.name, filter),
            &store::StoredGrid {
                limit: Some(*limit),
                filter: filter.clone(),
                showing_structure: *showing_structure,
                order_by: sort
                    .iter()
                    .map(|key| (key.expression.clone(), key.ascending))
                    .collect(),
                ..grid
            },
        );
    }
}

/// The id the next new buffer gets.
///
/// `stored` is what the profile last wrote, and the maximum over the open tabs
/// is the floor: a profile written before the field was kept has none, and one
/// written by a build that derived it could hand out an id a tab already holds.
/// Never the derived value alone -- that decreases when the highest tab closes,
/// and the reused id would hydrate the closed tab's snapshot.
pub(crate) fn next_query_id(stored: u64, tabs: &[store::StoredQueryTab]) -> u64 {
    stored.max(tabs.iter().map(|tab| tab.id + 1).max().unwrap_or(0))
}

/// Whether the rows on screen are only waiting to be replaced. A relation
/// keeps its rows on screen while it is refreshed, and the result replaces the
/// grid wholesale, edits staged on it included. A query tab's run blanks its
/// grid first, but for an `EXPLAIN`, which never touches it.
pub(crate) fn refreshing(active: Tab, query: Option<&QueryState>) -> bool {
    matches!(active, Tab::Object(_)) && matches!(query, Some(QueryState::Running { .. }))
}

pub(crate) fn result_pane_is_expanded(query: &QueryState) -> bool {
    !matches!(query, QueryState::Idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql;

    #[test]
    fn a_scroll_scope_tells_apart_tabs_that_share_an_id() {
        // next_query_id and next_object_id each count from zero per profile,
        // so a query tab and an object tab in the same profile -- or the same
        // kind of tab in two profiles -- can share a bare id.
        assert_ne!(
            Tab::Query(1).scroll_scope("a"),
            Tab::Object(1).scroll_scope("a")
        );
        assert_ne!(
            Tab::Query(1).scroll_scope("a"),
            Tab::Query(1).scroll_scope("b")
        );
        assert_eq!(
            Tab::Query(1).scroll_scope("a"),
            Tab::Query(1).scroll_scope("a")
        );
    }

    #[test]
    fn a_queue_asks_after_a_failure_only_when_something_is_left_to_run() {
        let remaining = [10..18, 20..27];
        assert_eq!(next_step(false, &remaining), Step::Next(10..18));
        assert_eq!(next_step(true, &remaining), Step::Ask);
        assert_eq!(next_step(true, &[]), Step::Finished);
        assert_eq!(next_step(false, &[]), Step::Finished);
    }

    #[test]
    fn a_query_label_is_one_clipped_line() {
        assert_eq!(query_label("SELECT\n  1"), "SELECT 1");
        assert_eq!(
            query_label("SELECT a, b, c, d, e, f, g, h FROM accounts"),
            "SELECT a, b, c, d, e, f, g, h FR…"
        );
    }

    #[test]
    fn a_count_answers_only_the_filter_it_was_asked_under() {
        let count = RowCount::Counted("id > 3".into(), 40);
        assert!(count.answers("id > 3"));
        assert!(!count.answers(""));
        assert!(!RowCount::Unasked.answers(""));
        let counting = RowCount::Counting {
            filter: String::new(),
            started: std::time::Instant::now(),
            cancel: CancelToken::default(),
            cancelling: false,
        };
        assert!(counting.answers(""));
        assert_eq!(relation_rows(&counting, "", None, false, false), None);
    }

    #[test]
    fn the_status_bar_shows_a_count_over_an_estimate_and_neither_when_unsure() {
        let unasked = RowCount::Unasked;
        let counted = RowCount::Counted("id > 3".into(), 1_234);

        assert_eq!(
            relation_rows(&unasked, "", Some(1_000_000), false, false).as_deref(),
            Some("\u{2248}1,000,000 rows")
        );
        // Snowflake's count is the table's, not a sample's.
        assert_eq!(
            relation_rows(&unasked, "", Some(1_000_000), true, false).as_deref(),
            Some("1,000,000 rows")
        );
        assert_eq!(
            relation_rows(&counted, "id > 3", Some(1_000_000), false, false).as_deref(),
            Some("1,234 rows")
        );
        // An estimate is of the whole table, so it says nothing under a filter,
        // and a count asked under another filter says nothing under this one.
        assert_eq!(
            relation_rows(&unasked, "id > 3", Some(9), false, false),
            None
        );
        assert_eq!(relation_rows(&counted, "id > 4", None, false, false), None);
        // A never-analyzed table's zero, and a short first page whose own
        // readout is already exact.
        assert_eq!(relation_rows(&unasked, "", Some(0), false, false), None);
        assert_eq!(
            relation_rows(&unasked, "", Some(0), true, false).as_deref(),
            Some("0 rows")
        );
        assert_eq!(relation_rows(&unasked, "", Some(900), false, true), None);
        assert_eq!(relation_rows(&unasked, "", None, false, false), None);
    }

    #[test]
    fn one_relation_and_one_filter_is_one_tab() {
        let open = [
            (7u64, "public", "customers", ""),
            (9u64, "public", "customers", r#""id" = '42'"#),
        ];
        assert_eq!(
            matching_tab(open.iter().copied(), "public", "customers", ""),
            Some(7)
        );
        assert_eq!(
            matching_tab(
                open.iter().copied(),
                "public",
                "customers",
                r#""id" = '42'"#
            ),
            Some(9)
        );
    }

    #[test]
    fn a_filtered_relation_does_not_take_over_the_unfiltered_tab() {
        // The invariant this tier changes: following a key cannot clobber what
        // the relation's own tab was showing.
        let open = [(7u64, "public", "customers", "")];
        assert_eq!(
            matching_tab(
                open.iter().copied(),
                "public",
                "customers",
                r#""id" = '42'"#
            ),
            None
        );
    }

    #[test]
    fn two_schemas_can_hold_a_relation_of_the_same_name_and_filter() {
        let open = [(7u64, "public", "customers", r#""id" = '42'"#)];
        assert_eq!(
            matching_tab(
                open.iter().copied(),
                "archive",
                "customers",
                r#""id" = '42'"#
            ),
            None
        );
    }

    #[test]
    fn a_field_nobody_touched_is_left_out_so_the_servers_default_applies() {
        // The whole design of the form in one function: absent, NULL, or a
        // value -- and the first of those is not a value at all.
        assert_eq!(insert_value(false, false, ""), None);
        assert_eq!(insert_value(true, false, ""), Some(None));
        // Typed and emptied is a value: `''` has to be reachable.
        assert_eq!(insert_value(false, true, ""), Some(Some(String::new())));
        assert_eq!(
            insert_value(false, true, "42"),
            Some(Some("42".to_string()))
        );
        // The chip is the later word: a field nulled after being typed into is
        // a NULL.
        assert_eq!(insert_value(true, true, "42"), Some(None));
    }

    #[test]
    fn a_half_filled_form_names_only_the_columns_it_filled() {
        // `id` untouched so the sequence fills it, `note` deliberately nulled,
        // `bio` typed and emptied so it inserts an empty string.
        let filled: Vec<(String, Option<String>)> = [
            ("id", false, false, ""),
            ("name", false, true, "Ada"),
            ("note", true, false, ""),
            ("bio", false, true, ""),
        ]
        .into_iter()
        .filter_map(|(column, nulled, touched, typed)| {
            insert_value(nulled, touched, typed).map(|value| (column.to_string(), value))
        })
        .collect();
        let borrowed: Vec<(&str, Option<&str>)> = filled
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_deref()))
            .collect();

        let statement =
            sql::insert_row(Engine::Postgres, "public", "accounts", &borrowed, &[]).unwrap();
        assert_eq!(
            statement,
            r#"INSERT INTO "public"."accounts" ("name", "note", "bio") VALUES ('Ada', NULL, '')"#
        );
        assert!(
            sql::is_generated_write(&statement),
            "{statement} was refused"
        );
    }

    #[test]
    fn only_the_tab_that_is_a_file_is_asked_about_before_it_closes() {
        let saved = |name: &str| CloseTarget::SavedQuery(name.to_string());

        assert_eq!(close_target(Tab::Object(3), None), CloseTarget::Object(3));
        assert_eq!(close_target(Tab::Query(0), Some("daily")), saved("daily"));
        // Any unsaved buffer, the last one too, is a scratch pad someone is
        // done with: it goes without a question, the way an unnamed buffer
        // does everywhere.
        assert_eq!(close_target(Tab::Query(7), None), CloseTarget::Buffer(7));
        // What the query tab happens to be holding says nothing about an
        // object tab, which is the one in front.
        assert_eq!(
            close_target(Tab::Object(3), Some("daily")),
            CloseTarget::Object(3)
        );
    }

    #[test]
    fn a_new_tab_opens_at_the_right_end_of_the_strip_as_drawn() {
        let restored = vec![
            TabKey::Unsaved(0),
            TabKey::Saved("daily".into()),
            TabKey::Object(0),
        ];
        // Nothing has been dragged, so the restored chips are in no order but
        // the default one -- which files a new buffer beside the old one.
        let mut chips = restored.clone();
        chips.insert(1, TabKey::Unsaved(1));
        let order = placed_last(strip_order(chips.clone(), &[]), TabKey::Unsaved(1));
        assert_eq!(
            strip_order(chips.clone(), &order),
            [restored.clone(), vec![TabKey::Unsaved(1)]].concat()
        );

        // And the next one, an object, lands after that.
        chips.push(TabKey::Object(1));
        let order = placed_last(strip_order(chips.clone(), &order), TabKey::Object(1));
        assert_eq!(
            strip_order(chips, &order),
            [restored, vec![TabKey::Unsaved(1), TabKey::Object(1)]].concat()
        );
    }

    #[test]
    fn a_dragged_order_survives_chips_it_does_not_mention() {
        let chips = vec![TabKey::Unsaved(0), TabKey::Unsaved(1), TabKey::Object(0)];
        let dragged = [TabKey::Object(0), TabKey::Unsaved(5), TabKey::Unsaved(0)];
        // A key whose tab has gone is skipped, and a chip the order does not
        // name follows it.
        assert_eq!(
            strip_order(chips, &dragged),
            [TabKey::Object(0), TabKey::Unsaved(0), TabKey::Unsaved(1)]
        );
    }

    #[test]
    fn a_closed_tab_hands_the_front_to_its_neighbour_in_the_strip() {
        let strip = [Tab::Query(0), Tab::Object(3), Tab::Query(7)];
        assert_eq!(neighbour(&strip, Tab::Object(3)), Some(Tab::Query(0)));
        assert_eq!(neighbour(&strip, Tab::Query(7)), Some(Tab::Object(3)));
        // The first has nothing on its left.
        assert_eq!(neighbour(&strip, Tab::Query(0)), Some(Tab::Object(3)));
        // The last of all leaves nothing, and a tab not in the strip names no
        // neighbour.
        assert_eq!(neighbour(&[Tab::Query(0)], Tab::Query(0)), None);
        assert_eq!(neighbour(&strip, Tab::Object(9)), None);
    }

    #[test]
    fn only_a_relation_running_with_rows_kept_holds_them_read_only() {
        let running = QueryState::Running {
            started: std::time::Instant::now(),
            cancelling: None,
            cancel: CancelToken::default(),
        };
        let landed = QueryState::Complete {
            rows: 1,
            bytes: 0,
            elapsed: std::time::Duration::ZERO,
            rows_affected: None,
        };
        assert!(refreshing(Tab::Object(3), Some(&running)));
        assert!(!refreshing(Tab::Object(3), Some(&landed)));
        // A query tab running over rows is running an `EXPLAIN`, which leaves
        // them where they are.
        assert!(!refreshing(Tab::Query(0), Some(&running)));
        // A routine runs nothing.
        assert!(!refreshing(Tab::Object(3), None));
    }

    #[test]
    fn result_pane_expands_as_soon_as_a_query_starts() {
        assert!(!result_pane_is_expanded(&QueryState::Idle));
        assert!(result_pane_is_expanded(&QueryState::Running {
            started: std::time::Instant::now(),
            cancelling: None,
            cancel: CancelToken::default(),
        }));
    }

    /// A cancel that has been sent has not stopped anything yet, so every
    /// question asked of a running query has to keep its old answer.
    #[test]
    fn a_query_being_cancelled_is_still_running() {
        let cancelling = QueryState::Running {
            started: std::time::Instant::now(),
            cancelling: Some(std::time::Instant::now()),
            cancel: CancelToken::default(),
        };
        assert!(result_pane_is_expanded(&cancelling));
        assert!(matches!(cancelling, QueryState::Running { .. }));
    }

    #[test]
    fn the_next_buffer_id_never_goes_backwards_over_a_closed_tab() {
        let tabs = |ids: &[u64]| {
            ids.iter()
                .map(|id| store::StoredQueryTab {
                    id: *id,
                    name: None,
                    active: false,
                    queued_results: 0,
                })
                .collect::<Vec<_>>()
        };

        // Tab 1 closed, so the highest surviving id is 0 -- and deriving the
        // next id from it would hand out 1 again, over tab 1's snapshot.
        assert_eq!(next_query_id(2, &tabs(&[0])), 2);
        // A profile written before the id was persisted has no stored value,
        // and the derived one is all there is.
        assert_eq!(next_query_id(0, &tabs(&[0, 1])), 2);
        // A stored value behind the open tabs -- an older build's file beside
        // a newer build's tabs -- must not hand out a live id.
        assert_eq!(next_query_id(1, &tabs(&[0, 4])), 5);
        assert_eq!(next_query_id(0, &tabs(&[])), 0);
    }
}

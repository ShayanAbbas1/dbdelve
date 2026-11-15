//! Changing rows: inserting, editing, nulling, deleting, paging, sorting and copying.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::actions::StepPastEdge;
use crate::db::{StepKind, VerifiedStep};
use crate::result_grid::DraftRow;
use crate::session::{
    APPLY_IN_TRANSACTION, AfterEdits, ReviewSlot, StaleEdit, StaleResume, Verification,
    insert_placeholder, refreshing,
};

impl Workspace {
    /// Whether a verified write is refused, with the notice Apply gives, because
    /// the user's transaction is open: the write's own `BEGIN` and `COMMIT`
    /// would end it. Asked before a form or a review opens, so nothing is
    /// filled in or read only to be refused at Run.
    fn refuses_in_transaction(&mut self, cx: &mut Context<Self>) -> bool {
        let refused = self.target().verifies_applies()
            && self
                .profile()
                .is_some_and(|profile| profile.transaction_open_for(profile.session.active));
        if refused {
            self.note(APPLY_IN_TRANSACTION.into(), cx);
        }
        refused
    }

    /// Open the "Insert" form over the preview in front, one field per column
    /// of the relation's loaded structure (spec §4).
    ///
    /// Requires a schema and a table and not a primary key, which is why this
    /// is offered on relations the grid refuses to edit.
    pub(crate) fn new_row(&mut self, _: &NewRow, window: &mut Window, cx: &mut Context<Self>) {
        if self.import_covers() {
            return;
        }
        // Does not route through `editable`: an insert has no existing row to
        // check a column of, so the mode is asked outright -- but not on a
        // structure tab, which has no row to add and only needs pointing back.
        if self
            .profile()
            .and_then(|profile| profile.session.active_object())
            .is_some_and(|tab| matches!(tab.body, ObjectBody::Structure { .. }))
        {
            self.note("Switch to Data to add a row.".into(), cx);
            return;
        }
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.clear_notice();
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let active = profile.session.active;
        // A form already open for this tab is focused rather than reset, so
        // pressing New row (or its shortcut) again does not silently wipe a
        // half-filled row.
        if let Some(form) = &profile.session.insert_form
            && form.tab == active
        {
            if let Some(field) = form.fields.first() {
                field.input.focus_handle(cx).focus(window, cx);
            }
            // The form is drawn in the results pane, so asking for one brings
            // a hidden pane back rather than filling in a form nobody can see.
            if let Some(profile) = self.profile_mut() {
                profile.session.reveal_results(active);
            }
            return;
        }
        let (schema, table, columns) = match active {
            Tab::Object(id) => {
                let Some(tab) = profile.session.objects.iter().find(|tab| tab.id == id) else {
                    return;
                };
                let (structure, results) = match &tab.body {
                    ObjectBody::Relation {
                        structure, results, ..
                    } => (structure, results),
                    ObjectBody::Structure { .. }
                    | ObjectBody::Routine(_)
                    | ObjectBody::Diagram { .. } => return,
                };
                if !tab.takes_inserts(self.target()) {
                    self.note("This view takes no inserts.".into(), cx);
                    return;
                }
                // The columns are the form: without them there is nothing to
                // draw, and guessing at them would be inventing a table.
                let StructureState::Loaded(structure) = structure else {
                    self.note(
                        "DBDelve has not read this relation's columns yet.".into(),
                        cx,
                    );
                    return;
                };
                // A Draft view of a table whose rows are written back keeps
                // the row in the grid, where it is typed in place and read in
                // the draft's review; every other view takes the form.
                if matches!(active, Tab::Object(_))
                    && profile.session.edit_mode(active) == EditMode::Draft
                    && self.target().keeps_edits()
                    && results.read(cx).delegate().insert_target().is_some()
                {
                    let results = results.clone();
                    self.add_grid_row(active, results, cx);
                    return;
                }
                let columns: Vec<(String, String)> = structure
                    .columns
                    .iter()
                    .filter(|column| !engine.new_row_omits(&column.name))
                    .map(|column| (column.name.clone(), column.data_type.clone()))
                    .collect();
                (tab.schema.clone(), tab.name.clone(), columns)
            }
            // The grid's own columns, where its rows are written back to: one
            // table, which is never so for a join or a computed result.
            Tab::Query(_) => {
                let Some(target) = profile.session.query_insert_target(self.target(), cx) else {
                    self.note("These rows take no new row.".into(), cx);
                    return;
                };
                target
            }
        };
        if self.refuses_in_transaction(cx) {
            return;
        }
        let mut fields = Vec::with_capacity(columns.len());
        for (index, (column, data_type)) in columns.into_iter().enumerate() {
            let sends_empty = engine.new_row_sends_empty(&column);
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(insert_placeholder(
                    false,
                    false,
                    sends_empty,
                ))
            });
            // The first keystroke is what makes this field a value rather than
            // an omission, so the change event is where `touched` is set.
            cx.subscribe_in(
                &input,
                window,
                move |workspace, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        workspace.touch_insert_field(index, window, cx);
                    }
                },
            )
            .detach();
            fields.push(InsertField {
                column,
                data_type,
                input,
                nulled: false,
                touched: false,
                sends_empty,
            });
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.insert_form = Some(InsertForm {
                tab: active,
                schema,
                table,
                fields,
            });
            profile.session.reveal_results(active);
        }
        cx.notify();
    }

    /// Insert in a Draft view: a new row at the end of the grid, the ring on
    /// its first cell. Nothing is sent and nothing is asked; the row is part of
    /// the draft, and `apply_edits` writes it.
    ///
    /// Held back as staging a cell is: not while a review holds the draft, and
    /// not while the tab's rows are still being read, which would put the row
    /// above rows that have not arrived.
    fn add_grid_row(
        &mut self,
        tab: Tab,
        results: Entity<gpui_component::table::TableState<ResultGrid>>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        if !profile.session.may_stage_into(tab) {
            return;
        }
        if profile.session.running(tab) {
            self.note(
                "The rows are still being read, so no row was added.".into(),
                cx,
            );
            return;
        }
        if !self.confirm_stale(StaleResume::NewRow, cx) {
            return;
        }
        results.update(cx, |table, cx| {
            crate::result_grid::add_row_at_end(table, cx);
            cx.notify();
        });
    }

    /// Typing is the latest word, so it turns a NULL off.
    pub(crate) fn touch_insert_field(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_insert_state(index, false, true, window, cx);
    }

    /// Clears the box when turning NULL on, so the input shows what is sent;
    /// turning it off returns the field to left out.
    pub(crate) fn toggle_insert_null(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let nulled = self
            .profile()
            .and_then(|profile| profile.session.insert_form.as_ref())
            .and_then(|form| form.fields.get(index))
            .is_some_and(|field| field.nulled);
        if nulled {
            self.default_insert_field(index, window, cx);
        } else {
            self.set_insert_state(index, true, false, window, cx);
            self.clear_insert_input(index, window, cx);
        }
    }

    /// Back to left out of the `INSERT`: the one way to undo a first
    /// keystroke, which otherwise makes even an emptied field a `''`.
    pub(crate) fn default_insert_field(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_insert_state(index, false, false, window, cx);
        self.clear_insert_input(index, window, cx);
    }

    fn set_insert_state(
        &mut self,
        index: usize,
        nulled: bool,
        touched: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(profile) = self.profile_mut()
            && let Some(form) = &mut profile.session.insert_form
            && let Some(field) = form.fields.get_mut(index)
        {
            field.nulled = nulled;
            field.touched = touched;
            let placeholder = insert_placeholder(nulled, touched, field.sends_empty);
            let input = field.input.clone();
            input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
            cx.notify();
        }
    }

    fn clear_insert_input(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self
            .profile()
            .and_then(|profile| profile.session.insert_form.as_ref())
            && let Some(field) = form.fields.get(index)
        {
            field
                .input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    /// Generate the `INSERT` and show it. **Nothing runs here** — Run in the
    /// review panel is the ask (`AGENTS.md` rule 1).
    pub(crate) fn confirm_new_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_notice();
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(form) = &profile.session.insert_form else {
            return;
        };
        let (tab, schema, table) = (form.tab, form.schema.clone(), form.table.clone());
        let filled: Vec<(String, Option<String>)> = form
            .fields
            .iter()
            .filter_map(|field| {
                insert_value(field.nulled, field.touched, &field.input.read(cx).value())
                    .map(|value| (field.column.clone(), value))
            })
            .collect();
        let types: Vec<(String, String)> = form
            .fields
            .iter()
            .map(|field| (field.column.clone(), field.data_type.clone()))
            .collect();
        let borrowed: Vec<(&str, Option<&str>)> = filled
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_deref()))
            .collect();

        let statement = match sql::insert_row(engine, &schema, &table, &borrowed, &types) {
            Ok(statement) => statement,
            Err(message) => {
                self.note(message, cx);
                return;
            }
        };
        // The gate every generated statement passes before anything executes
        // (`AGENTS.md` rule 2). Failing it is dbdelve disagreeing with itself --
        // a bug in dbdelve rather than a user error -- so it is said and not run.
        if !sql::is_generated_write_on(engine, &statement) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not an INSERT.".into(),
                cx,
            );
            return;
        }
        // A form opened before the user's transaction began.
        if self.refuses_in_transaction(cx) {
            return;
        }
        // The `INSERT` must count the one row it adds.
        let verify = if self.target().verifies_applies() {
            match lone_verification(engine, &statement, StepKind::Insert, None, None) {
                Ok(verify) => Some(verify),
                Err(message) => {
                    self.note(message, cx);
                    return;
                }
            }
        } else {
            None
        };
        if !self.review_slot_free(tab, false, window, cx) {
            return;
        }

        let notes = self.target().review_notes(&statement);
        if let Some(profile) = self.profile_mut() {
            profile.session.insert_form = None;
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "Insert",
                notes,
                sql: statement,
                error: None,
                edits: false,
                verify,
            });
        }
        cx.notify();
    }

    /// Put the form away. Nothing has been generated yet, so there is nothing
    /// to keep.
    ///
    /// Scoped to the active tab's own form: one left open on another tab is
    /// not what `escape` on this surface means, and must not eat the
    /// keystroke meant for whatever this tab actually has in front.
    pub(crate) fn close_new_row(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let active = profile.session.active;
        let on_active_tab = profile
            .session
            .insert_form
            .as_ref()
            .is_some_and(|form| form.tab == active);
        if !on_active_tab {
            return false;
        }
        let closed = profile.session.insert_form.take().is_some();
        if closed {
            cx.notify();
        }
        closed
    }

    /// Move a relation's preview one page of `limit` rows through the table.
    pub(crate) fn next_page(&mut self, _: &NextPage, _: &mut Window, cx: &mut Context<Self>) {
        self.turn_page(true, cx);
    }

    pub(crate) fn previous_page(
        &mut self,
        _: &PreviousPage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.turn_page(false, cx);
    }

    /// Paging is gated on what the last run brought back, not on a row count
    /// the server was never asked for: a page shorter than its limit is the
    /// relation's last, so there is nothing forward of it, and only a completed
    /// page says how long it was. Backwards needs no result at all -- anywhere
    /// but the first page, there is a page before this one.
    pub(crate) fn turn_page(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.clear_notice();
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
        else {
            return;
        };
        let ObjectBody::Relation {
            query,
            limit,
            offset,
            ..
        } = &tab.body
        else {
            return;
        };
        let can_turn = match forward {
            true if !self.engine().pages() => false,
            true => matches!(
                query,
                QueryState::Complete { rows, .. } if limit.is_some_and(|limit| *rows >= limit)
            ),
            false => *offset > 0,
        };
        if !can_turn {
            return;
        }
        self.requery_relation(
            id,
            move |_, _, limit, offset| {
                let Some(limit) = *limit else {
                    return false;
                };
                match forward {
                    true => *offset += limit,
                    false => *offset = offset.saturating_sub(limit),
                }
                true
            },
            cx,
        );
    }

    /// Jump to a typed page, for a relation deep enough that reaching page 20
    /// by arrow is twenty clicks.
    ///
    /// Offsets stay multiples of the limit, the same invariant `turn_page`
    /// keeps, so the field reads the page back exactly once it lands.
    /// Nothing here knows how long the relation is, so a page past its end is
    /// allowed to come back empty rather than be guessed at -- the previous
    /// arrow is the way back from one.
    pub(crate) fn go_to_page(&mut self, cx: &mut Context<Self>) {
        self.clear_notice();
        let Some((id, input)) = self
            .profile()
            .and_then(|profile| match profile.session.active {
                Tab::Object(id) => Some((id, profile.session.page_input.clone())),
                _ => None,
            })
        else {
            return;
        };
        let typed = input.read(cx).value().trim().to_string();
        if typed.is_empty() || !self.engine().pages() {
            return;
        }
        let Some(page) = typed_page(&typed) else {
            self.note(format!("{typed} is not a page number."), cx);
            return;
        };
        self.requery_relation(
            id,
            move |_, _, limit, offset| {
                let Some(limit) = *limit else {
                    return false;
                };
                *offset = page * limit;
                true
            },
            cx,
        );
    }

    /// Show the page in front in the field. Typing is left alone while the
    /// page stays put; once it moves, the typed number was for a page that is
    /// no longer the one in front, so it goes too.
    ///
    /// From render because the offset moves in more places than one -- the
    /// arrows, the row-limit chips, a tab switch -- and the field is shared by
    /// every tab, so reading it back once a frame is the one spot all of them
    /// pass through.
    pub(crate) fn sync_page_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let input = profile.session.page_input.clone();
        let page = profile
            .session
            .active_object()
            .and_then(|tab| match &tab.body {
                ObjectBody::Relation { limit, offset, .. } => {
                    Some(limit.map_or(1, |limit| offset / limit + 1))
                }
                _ => None,
            });
        let Some(page) = page else {
            return;
        };
        let moved = profile.session.page_shown != page;
        if !moved && input.focus_handle(cx).is_focused(window) {
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.page_shown = page;
        }
        let page = page.to_string();
        if input.read(cx).value() != page {
            input.update(cx, |state, cx| state.set_value(page, window, cx));
        }
    }

    /// A column header was clicked: move that column through the view's sort.
    ///
    /// On a view the server sorts, the column goes into the statement's
    /// `ORDER BY` and the statement runs again, so the top of the sort is the
    /// table's, not the page's. On a view sorted in memory, the rows already
    /// held are reordered and nothing runs.
    ///
    /// A click appends: a column that is not in the sort joins the end of it,
    /// one that is ascending turns around, and one that is descending drops
    /// out. Several columns therefore build up a compound sort by clicking.
    pub(crate) fn sort_column(
        &mut self,
        action: &SortColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let column = action.column;
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        // Asked of the tab, not the grid on screen, which can be a finished
        // result of a queue whose next statement is still running. A sort
        // that runs SQL would be refused by that run after it had already
        // rewritten the buffer, and rows still arriving would be ordered only
        // in part.
        let tab = profile.session.active;
        if profile.session.running(tab) {
            self.note(
                "A statement is still running on this tab. Sort once it finishes, or cancel it."
                    .into(),
                cx,
            );
            return;
        }
        if profile
            .session
            .sorting(tab)
            .and_then(Sorting::client_keys)
            .is_some()
        {
            // A Row view's edited row is saved before the rows move under it,
            // as a sort on the server would (`requery_relation`); a Draft view
            // keeps its edits through a sort in memory.
            if profile.session.edit_mode(tab) == EditMode::Row
                && self.held_for_edits(tab, AfterEdits::Sort(column), cx)
            {
                return;
            }
            self.sort_held_rows(tab, column, cx);
            return;
        }

        match tab {
            // Already guarded inside `requery_relation`, which asks of copies.
            Tab::Object(id) => self.relation_sort(id, column, cx),
            Tab::Query(_) => {
                // Before `query_sort` rewrites the buffer: a prompt asked after
                // it would leave a cancelled sort written into the user's SQL.
                if self.held_for_edits(tab, AfterEdits::Sort(column), cx) {
                    return;
                }
                self.query_sort(column, window, cx)
            }
        }
    }

    /// A header click on a view sorted in memory: the grid in front, which on
    /// a queue is the result its switcher is showing.
    fn sort_held_rows(&mut self, tab: Tab, column: usize, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        let columns = results.read(cx).delegate().columns().to_vec();
        let Some(expression) = sort_expression(engine, &columns, column) else {
            return;
        };
        let Some(Sorting::Client(keys)) = profile.session.sorting_mut(tab) else {
            return;
        };
        cycle(keys, &expression);
        let order = sort_columns(engine, keys, &columns);
        results.update(cx, |table, cx| {
            table.delegate_mut().sort_in_memory(order);
            // The inspector reads the selected row, and that index
            // now names whichever row the sort moved there.
            table.clear_selection(cx);
            cx.notify();
        });
    }

    /// The status bar's Server and Client chips: who orders the rows of the
    /// view in front. The sort on screen carries across. Into memory, the
    /// statement keeps its `ORDER BY`; into the server, the keys go the way a
    /// header click's do, refusals included, and a refused query tab stays
    /// sorted in memory, since those are still the rows on screen.
    pub(crate) fn set_view_sorting(
        &mut self,
        client: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        // Either way the result in front: `reorder_query` splices into the
        // statement behind it.
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        let (order, shown) = {
            let grid = results.read(cx).delegate();
            let shown: Vec<SortKey> = grid
                .sort()
                .iter()
                .filter_map(|&(column, ascending)| {
                    Some(SortKey::new(
                        sort_expression(engine, grid.columns(), column)?,
                        ascending,
                    ))
                })
                .collect();
            (grid.sort().to_vec(), shown)
        };
        // Only a switch that runs the statement again can lose a draft: into
        // memory nothing runs, and out of it with no sort on screen there is
        // nothing to ask the server for. Before `switch` moves the sorting.
        let leaves_memory = !client
            && profile
                .session
                .sorting(tab)
                .is_some_and(|sorting| sorting.client_keys().is_some());
        if leaves_memory
            && !shown.is_empty()
            && self.held_for_edits(tab, AfterEdits::ViewSorting(client), cx)
        {
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(sorting) = profile.session.sorting_mut(tab) else {
            return;
        };
        let before = sorting.clone();
        let entering = client && sorting.client_keys().is_none();
        let ask = sorting.switch(client, shown);
        if entering {
            // Already in this order, from the server. Sorted again so every
            // header takes a click and ties fall back to the order they came in.
            results.update(cx, |table, cx| {
                table.delegate_mut().sort_in_memory(order);
                table.clear_selection(cx);
                cx.notify();
            });
        }
        if let Some(keys) = ask {
            match tab {
                Tab::Object(id) => self.requery_relation(
                    id,
                    move |_, sort, _, offset| {
                        *sort = keys;
                        // As a header click's: page five of one sort is not
                        // page five of another.
                        *offset = 0;
                        true
                    },
                    cx,
                ),
                Tab::Query(_) => {
                    if !self.reorder_query(|existing| *existing = keys, window, cx)
                        && let Some(sorting) = self
                            .profile_mut()
                            .and_then(|profile| profile.session.sorting_mut(tab))
                    {
                        *sorting = before;
                    }
                }
            }
        }
        cx.notify();
    }

    /// Sorting a query the user wrote: the `ORDER BY` goes into their statement,
    /// where they can see it, edit it and undo it. dbdelve changes SQL only when
    /// asked, and a header click is the ask (`AGENTS.md`, rule 1).
    pub(crate) fn query_sort(
        &mut self,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .cloned()
        else {
            return;
        };
        let Some(expression) =
            sort_expression(engine, results.read(cx).delegate().columns(), column)
        else {
            return;
        };
        self.reorder_query(|keys| cycle(keys, &expression), window, cx);
    }

    /// Give the statement behind the query tab's grid the `ORDER BY` that
    /// `change` makes of the one it has, in the buffer, and run it. A header
    /// click and a switch to server sorting both come through here, so both
    /// meet the same refusals. `false` when nothing ran.
    pub(crate) fn reorder_query(
        &mut self,
        change: impl FnOnce(&mut Vec<SortKey>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return false;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return false;
        };
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let (in_turn, mode) = (profile.in_turn(), profile.mode);
        let bound = profile.session.is_bound(Tab::Query(tab_id));
        // A header click refuses this itself; the sorting chips come here
        // without asking.
        let running = profile.session.running(Tab::Query(tab_id));
        // On a queue, the result the switcher shows. Only the last of them is
        // the tab's slot, which `last_query` describes.
        let queued = tab.queue.as_ref().and_then(|queue| {
            let finished = queue.done.get(queue.showing)?;
            (finished.grid != tab.results).then(|| (queue.showing, finished.sql.clone()))
        });
        // An earlier result reruns as a lane, which only a queue with nothing
        // left to send can take: one still out, or parked on a failure,
        // would land its own results around it.
        let unfinished = tab
            .queue
            .as_ref()
            .is_some_and(|queue| !queue.remaining.is_empty() || queue.awaiting || queue.at_once());
        // A lane finds its result by where its statement starts, which a
        // batch's further sets share with its first.
        let unreachable = tab.queue.as_ref().is_some_and(|queue| {
            queue
                .done
                .get(queue.showing)
                .is_some_and(|finished| queue.lane(finished.start) != Some(queue.showing))
        });
        let (queued, last_query) = match queued {
            Some((index, sql)) => (Some(index), sql),
            None => match tab.last_query.clone() {
                Some(sql) => (None, sql),
                None => return false,
            },
        };
        if running {
            self.note(
                "A statement is still running on this tab. Sort once it finishes, or cancel it."
                    .into(),
                cx,
            );
            return false;
        }
        if queued.is_some() && unfinished {
            self.note(
                "Sort this result once the rest of the run has finished.".into(),
                cx,
            );
            return false;
        }
        if queued.is_some() && unreachable {
            self.note(
                "These rows are one of several sets a batch returned, and cannot be sorted on \
                 their own."
                    .into(),
                cx,
            );
            return false;
        }
        // A sort re-runs the statement behind the grid, and one that writes
        // would write again.
        if !sql::rerunnable(engine, &last_query) {
            self.note(
                "These rows came from a statement that writes, and sorting would run it again."
                    .into(),
                cx,
            );
            return false;
        }

        let (text, cursor) = {
            let editor = editor.read(cx);
            (editor.value().to_string(), editor.cursor())
        };
        // The grid's own statement, not whatever the cursor is on: that may be
        // another statement entirely, and one that never ran.
        let Some(range) =
            Buffer::for_engine(engine, &text).find(engine, &text, cursor, &last_query)
        else {
            self.note(
                "The statement behind these rows is no longer in the editor.".into(),
                cx,
            );
            return false;
        };
        let statement = &text[range.clone()];

        let Some(mut keys) = sql::order_by(engine, statement) else {
            self.note(
                format!(
                    "dbdelve cannot add {} to this statement without rewriting it.",
                    engine.sort_clause()
                ),
                cx,
            );
            return false;
        };
        change(&mut keys);
        let Some(sorted) = sql::with_order_by(engine, statement, &keys) else {
            self.note(
                format!("This statement cannot carry {}.", engine.sort_clause()),
                cx,
            );
            return false;
        };
        // An earlier result reruns on a connection of its own, as the queue's
        // reads did when they ran at once.
        if queued.is_some() {
            let refusal = match in_turn {
                Some(_) => Some(
                    "Only the last result can be sorted by the server while this connection \
                     has a transaction open or changed session settings. Use Client sorting \
                     for this one.",
                ),
                // A lane runs on the profile's own connection, which is not
                // the database the tab is bound to.
                None if bound => Some(
                    "Only the last result can be sorted by the server in a tab bound to a \
                     database. Use Client sorting for this one.",
                ),
                None if !session::runs_at_once(engine, mode, &[&sorted]) => Some(
                    "Only the last result can be sorted by the server unless its statement \
                     is a plain read. Use Client sorting for this one.",
                ),
                None => None,
            };
            if let Some(refusal) = refusal {
                self.note(refusal.into(), cx);
                return false;
            }
        }

        editor.update(cx, |editor, cx| {
            queries::replace_range(editor, range, &sorted, window, cx);
        });
        match queued {
            Some(index) => self.rerun_queued(Tab::Query(tab_id), index, sorted, cx),
            None => {
                self.execute_sql(sorted, Tab::Query(tab_id), cx);
                true
            }
        }
    }

    /// `Enter` on the active cell opens an input on it. Everything after this
    /// keystroke — the input, the commit, the cancel — belongs to the grid; the
    /// refusal belongs here, because the grid has nowhere to say anything.
    pub(crate) fn edit_cell(&mut self, _: &EditCell, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        // A batch already on screen was generated from the pending set as it
        // stood. Another edit behind the modal would leave the statement the
        // user is reading describing something other than what the grid holds.
        if !profile.session.may_stage_into(profile.session.active) {
            return;
        }
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        // The grid's own active cell, not the library's selection: its selected
        // row and column are mutually exclusive modes rather than a cell, so
        // `selected_col` is `None` after a click. Both of its selections are
        // folded into this coordinate, which is why arrow keys land here too.
        let Some((row, col)) = results.read(cx).delegate().active() else {
            return;
        };
        let tab = profile.session.active;
        if results.read(cx).delegate().editable(row, col)
            && !self.confirm_stale(StaleResume::Open, cx)
        {
            return;
        }
        if results.update(cx, |table, cx| {
            let opened = table.delegate_mut().begin_edit(row, col);
            cx.notify();
            opened
        }) {
            return;
        }

        // Only structure can refuse an open now: a mode too low still opens the
        // cell, so a value can be selected and copied out of it, and refuses at
        // the commit -- which is where the mode prompt is raised.
        //
        // Which of the two refusals this is, read off `editable` rather than off
        // an edit target the grid deliberately does not expose: a result dbdelve
        // cannot trace to one table has no editable cell anywhere in the row,
        // and one it can has this column alone refused.
        // A snapshot written before edit targets were kept has none, so it
        // would read as untraceable too.
        let (marked, traced, snapshot, reason, bytes) = {
            let grid = results.read(cx).delegate();
            (
                grid.is_marked(row),
                grid.row_editable(row),
                grid.captured().is_some(),
                grid.read_only_reason().map(str::to_string),
                grid.bytes_refusal(row, col),
            )
        };
        if let Some(bytes) = bytes.filter(|_| !marked) {
            self.note(bytes.into(), cx);
            return;
        }
        // Checked before the snapshot: running a write again is not the way
        // back to editable rows.
        let engine = self.engine();
        let wrote = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .is_some_and(|tab| {
                tab.last_query
                    .as_deref()
                    .is_some_and(|statement| !sql::rerunnable(engine, statement))
            });
        self.note(
            match (traced, reason, wrote, snapshot) {
                _ if marked => "This row is marked for deletion.".into(),
                (true, _, _, _) => "This column cannot be edited.".into(),
                // The engine's own word on why, where it gave one.
                (false, Some(reason), _, _) => reason,
                (false, None, true, _) => {
                    "These rows came from a statement that writes. Fetch them with a query that only reads to edit them.".into()
                }
                (false, None, false, true) => match tab {
                    Tab::Query(_) => {
                        "These rows are from an earlier session. Run the query again to edit them."
                            .into()
                    }
                    _ => "These rows are from an earlier session. Refresh them to edit.".into(),
                },
                (false, None, false, false) => {
                    "dbdelve cannot tell which table these rows come from.".into()
                }
            },
            cx,
        );
    }

    /// Stage a `NULL` on the active cell, from the keystroke, the palette, or
    /// the cell menu — one action behind all three, so none of them can mean
    /// something different (spec §3).
    pub(crate) fn set_null(&mut self, _: &SetNull, window: &mut Window, cx: &mut Context<Self>) {
        self.stage_value(NewValue::Null, window, cx);
    }

    /// The empty string, which is a different write from a `NULL` and the one
    /// an input cannot produce: an input closed as it opened records nothing
    /// (`ResultGrid::commit_text`), and a NULL or a staged keyword opens empty,
    /// so this action is the one way to write the empty string over them.
    pub(crate) fn set_empty(&mut self, _: &SetEmpty, window: &mut Window, cx: &mut Context<Self>) {
        self.stage_value(NewValue::Value(gpui::SharedString::default()), window, cx);
    }

    /// `DEFAULT`, which is the server resolving the column's default rather
    /// than dbdelve guessing at what it would be — the structure reports a
    /// default's *presence*, and sometimes as a marker word rather than an
    /// expression.
    pub(crate) fn set_default(
        &mut self,
        _: &SetDefault,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stage_value(NewValue::Default, window, cx);
    }

    /// The grid's own gestures stage on the active cell.
    fn stage_value(&mut self, value: NewValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cell) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .and_then(|results| results.read(cx).delegate().active())
        else {
            return;
        };
        self.stage_and_refocus(cell, value, window, cx);
    }

    fn stage_and_refocus(
        &mut self,
        (row, col): (usize, usize),
        value: NewValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stage_cell(row, col, value, cx)
            && let Some(results) = self
                .profile()
                .and_then(|profile| profile.session.active_results())
        {
            // The input this just closed had focus, and a window with nothing
            // focused has no dispatch path at all.
            results.focus_handle(cx).focus(window, cx);
        }
    }

    /// The one body behind every staged value, from the grid and the row
    /// panel alike, so the gates, the mode prompt and the refusal cannot drift
    /// between them. `true` once the value is staged.
    pub(crate) fn stage_cell(
        &mut self,
        row: usize,
        col: usize,
        value: NewValue,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(profile) = self.profile() else {
            return false;
        };
        // The gate `edit_cell` holds, for its reason: a batch already on screen
        // was generated from the pending set as it stood.
        if !profile.session.may_stage_into(profile.session.active) {
            return false;
        }
        let Some(results) = profile.session.active_results().cloned() else {
            return false;
        };
        if results.read(cx).delegate().editable(row, col)
            && !self.confirm_stale(StaleResume::Stage((row, col), value.clone()), cx)
        {
            return false;
        }
        // Text that spells no bytes is refused whatever the mode.
        if let Some(reason) = results
            .read(cx)
            .delegate()
            .staging_refusal(row, col, &value)
        {
            self.note(reason.into(), cx);
            return false;
        }
        if results.update(cx, |table, cx| {
            let staged = table.delegate_mut().stage(row, col, value);
            cx.notify();
            staged
        }) {
            return true;
        }
        // Asked after the attempt rather than before it, so staging a value the
        // cell already holds -- which changes nothing -- does not ask a
        // Read-only connection to raise its mode for it.
        if self.require(Mode::ReadWrite, cx) {
            let reason = results.read(cx).delegate().bytes_refusal(row, col);
            self.note(reason.unwrap_or("This column cannot be edited.").into(), cx);
        }
        false
    }

    /// Stage what a row panel input holds, if it is not what it was filled
    /// with: focus passing through an input is not an edit, and an input on a
    /// NULL holds the empty string, which committing would write.
    pub(crate) fn commit_row_input(
        &mut self,
        input: &Entity<gpui_component::input::TextareaState>,
        cx: &mut Context<Self>,
    ) {
        let Some(inputs) = &self.row_panel.inputs else {
            return;
        };
        let Some((col, seed)) = inputs.fields.iter().enumerate().find_map(|(col, field)| {
            let field = field.as_ref().filter(|field| field.input == *input)?;
            Some((col, field.seed.clone()))
        }) else {
            return;
        };
        let (grid, row, rows) = (inputs.grid.clone(), inputs.row, inputs.rows);
        let owner = (inputs.profile.clone(), inputs.tab);
        let text = input.read(cx).value();
        // Rows replaced or reordered since: another record is at `row` now,
        // and the text goes the way the grid's own open input does.
        if text == seed || grid.read(cx).delegate().rows_id() != rows {
            return;
        }
        let in_front = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            == Some(&grid);
        let value = NewValue::Value(text.clone());
        let staged = match in_front {
            true => self.stage_cell(row, col, value, cx),
            false => self.stage_behind(&grid, owner, row, col, value, cx),
        };
        if staged
            && let Some(field) = self
                .row_panel
                .inputs
                .as_mut()
                .and_then(|inputs| inputs.fields.get_mut(col)?.as_mut())
        {
            field.seed = text;
        }
    }

    /// Stage typed text on a grid no longer in front, which is where a blur
    /// from switching tabs lands. `stage_cell`'s gates read the tab in front,
    /// so this asks the grid's own profile and tab, and the grid itself, and
    /// leaves text that would need the stale-rows prompt unstaged rather than
    /// raise it over another tab.
    fn stage_behind(
        &mut self,
        grid: &Entity<gpui_component::table::TableState<ResultGrid>>,
        (profile, tab): (String, Tab),
        row: usize,
        col: usize,
        value: NewValue,
        cx: &mut Context<Self>,
    ) -> bool {
        let may_stage = self
            .profiles
            .iter()
            .find(|found| found.id == profile)
            .is_some_and(|found| found.session.may_stage_into(tab));
        let staged = may_stage
            && grid.update(cx, |table, cx| {
                let staged =
                    !table.delegate().unconfirmed() && table.delegate_mut().stage(row, col, value);
                cx.notify();
                staged
            });
        if !staged {
            self.note(
                "A value typed in the row panel was not staged before switching tabs.".into(),
                cx,
            );
        }
        staged
    }

    /// A row panel chip. The input beside it is refilled even while it has
    /// the keyboard -- a button does not take focus -- since otherwise its
    /// old text would be staged back over the chip's value on the next blur.
    pub(crate) fn stage_row_field(
        &mut self,
        row: usize,
        col: usize,
        value: NewValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.stage_cell(row, col, value, cx) {
            return false;
        }
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .cloned()
        else {
            return true;
        };
        let seed =
            gpui::SharedString::from(results.read(cx).delegate().edit_seed(row, col).into_owned());
        if let Some(field) = self
            .row_panel
            .inputs
            .as_mut()
            .filter(|inputs| inputs.name(&results, row, cx))
            .and_then(|inputs| inputs.fields.get_mut(col)?.as_mut())
        {
            field
                .input
                .update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
            field.seed = seed;
        }
        true
    }

    /// Whether an edit may go ahead on the grid in front: not while its rows are
    /// being refreshed, and on restored rows only once the stale-rows prompt
    /// has been answered. Callers ask only once the grid would otherwise
    /// take the edit, so the prompt never stands in front of a refusal.
    fn confirm_stale(&mut self, resume: StaleResume, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let Some(results) = profile.session.active_results().cloned() else {
            return false;
        };
        if refreshing(profile.session.active, profile.session.active_query()) {
            self.note(Self::REFRESHING.into(), cx);
            return false;
        }
        if !results.read(cx).delegate().unconfirmed() {
            return true;
        }
        if profile.confirmed_stale {
            results.update(cx, |table, _| table.delegate_mut().confirm_stale());
            return true;
        }
        profile.session.stale_edit = Some(StaleEdit {
            resume: Some(resume),
            dont_ask: false,
        });
        cx.notify();
        false
    }

    pub(crate) fn cancel_stale_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self
            .profile_mut()
            .is_some_and(|profile| profile.session.stale_edit.take().is_some());
        if open {
            cx.notify();
        }
        open
    }

    pub(crate) fn toggle_stale_dont_ask(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut()
            && let Some(stale) = &mut profile.session.stale_edit
        {
            stale.dont_ask = !stale.dont_ask;
        }
        cx.notify();
    }

    /// Fetch the rows again instead. The edit is dropped: it was aimed at the
    /// rows being replaced.
    pub(crate) fn refresh_stale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_stale_edit(cx);
        // The statement that produced these rows, not the one under the cursor,
        // which may be a different statement in the same buffer.
        let rerun = self.profile().and_then(|profile| {
            let tab = profile.session.active_query_tab()?;
            Some((tab.id, tab.last_query.clone()?))
        });
        match rerun {
            Some((_, statement)) if !sql::rerunnable(self.engine(), &statement) => self.note(
                "These rows came from a statement that writes, and refreshing would run it again."
                    .into(),
                cx,
            ),
            Some((id, select)) => self.execute_sql(select, Tab::Query(id), cx),
            None => self.run_query(&RunQuery, window, cx),
        }
    }

    /// The status bar's Refresh on restored rows: the same prompt, so the
    /// statement is on screen before it is sent, with no edit behind it.
    pub(crate) fn ask_refresh_stale(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.stale_edit = Some(StaleEdit {
                resume: None,
                dont_ask: false,
            });
        }
        cx.notify();
    }

    /// Accept the rows as they are and carry on with the edit that asked.
    pub(crate) fn edit_stale_anyway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(StaleEdit {
            resume: Some(resume),
            dont_ask,
        }) = profile.session.stale_edit.take()
        else {
            return;
        };
        if let Some(results) = profile.session.active_results().cloned() {
            results.update(cx, |table, _| table.delegate_mut().confirm_stale());
        }
        if dont_ask {
            profile.confirmed_stale = true;
            self.remember_profiles(cx);
        }
        match resume {
            StaleResume::Open => self.edit_cell(&EditCell, window, cx),
            // Back to the panel input the value came from, if it did, so
            // stepping through the row carries on from there.
            StaleResume::Stage(cell, value) => match self.row_input(cell, cx) {
                // Through the chips' path, which refills the input: a chip's
                // value resumed here would otherwise leave typed text in it to
                // be staged back over the value on the next blur.
                Some(input) => {
                    if self.stage_row_field(cell.0, cell.1, value, window, cx) {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    }
                }
                None => self.stage_and_refocus(cell, value, window, cx),
            },
            StaleResume::Delete => self.delete_row(&DeleteRow, window, cx),
            StaleResume::NewRow => self.new_row(&NewRow, window, cx),
        }
        cx.notify();
    }

    /// The grid's own commit was refused by the mode. It has nowhere to say so
    /// and no way to offer the one thing that answers it, so the input stays
    /// open behind this prompt, and raising the mode commits what was typed.
    pub(crate) fn request_write_mode(
        &mut self,
        _: &RequestWriteMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require(Mode::ReadWrite, cx)
            && let Some(pending) = self
                .profile_mut()
                .and_then(|profile| profile.session.pending_run.as_mut())
        {
            pending.commit_cell = true;
        }
    }

    pub(crate) fn refuse_edit(
        &mut self,
        action: &RefuseEdit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note(action.reason.clone(), cx);
    }

    /// Generate the one-row `DELETE` and show it. **Nothing runs here** — Run in
    /// the review panel is the ask (`AGENTS.md` rule 1, spec §5).
    ///
    /// Deliberately not on a keybinding, for the reason [`Workspace::apply_edits`]
    /// is not: a destructive write one fat finger from a navigation key is a
    /// write nobody asked for. The palette row is how it is reached.
    pub(crate) fn delete_row(
        &mut self,
        _: &DeleteRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Does not route through `editable`: `row_key` answers "can this row be
        // named" and is true regardless of mode or engine, so both are asked
        // outright.
        let target = self.target();
        let engine = target.engine;
        if let Some(reason) = target.unwritable() {
            self.note(reason, cx);
            return;
        }
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        if profile.session.import_rows.is_some() {
            return;
        }
        let tab = profile.session.active;
        // Scoped to the active tab's own form: one left open on another tab
        // (or a closed one, cleared at close) must not block this one.
        if profile
            .session
            .insert_form
            .as_ref()
            .is_some_and(|form| form.tab == profile.session.active)
        {
            self.note("Close the new row form before deleting a row.".into(), cx);
            return;
        }
        // In a Draft view of a table whose rows are written back, Delete row
        // marks the rows in the grid, where the mark is part of the draft and
        // `apply_edits` writes it. Every other view reviews the one `DELETE`.
        if matches!(tab, Tab::Object(_))
            && profile.session.edit_mode(tab) == EditMode::Draft
            && target.keeps_edits()
            && let Some(results) = profile.session.active_results().cloned()
            && results.read(cx).delegate().insert_target().is_some()
        {
            self.toggle_row_marks(tab, results, cx);
            return;
        }
        // Browsing surfaces only. A query tab's grid is a view of the user's own
        // statement, and a delete there would be dbdelve writing into a buffer to
        // destroy rows -- bar an engine that takes them there
        // (`Target::edits_query_rows`), where the delete is reviewed as a
        // relation's is and the buffer is left be.
        if !matches!(tab, Tab::Object(_)) && !target.edits_query_rows() {
            self.note(
                "Deleting a row is offered on a table's rows, not on a query's results.".into(),
                cx,
            );
            return;
        }
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        if self.refuses_in_transaction(cx) {
            return;
        }
        let grid = results.read(cx);
        let unnameable = grid.delegate().read_only_reason().map_or_else(
            || {
                "dbdelve cannot name this row by its primary key, so it will not delete it."
                    .to_string()
            },
            str::to_string,
        );
        let key = grid.delegate().active().and_then(|(row, _)| {
            Some((
                row,
                grid.delegate().row_key(row)?,
                grid.delegate().row_types(row),
            ))
        });
        let Some((row, (schema, table, keys, null_keys), types)) = key else {
            self.note(unnameable, cx);
            return;
        };
        if !self.confirm_stale(StaleResume::Delete, cx) {
            return;
        }

        let borrowed: Vec<(&str, &str)> = keys
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_str()))
            .collect();
        let nulls: Vec<&str> = null_keys.iter().map(String::as_str).collect();
        let Some(statement) =
            sql::delete_row_matching(engine, &schema, &table, &borrowed, &nulls, &types)
        else {
            self.note(unnameable, cx);
            return;
        };
        // Both halves of the admission, before anything is shown: the gate every
        // generated statement passes (`AGENTS.md` rule 2), and the readout it
        // cannot give alone — that the predicate is this row's key and not some
        // other set of columns. Failing either is dbdelve disagreeing with itself,
        // a bug in dbdelve rather than a user error, so it is said and not run.
        let columns: Vec<&str> = borrowed
            .iter()
            .map(|&(column, _)| column)
            .chain(nulls.iter().copied())
            .collect();
        if !sql::is_generated_write_on(engine, &statement)
            || !sql::delete_matches_key_on(engine, &statement, &columns)
        {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not a DELETE of one row \
                 by its primary key."
                    .into(),
                cx,
            );
            return;
        }
        // The row read again before the `DELETE`, which must count one.
        let verify = if target.verifies_applies() {
            let grid = results.read(cx).delegate();
            match lone_verification(
                engine,
                &statement,
                StepKind::Delete,
                Some((grid.fetched_cells(row), grid.row_label(row))),
                None,
            ) {
                Ok(verify) => Some(verify),
                Err(message) => {
                    self.note(message, cx);
                    return;
                }
            }
        } else {
            None
        };
        if !self.review_slot_free(tab, false, window, cx) {
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "Delete row",
                notes: target.review_notes(&statement),
                sql: statement,
                error: None,
                edits: false,
                verify,
            });
        }
        cx.notify();
    }

    /// Delete row in a Draft view: toggle the deletion mark of the rows picked.
    /// Nothing is sent and nothing is asked; the mark is part of the draft.
    ///
    /// Held back as staging a cell is: not while a review holds the draft, whose
    /// statement a mark would no longer describe.
    fn toggle_row_marks(
        &mut self,
        tab: Tab,
        results: Entity<gpui_component::table::TableState<ResultGrid>>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        if !profile.session.may_stage_into(tab) {
            return;
        }
        if !self.confirm_stale(StaleResume::Delete, cx) {
            return;
        }
        let unnameable = results.read(cx).delegate().read_only_reason().map_or_else(
            || {
                "dbdelve cannot name this row by its primary key, so it will not delete it."
                    .to_string()
            },
            str::to_string,
        );
        let marked = results.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            let rows = grid.selected_draft_rows();
            let marked = grid.toggle_marks(&rows);
            table.refresh(cx);
            marked
        });
        if !marked {
            self.note(unnameable, cx);
        }
    }

    /// `cmd+c` on the active cell, whole value and all.
    ///
    /// The one-cell answer beside `export_results`' whole-grid one, and still
    /// the common case: reaching for a file to carry a single value across is
    /// the long way round. It works on every cell, including the ones that can never
    /// open an input — a join, an aggregate, a view, a primary-key column — and
    /// the grid withholds the value while an input is open, where `cmd+c`
    /// belongs to the input's own text selection.
    pub(crate) fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(results) = profile.session.active_results() else {
            return;
        };
        let Some(value) = results
            .read(cx)
            .delegate()
            .active_value()
            .map(str::to_string)
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(value));
    }

    /// `cmd+c` in the grid: the rows the gutter picked, or several selected
    /// any way, and otherwise the active cell, as Copy Cell copies it.
    pub(crate) fn copy_selection(
        &mut self,
        _: &CopySelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        if let Some(item) = results.read(cx).delegate().copy() {
            cx.write_to_clipboard(item);
        }
    }

    /// The active cell's row, whole values under a header, as TSV: what pastes
    /// into a spreadsheet as cells and into a ticket as something readable.
    pub(crate) fn copy_row(&mut self, _: &CopyRow, _: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        let grid = results.read(cx).delegate();
        let Some((row, _)) = grid.active() else {
            return;
        };
        let result = grid.result();
        let (rows, cell_types) = (
            &result.rows[row..=row],
            result.cell_types.get(row..=row).unwrap_or_default(),
        );
        let text = export::render_rows(Format::Tsv, &result.columns, rows, cell_types);
        cx.write_to_clipboard(result_grid::copied_row_slice(
            text,
            true,
            &result.columns,
            rows,
            cell_types,
        ));
    }

    /// The selected rows, or the active cell's alone with nothing selected, in
    /// whichever format the "Copy Rows As" submenu was asked for.
    pub(crate) fn copy_rows(&mut self, action: &CopyRows, _: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        let Some(item) = results.read(cx).delegate().rows_as(action.kind) else {
            return;
        };
        cx.write_to_clipboard(item);
    }

    pub(crate) fn copy_results(&mut self, _: &CopyResults, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_results_as(ClipboardFormat::Tsv, cx);
    }

    /// The export's text on the clipboard rather than in a file, under the same
    /// rules: the rows the server returned, no pending edits, and a capped
    /// snapshot refused rather than copied short.
    pub(crate) fn copy_results_as(&mut self, format: ClipboardFormat, cx: &mut Context<Self>) {
        if !self.has_results(cx) || self.refuse_capped("copying", cx) {
            return;
        }
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        let result = results.read(cx).delegate().result();
        if result.columns.is_empty() {
            return;
        }
        let rows = result.rows.len();
        let text = match format.render(self.engine(), result) {
            Ok(text) => text,
            Err(message) => {
                self.note(message.into(), cx);
                return;
            }
        };
        cx.write_to_clipboard(result_grid::copied_rows(text, result));
        // A copy changes nothing on screen, and a whole result set is too much
        // to take on trust. Unlike a refusal, the confirmation goes by itself.
        self.confirm(
            format!(
                "Copied {} {} as {}.",
                group_thousands(rows as u64),
                if rows == 1 { "row" } else { "rows" },
                format.label()
            ),
            cx,
        );
    }

    /// A notice that goes by itself: see `clear_notice_later`.
    pub(crate) fn confirm(&mut self, message: String, cx: &mut Context<Self>) {
        self.note(message.clone(), cx);
        if let Some(profile) = self.profile() {
            let (id, generation) = (profile.id.clone(), profile.generation);
            self.clear_notice_later(id, generation, message, cx);
        }
    }

    /// Take a confirmation down after a few seconds, the way a refusal is not:
    /// a refusal is still true until the user acts on it, a success is news.
    pub(crate) fn clear_notice_later(
        &self,
        id: String,
        generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |workspace, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            let _ = workspace.update(cx, |workspace, cx| {
                // A later notice owns the status bar now; this timer is not its
                // to clear.
                if let Some(profile) = workspace.issued_to(&id, generation)
                    && profile.session.notice.as_ref() == Some(&message)
                {
                    profile.session.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A restored snapshot holds at most `GRID_ROW_CAP` rows of a larger
    /// result, and a run at most the row limit. Writing those out is quietly
    /// short of what the status bar says the tab is showing, so this says how
    /// to get the rest instead.
    fn refuse_capped(&mut self, doing: &str, cx: &mut Context<Self>) -> bool {
        let Some(short) = self.short(cx) else {
            return false;
        };
        self.note(short.refusal(doing), cx);
        true
    }

    /// How the rows in front fall short of the result they came from, if
    /// they do.
    pub(crate) fn short(&self, cx: &App) -> Option<Short> {
        let grid = self
            .profile()?
            .session
            .active_results()?
            .read(cx)
            .delegate();
        let showing = grid.result().rows.len();
        if grid.streamed_set().is_some() {
            return Some(Short::Streaming);
        }
        if let Some(stopped) = grid.result().stopped {
            return Some(Short::Stopped(stopped, showing));
        }
        (showing < grid.total_rows()).then(|| Short::Capped {
            showing,
            total: grid.total_rows(),
            snapshot: grid.captured().is_some(),
        })
    }

    /// One field of the row panel, taken from the cell's whole value -- its
    /// staged one, where it has an edit pending, as the panel shows -- rather
    /// than the re-indented, clipped text the panel paints. The field's button
    /// shows a tick for a moment after, since a copy otherwise changes nothing
    /// on screen.
    pub(crate) fn copy_row_field(&mut self, row_ix: usize, col_ix: usize, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        let value = results
            .read(cx)
            .delegate()
            .copied_value(row_ix, col_ix)
            .into_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(value));

        let copied = Some((row_ix, col_ix));
        self.row_panel.copied = copied;
        cx.notify();
        cx.spawn(async move |workspace, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = workspace.update(cx, |workspace, cx| {
                // A later copy owns the tick now; this timer is not its to clear.
                if workspace.row_panel.copied == copied {
                    workspace.row_panel.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The plan pane's copy button: the server's plan exactly as it arrived,
    /// unedited by anything dbdelve draws over it. Ticks the same way
    /// `copy_row_field` does, but with one flag rather than a keyed slot,
    /// since only one plan pane is ever on screen at a time.
    pub(crate) fn copy_plan(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
        else {
            return;
        };
        let Some(explained) = &tab.plan else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(explained.plan.text.clone()));

        self.plan_copied = true;
        cx.notify();
        cx.spawn(async move |workspace, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.plan_copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Puts the table in front into `mode`, when its target offers it
    /// (`EditMode::on`). Nothing changes, and nothing is said, for a mode the
    /// view already has or a tab that has none to choose.
    ///
    /// Switching into Row is refused while the grid holds staged edits: they
    /// are a batch for one review, and one switch must not turn them into
    /// sends, one row at a time, that nobody reviewed. Switching into Classic
    /// is refused while the grid holds an added row or a delete mark, which a
    /// Classic view has no way to show or apply. Cell edits carry between
    /// Classic and Draft with no prompt, since both review the same `UPDATE`s.
    /// Switching into Row is refused while the user's transaction is open,
    /// where a Row view sends nothing. Leaving Row over edited rows sends them first, as any action
    /// that leaves them would (`held_for_edits`), and does not switch if one is
    /// refused.
    pub(crate) fn pick_edit_mode(
        &mut self,
        action: &crate::actions::SetEditMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_edit_mode(action.mode, cx);
    }

    pub(crate) fn set_edit_mode(&mut self, mode: EditMode, cx: &mut Context<Self>) {
        let mode = mode.on(self.target());
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        let Some(results) = profile.session.results(tab).cloned() else {
            return;
        };
        let current = profile.session.edit_mode(tab);
        let in_transaction = profile.transaction_open_for(tab);
        if !matches!(tab, Tab::Object(_)) || current == mode {
            return;
        }
        let staged = results.read(cx).delegate().pending_count();
        let holds_rows = results.read(cx).delegate().holds_rows();
        // Leaving Row sends the dirty row first and switches when it lands.
        if current == EditMode::Row && self.held_for_edits(tab, AfterEdits::EditMode(mode), cx) {
            return;
        }
        if (mode == EditMode::Row && staged > 0) || (mode == EditMode::Classic && holds_rows) {
            // Cells alone are edits; a row added makes the draft's size a
            // count of changes.
            let noun = if holds_rows { "change" } else { "edit" };
            let size = match staged {
                1 => format!("1 {noun}"),
                count => format!("{count} {noun}s"),
            };
            self.note(
                format!(
                    "The draft has {size}{}{} mode.",
                    Self::DRAFT_REFUSAL,
                    mode.label()
                ),
                cx,
            );
            return;
        }
        // A Row view sends nothing while the user's transaction is open, so
        // it would only hold every row it was asked to leave.
        if mode == EditMode::Row && in_transaction {
            self.note(
                format!(
                    "A transaction is open on this connection{}{} mode.",
                    Self::DRAFT_REFUSAL,
                    mode.label()
                ),
                cx,
            );
            return;
        }
        if let Some(slot) = self
            .profile_mut()
            .and_then(|profile| profile.session.edit_mode_mut(tab))
        {
            *slot = mode;
            results.update(cx, |table, cx| {
                table
                    .delegate_mut()
                    .set_row_washes(mode != EditMode::Classic);
                table.refresh(cx);
            });
            self.clear_draft_refusal();
            cx.notify();
        }
    }

    /// What the notice says between how many edits the draft holds and the
    /// mode a refused switch was headed for.
    const DRAFT_REFUSAL: &str = ", so the view was not switched to ";

    /// Takes the draft's refusal down once what it reported is over: the draft
    /// is gone or the view is in the mode it asked for. Only that notice goes;
    /// whatever else the bar says, such as the row a Row send just wrote, is
    /// not this call's.
    fn clear_draft_refusal(&mut self) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.notice.as_deref().is_some_and(|notice| {
            notice.starts_with("The draft has ")
                && notice.contains(Self::DRAFT_REFUSAL)
                && notice.ends_with(" mode.")
        }) {
            profile.session.notice = None;
        }
    }

    /// Sends the staged edits of `row` of relation tab `tab` as the one
    /// `UPDATE` of a Row-mode view, with no review. True when the statement was
    /// handed to [`Self::execute_and_then`], which judges it under the mode in
    /// force as it is sent; false when nothing was sent.
    ///
    /// The one writer of a Row send: the statement is written as a draft's
    /// batch is, over one row, and passes the same gate before anything runs
    /// (`AGENTS.md` rule 2). It carries the read of the row by its key, which
    /// is cut from the gated statement (`sql::row_read`), so what the grid shows
    /// afterwards is what the server holds. A tab already sending sends nothing
    /// more: a second Save while the first is out would be the same `UPDATE`
    /// twice.
    ///
    /// `to` is the row the ring moved to when a row leave started the send, and
    /// `None` for Save, which does not move it. A send that fails, or is stopped
    /// by the mode, puts the ring back from `to` onto `row`.
    ///
    /// `then` is the action the user asked for that the row had to be saved
    /// before (`held_for_edits`): carried out when the send lands, dropped on
    /// every other outcome, which puts back what it had changed
    /// (`Session::put_back`).
    pub(crate) fn save_row(
        &mut self,
        tab: u64,
        row: usize,
        to: Option<usize>,
        then: Option<AfterEdits>,
        cx: &mut Context<Self>,
    ) -> bool {
        let tab = Tab::Object(tab);
        let Some(profile) = self.profile() else {
            return false;
        };
        let Some(results) = profile.session.results(tab).cloned() else {
            return false;
        };
        if profile.session.running(tab) {
            return false;
        }
        // A statement waiting in review is the user's to run or close first:
        // this send's outcome would be written into it, and a success would
        // drop it unread.
        if profile.session.apply_review_covers(tab) {
            self.note(
                "The row was not sent: a statement is waiting in review.".into(),
                cx,
            );
            return false;
        }
        let target = self.target();
        if let Some(reason) = target.unwritable() {
            self.note(reason, cx);
            return false;
        }
        let (pending, rows_id, sent) = {
            let grid = results.read(cx).delegate();
            (
                grid.pending_row(row),
                grid.rows_id(),
                grid.pending_cells(row),
            )
        };
        let Some(pending) = pending else {
            return false;
        };
        // The row's edits stay as they are: the last apply may have landed.
        if results.read(cx).delegate().outcome_unknown() {
            results.update(cx, |table, cx| {
                table
                    .delegate_mut()
                    .set_send_failure(row, OUTCOME_UNKNOWN_REFUSAL.into());
                cx.notify();
            });
            self.snap_back_to(tab, row, to, cx);
            return false;
        }
        // The send is verified inside a transaction of its own, whose `BEGIN`
        // and `COMMIT` would end one the user opened, so nothing is sent while
        // one is open.
        if profile.transaction_open_for(tab) {
            results.update(cx, |table, cx| {
                table.delegate_mut().set_send_failure(
                    row,
                    "A transaction is open on this connection; the row was not sent.".into(),
                );
                cx.notify();
            });
            self.snap_back_to(tab, row, to, cx);
            return false;
        }
        let defaulted = pending
            .sets
            .iter()
            .any(|(_, value)| matches!(value, NewValue::Default));
        let update = match update_batch(self.engine(), &[pending]) {
            Ok(update) => update,
            Err(message) => {
                self.note(message, cx);
                return false;
            }
        };
        if !sql::is_generated_write_on(self.engine(), &update) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not an UPDATE.".into(),
                cx,
            );
            return false;
        }
        let Some(read) = sql::row_read(self.engine(), &update) else {
            // A row named by every value is read back by the values it now
            // holds, and one set to DEFAULT holds none the statement spells.
            if defaulted && names_rows_by_every_column(results.read(cx).delegate()) {
                results.update(cx, |table, cx| {
                    table.delegate_mut().set_send_failure(
                        row,
                        "The row was not sent: a value set to DEFAULT \
                         cannot be read back on a row named by every value."
                            .into(),
                    );
                    cx.notify();
                });
                self.snap_back_to(tab, row, to, cx);
                return false;
            }
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not the read of one \
                 row."
                    .into(),
                cx,
            );
            return false;
        };
        // The send is one step: the row read again and compared before the
        // `UPDATE`, the write counted, and `read` taken inside the same
        // transaction. Row is offered only where every apply is verified
        // (`Target::keeps_edits`), so there is no send of the text alone.
        let verify = {
            let grid = results.read(cx).delegate();
            match lone_verification(
                self.engine(),
                &update,
                StepKind::Update,
                Some((grid.fetched_cells(row), grid.row_label(row))),
                Some(read),
            ) {
                Ok(verify) => Some(verify),
                Err(message) => {
                    self.note(message, cx);
                    return false;
                }
            }
        };
        let out = sent.clone();
        self.execute_and_then(
            update,
            tab,
            Some(Refresh::Row {
                row,
                rows_id,
                sent,
                to,
                then,
                verify,
            }),
            false,
            None,
            cx,
        );
        let running = self
            .profile()
            .is_some_and(|profile| profile.session.running(tab));
        // The grid knows the cells are out until the send lands, so that one
        // put back to its fetched value meanwhile stays staged, and Esc leaves
        // the row alone.
        if running {
            results.update(cx, |table, _| table.delegate_mut().begin_send(row, &out));
        }
        // Not sent: the mode stopped it and raised its prompt, or the
        // connection was gone. Nothing went, the row is still dirty, and the
        // ring the user moved off it goes back (`to` is where it moved to).
        if to.is_some() && !running {
            self.snap_back_to(tab, row, to, cx);
        }
        true
    }

    /// Esc on a Row view's grid: puts the ring's row back as it was fetched,
    /// its staged values and the mark of a refused send gone, the other rows'
    /// edits kept. True when it reverted something; false leaves the key to
    /// whatever it did before.
    ///
    /// Only while the grid itself holds the keyboard, so never while a cell's
    /// input does: that Esc cancels the input. Never on a row whose send is
    /// out, which keeps its values until the server answers.
    pub(crate) fn revert_active_row(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile() else {
            return false;
        };
        let tab = profile.session.active;
        if !matches!(tab, Tab::Object(_)) || profile.session.edit_mode(tab) != EditMode::Row {
            return false;
        }
        let Some(results) = profile.session.results(tab).cloned() else {
            return false;
        };
        if !results.focus_handle(cx).is_focused(window) {
            return false;
        }
        let row = {
            let grid = results.read(cx).delegate();
            // The server is writing a row whose send is out, so dropping its
            // values would show it as fetched over what the landing reads back.
            grid.active().map(|(row, _)| row).filter(|row| {
                grid.sending_row() != Some(*row)
                    && (grid.row_has_pending(*row)
                        || grid
                            .send_failure()
                            .is_some_and(|(refused, _)| refused == *row))
            })
        };
        let Some(row) = row else {
            return false;
        };
        results.update(cx, |table, cx| {
            table.delegate_mut().discard_row(row);
            table.refresh(cx);
        });
        if let Some(profile) = self.profile_mut() {
            profile
                .session
                .tab_notice
                .take_if(|(noticed, _)| *noticed == tab);
        }
        cx.notify();
        true
    }

    /// Puts the ring of relation tab `tab` back on `row` when it is still on
    /// `to`, the row it moved to as `row` was left, and `to` holds no edits: a
    /// ring the user has moved on, or into a row of edits, is theirs. Hands the
    /// keyboard back to the grid when an input of `to`'s went with the move.
    pub(crate) fn snap_back_to(
        &mut self,
        tab: Tab,
        row: usize,
        to: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(to) = to else {
            return;
        };
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.results(tab))
            .cloned()
        else {
            return;
        };
        let stays = results.read(cx).delegate().active().map(|(at, _)| at) == Some(to)
            && !results.read(cx).delegate().row_has_pending(to);
        if stays && results.update(cx, |table, cx| result_grid::snap_back(table, row, cx)) {
            self.refocus_front();
        }
        cx.notify();
    }

    /// What a ring move does about the input it closed and the row it left,
    /// called after every move the grid made ([`ResultGrid::take_left_row`]).
    /// The closed input is staged first, in every view and on every grid
    /// ([`ResultGrid::commit_closed_input`]); a Row view then sends the row
    /// left, and a Draft view sends nothing and says nothing. The one caller of
    /// [`Self::save_row`] besides Save.
    ///
    /// A grid that is not a relation tab's of the profile in front (a query
    /// tab's, or one of a profile behind) has no mode to ask: what was typed is
    /// staged and the mark dropped.
    pub(crate) fn flush_left_row(
        &mut self,
        grid: &Entity<gpui_component::table::TableState<ResultGrid>>,
        cx: &mut Context<Self>,
    ) {
        // The input is staged and the left row taken first and whatever comes
        // of them, so a grid with no Row view to ask does not hold either for a
        // later move.
        let (left, ring) = grid.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.commit_closed_input(cx);
            (grid.take_left_row(), grid.active().map(|(row, _)| row))
        });
        let Some(row) = left.filter(|row| Some(*row) != ring) else {
            return;
        };
        let Some((tab, mode)) = self.profile().and_then(|profile| {
            profile
                .session
                .objects
                .iter()
                .find_map(|tab| match &tab.body {
                    ObjectBody::Relation {
                        results, edit_mode, ..
                    } if results == grid => Some((tab.id, *edit_mode)),
                    _ => None,
                })
        }) else {
            return;
        };
        if mode != EditMode::Row {
            return;
        }
        self.send_row_if_idle(tab, grid, row, ring, cx);
    }

    /// The send both ways of leaving a Row view's row end in: nothing when the
    /// row holds no edit, and a note, the row staying staged, while the tab's
    /// last send is still running.
    fn send_row_if_idle(
        &mut self,
        tab: u64,
        grid: &Entity<gpui_component::table::TableState<ResultGrid>>,
        row: usize,
        to: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if !grid.read(cx).delegate().row_has_pending(row) {
            return;
        }
        // One send a tab at a time: this row stays staged, which is as visible
        // as it was, and is sent by the next leave or by Save.
        if self
            .profile()
            .is_some_and(|profile| profile.session.running(Tab::Object(tab)))
        {
            self.note("The previous row is still being saved.".into(), cx);
            return;
        }
        self.save_row(tab, row, to, None, cx);
    }

    /// A step key at the grid's edge with a cell's input open
    /// ([`result_grid::commit_and_step`] found no cell to step to). In a Row view
    /// the key is the row losing the keyboard, which there is leaving the row:
    /// the input commits by Enter's rule and the row is sent as a ring move off
    /// it sends it, with no `to` because the ring did not move. The ring stays
    /// on the cell and the grid has the keyboard; the cell is not reopened. A
    /// mode that refuses the write raises the Read-write prompt as Enter does
    /// and sends nothing.
    ///
    /// A Classic or Draft view (a query tab is Draft) returns before any
    /// commit: the input stays open and nothing is staged. Click-away is what
    /// keeps that text.
    pub(crate) fn step_past_edge(
        &mut self,
        _: &StepPastEdge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab, grid)) = self.profile().and_then(|profile| {
            let tab = profile.session.active_object()?;
            match &tab.body {
                ObjectBody::Relation {
                    results,
                    edit_mode: EditMode::Row,
                    ..
                } => Some((tab.id, results.clone())),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(row) = grid.read(cx).delegate().active().map(|(row, _)| row) else {
            return;
        };
        if !grid.update(cx, |table, cx| {
            result_grid::commit_from_input(table, window, cx)
        }) {
            return;
        }
        self.send_row_if_idle(tab, &grid, row, None, cx);
    }

    /// Turn the grid's pending edits into SQL and put it where the user can read
    /// it: the query tab's buffer, or the review modal on a relation tab and on
    /// a query tab whose target reviews them (`Target::reviews_query_apply`).
    ///
    /// Binds to `cmd+s` -- a deliberate departure from `cmd+enter` ("run the
    /// statement under the cursor"), which stays unrelated so neither
    /// keystroke can be mistaken for the other.
    pub(crate) fn apply_edits(
        &mut self,
        _: &ApplyEdits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_covers() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        // Asked of the profile as it is now, not of the grid: its rows, and the
        // `edit` that opened its cells, may have been read through a driver
        // the profile has since left.
        let target = self.target();
        if let Some(reason) = target.unwritable() {
            self.note(reason, cx);
            return;
        }
        // A Row view sends one row's edits as they are saved, with no review.
        // Whichever row Save sends, the ring's first, and none at all when
        // nothing is staged.
        if let Tab::Object(object) = tab
            && profile.session.edit_mode(tab) == EditMode::Row
        {
            if let Some(row) = results.read(cx).delegate().row_to_send() {
                self.save_row(object, row, None, None, cx);
            }
            return;
        }
        // A draft is applied again only once the rows are read: the last
        // `COMMIT` may have landed, and a second would write it twice.
        if results.read(cx).delegate().outcome_unknown() {
            self.note(OUTCOME_UNKNOWN_REFUSAL.into(), cx);
            return;
        }
        let own_review = profile.session.apply_review_covers(tab);
        // A verified apply opens and ends a transaction of its own, whose
        // `BEGIN` and `COMMIT` would end one the user opened, so nothing is
        // written while one is open; the draft is kept for when it ends.
        if self.refuses_in_transaction(cx) {
            return;
        }
        // A draft holding an added row or a marked one is written by the
        // draft's own writer, for a relation tab alone: only there can a row be
        // added or marked.
        if matches!(tab, Tab::Object(_)) && results.read(cx).delegate().holds_rows() {
            self.apply_draft(tab, &results, window, cx);
            return;
        }
        let pending = results.read(cx).delegate().pending_updates();
        // Nothing partial runs: a batch missing one of its rows is not the
        // change the user made.
        let batch = match update_batch(self.engine(), &pending) {
            Ok(batch) => batch,
            Err(message) => {
                self.note(message, cx);
                return;
            }
        };
        // The gate every generated statement passes before anything executes
        // (`AGENTS.md` rule 2). Failing it means dbdelve wrote something outside
        // the shapes the gate names, which is a bug in dbdelve rather than a user
        // error.
        if !sql::is_generated_write_on(self.engine(), &batch) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not an UPDATE.".into(),
                cx,
            );
            return;
        }

        let notes = target.review_notes(&batch);
        // What the review's run is checked against, where the target verifies
        // an apply: the rows as the grid fetched them, one step to a statement.
        // A query tab's is built the same way, by the real column names its
        // edit target gives the projection. Not where its batch goes to the
        // buffer, which has nothing to check it against.
        let verified = target.verifies_applies()
            && match tab {
                Tab::Object(_) => true,
                Tab::Query(_) => target.reviews_query_apply(),
            };
        let verify = if verified {
            match verification(self.engine(), results.read(cx).delegate(), &batch) {
                Ok(verify) => Some(verify),
                Err(message) => {
                    self.note(message, cx);
                    return;
                }
            }
        } else {
            None
        };
        match tab {
            Tab::Query(_) if !target.reviews_query_apply() => {
                if !own_review {
                    self.apply_in_buffer(batch, window, cx);
                }
            }
            _ if !self.review_slot_free(tab, true, window, cx) => {}
            _ => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.apply_review = Some(ApplyReview {
                        tab,
                        title: "Apply edits",
                        notes,
                        sql: batch,
                        error: None,
                        edits: true,
                        verify,
                    });
                    cx.notify();
                }
            }
        }
    }

    /// Write the draft of relation tab `tab`, which holds an added row, as one
    /// batch and put it in an `Apply draft` review. Nothing runs here: Run in
    /// the review is the ask (`AGENTS.md` rule 1).
    ///
    /// The batch passes both checks a generated draft is held to before it is
    /// shown: the gate's read of its shape and the key check's read of which
    /// table and key every `DELETE` names, against the rows marked.
    fn apply_draft(
        &mut self,
        tab: Tab,
        results: &Entity<gpui_component::table::TableState<ResultGrid>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let target = self.target();
        let (inserts, updates, deletes, marks, keys, relation) = {
            let grid = results.read(cx).delegate();
            (
                grid.draft_inserts(),
                grid.pending_updates(),
                grid.draft_deletes(),
                grid.marked_count(),
                grid.key_columns(),
                grid.insert_target(),
            )
        };
        let inserts = match inserts {
            Ok(inserts) => inserts,
            Err(row) => {
                self.note(
                    format!(
                        "The added row {} holds no value, so the draft was not applied.",
                        row + 1
                    ),
                    cx,
                );
                return;
            }
        };
        let Some((schema, table, _)) = relation else {
            return;
        };
        let batch = match sql::draft_batch(engine, &deletes, &updates, &inserts) {
            Ok(batch) => batch,
            Err(message) => {
                self.note(message, cx);
                return;
            }
        };
        // Failing either check is dbdelve disagreeing with itself, a bug in
        // dbdelve rather than a user error, so it is said and not run.
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        if !sql::is_generated_write_on(engine, &batch)
            || sql::draft_deletes_match(engine, &batch, &keys, &schema, &table) != Some(marks)
        {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not a draft of \
                 one-row deletes, updates and inserts."
                    .into(),
                cx,
            );
            return;
        }
        let verify = if target.verifies_applies() {
            match verification(engine, results.read(cx).delegate(), &batch) {
                Ok(verify) => Some(verify),
                Err(message) => {
                    self.note(message, cx);
                    return;
                }
            }
        } else {
            None
        };
        if !self.review_slot_free(tab, true, window, cx) {
            return;
        }
        let notes = target.review_notes(&batch);
        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "Apply draft",
                notes,
                sql: batch,
                error: None,
                edits: true,
                verify,
            });
            cx.notify();
        }
    }

    /// The query tab: the batch is appended to the user's buffer, runs from
    /// there, and the `SELECT` that produced the grid runs after it.
    ///
    /// The append is what keeps a failure readable: the statement that was
    /// attempted is still in the buffer. On a target that answers
    /// `Target::keeps_edits` a failure leaves the edits staged as well; elsewhere
    /// `execute_sql` clears the grid as it starts and they go with it.
    pub(crate) fn apply_in_buffer(
        &mut self,
        batch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let (id, editor) = (tab.id, tab.editor.clone());
        let Some(refresh) = tab.apply_refresh() else {
            self.note(
                "dbdelve does not know which statement produced these rows.".into(),
                cx,
            );
            return;
        };

        let text = editor.read(cx).value().to_string();
        let appended = appended_statement(self.engine(), &text, &batch);
        editor.update(cx, |editor, cx| {
            queries::rewrite(editor, &appended, window, cx)
        });
        self.execute_and_then(batch, Tab::Query(id), Some(refresh), false, None, cx);
    }

    /// Run the batch a relation tab is showing. The modal stays up until it
    /// succeeds, so a failure leaves the statement on screen.
    pub(crate) fn run_apply_review(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(review) = &profile.session.apply_review else {
            return;
        };
        let (tab, sql) = (review.tab, review.sql.clone());
        // Cloned, never taken: a refusal leaves the review open and the next
        // Run reads the rows again.
        let verify = review.verify.clone();
        // Without the refresh the grid would show the UPDATE's empty result set
        // and the user would watch their table vanish.
        let refresh = match tab {
            Tab::Object(id) => Refresh::Relation { id, verify },
            Tab::Query(id) => match profile
                .session
                .query_tab(id)
                .and_then(QueryTab::apply_refresh)
            {
                Some(refresh) => refresh.verified(verify),
                None => {
                    self.note(
                        "dbdelve does not know which statement produced these rows.".into(),
                        cx,
                    );
                    return;
                }
            },
        };
        // A second Run starts clean: what the first came to is not this one's.
        if let Some(profile) = self.profile_mut()
            && let Some(review) = &mut profile.session.apply_review
        {
            review.error = None;
        }
        self.execute_and_then(sql, tab, Some(refresh), false, None, cx);
    }

    /// Put the batch away, leaving the edits pending: reading a statement and
    /// deciding not to run it is not the same as throwing the edits out, which
    /// is what Discard is for.
    ///
    /// Scoped to the active tab, the only one it is drawn over, as
    /// `close_new_row` is: one left on another tab must not eat the `escape`
    /// meant for what this tab has in front.
    pub(crate) fn close_apply_review(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        if !profile.session.apply_review_covers(profile.session.active) {
            return false;
        }
        let closed = profile.session.apply_review.take().is_some();
        if closed {
            cx.notify();
        }
        closed
    }

    /// Whether a review written from `from` may be put in front of the user.
    ///
    /// Refused while any review is open, another tab's included: the session
    /// holds one, and a second would overwrite the first's statement unseen.
    /// Another tab's is brought forward, so what is in the way is on screen.
    /// `from`'s own is replaced only when `replaces_own`, the new statement
    /// then being its newer one.
    ///
    /// Asked last, once the creator has nothing left of its own to refuse:
    /// a creator that would have refused anyway must not move the user.
    fn review_slot_free(
        &mut self,
        from: Tab,
        replaces_own: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        match profile.session.review_slot(from, replaces_own) {
            ReviewSlot::Free | ReviewSlot::Replace => true,
            ReviewSlot::Stale => {
                profile.session.apply_review = None;
                true
            }
            ReviewSlot::Refuse => false,
            ReviewSlot::JumpTo(holder) => {
                self.activate_tab(holder, window, cx);
                self.note(
                    "Run or close this statement before starting another.".into(),
                    cx,
                );
                false
            }
        }
    }

    /// Back to exactly the rows the server sent.
    pub(crate) fn discard_edits(
        &mut self,
        _: &DiscardEdits,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab, results)) = self.profile().and_then(|profile| {
            let results = profile.session.active_results()?.clone();
            Some((profile.session.active, results))
        }) else {
            return;
        };
        results.update(cx, |table, cx| {
            table.delegate_mut().discard_pending();
            table.refresh(cx);
        });
        if let Some(profile) = self.profile_mut() {
            profile.session.drop_apply_review_of(tab);
        }
        self.clear_draft_refusal();
        cx.notify();
    }

    /// Ring the next pending edit, so a batch can be read over before it is
    /// applied.
    pub(crate) fn next_edit(&mut self, _: &NextEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.step_pending_edit(false, window, cx);
    }

    pub(crate) fn previous_edit(
        &mut self,
        _: &PreviousEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_pending_edit(true, window, cx);
    }

    fn step_pending_edit(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        results.update(cx, |table, cx| {
            result_grid::step_pending(table, back, window, cx)
        });
    }
}

/// What a typed page names, counted from zero the way an offset is. Pages are
/// counted from one where they are read and written, so a zero is as much a
/// non-page as a word is.
fn typed_page(typed: &str) -> Option<usize> {
    match typed.parse::<usize>() {
        Ok(page) if page > 0 => Some(page - 1),
        _ => None,
    }
}

/// Why the rows a grid holds are not every row of its result.
pub(crate) enum Short {
    /// Rows are still arriving.
    Streaming,
    /// The statement was stopped at the row limit or by a cancel, with this
    /// many rows in.
    Stopped(db::Stopped, usize),
    /// The row limit or a snapshot kept `showing` of `total`.
    Capped {
        showing: usize,
        total: usize,
        snapshot: bool,
    },
    /// A relation tab's page, starting `offset` rows in.
    Page { showing: usize, offset: usize },
}

impl Short {
    /// The rows on screen, while none are still arriving.
    pub(crate) fn showing(&self) -> Option<usize> {
        match self {
            Short::Streaming => None,
            Short::Stopped(_, showing)
            | Short::Capped { showing, .. }
            | Short::Page { showing, .. } => Some(*showing),
        }
    }

    /// What the tab is showing, as one sentence.
    pub(crate) fn situation(&self) -> String {
        let count = |rows: usize| group_thousands(rows as u64);
        match *self {
            Short::Streaming => "Rows are still arriving.".to_string(),
            Short::Stopped(db::Stopped::AtLimit, showing) | Short::Page { showing, offset: 0 } => {
                format!(
                    "This tab is showing the first {} rows, the row limit.",
                    count(showing)
                )
            }
            Short::Stopped(db::Stopped::Cancelled, showing) => format!(
                "This tab is showing the {} rows that arrived before the statement was cancelled.",
                count(showing)
            ),
            Short::Capped {
                showing,
                total,
                snapshot: true,
            } => format!(
                "This tab is showing {} of {} rows from a snapshot.",
                count(showing),
                count(total)
            ),
            Short::Capped {
                showing,
                total,
                snapshot: false,
            } => format!(
                "This tab is showing {} of {} rows, the row limit.",
                count(showing),
                count(total)
            ),
            Short::Page { showing, offset } => format!(
                "This tab is showing a page of {} rows, from row {}.",
                count(showing),
                count(offset + 1)
            ),
        }
    }

    /// [`Short::situation`], and what to do about it before `doing`.
    pub(crate) fn refusal(&self, doing: &str) -> String {
        let remedy = match self {
            Short::Streaming => {
                return format!(
                    "Rows are still arriving. Wait for the statement to finish before {doing}."
                );
            }
            Short::Stopped(db::Stopped::Cancelled, _) => "Run it again",
            Short::Capped { snapshot: true, .. } => "Refresh it",
            Short::Stopped(db::Stopped::AtLimit, _)
            | Short::Capped {
                snapshot: false, ..
            }
            | Short::Page { .. } => "Raise it and run the statement again",
        };
        format!("{} {remedy} before {doing}.", self.situation())
    }
}

/// What Apply edits, Save and the review's Run say while a grid's last apply
/// may have landed: the draft is sent again only once the rows are read.
pub(super) const OUTCOME_UNKNOWN_REFUSAL: &str = "Not applied: whether the last apply reached the server is not known until the rows are read again.";

/// Whether `grid` names the rows it writes back by every column they were read
/// with, as a table with no primary key does. Asked positively, so a grid with
/// no edit target says no.
fn names_rows_by_every_column(grid: &ResultGrid) -> bool {
    grid.result()
        .edit
        .as_ref()
        .is_some_and(|edit| matches!(edit.row_match, db::RowMatch::EveryColumn { .. }))
}

/// The sentence for a draft whose statements cannot be paired with its rows.
const UNPAIRED: &str = "dbdelve refused to run a statement it wrote itself: the draft and its statements do not match.";

/// What a reviewed draft's run is verified against: `batch`'s statements, cut
/// from the text the gate admitted, each paired with the row it was written for
/// ([`ResultGrid::draft_order`]), the row as the grid fetched it, and the read
/// that brings it back ([`sql::row_reread`]).
///
/// Refused, so nothing is reviewed, when the statements and the draft do not
/// pair up, or when a row an `UPDATE` or `DELETE` names has no re-read or no
/// name: an apply that cannot be verified is not offered for a target that
/// verifies.
pub(super) fn verification(
    engine: Engine,
    grid: &ResultGrid,
    batch: &str,
) -> Result<Verification, String> {
    let statements = sql::generated_writes(engine, batch).ok_or(UNPAIRED)?;
    let order = grid.draft_order();
    let identities = grid.draft_identities();
    if statements.len() != order.len() || identities.len() != order.len() {
        return Err(UNPAIRED.into());
    }
    let (mut steps, mut rows) = (Vec::new(), Vec::new());
    for (statement, row) in statements.into_iter().zip(order) {
        let (kind, fetched) = match row {
            DraftRow::Marked(row) => (StepKind::Delete, Some(row)),
            DraftRow::Updated(row) => (StepKind::Update, Some(row)),
            DraftRow::Added(_) => (StepKind::Insert, None),
        };
        let (reread, before, label) = match (row, fetched) {
            (DraftRow::Added(index), _) => (None, Vec::new(), format!("added row {}", index + 1)),
            (_, Some(fetched)) => (
                Some(sql::row_reread(engine, &statement).ok_or(UNPAIRED)?),
                grid.fetched_cells(fetched),
                grid.row_label(fetched).ok_or(UNPAIRED)?,
            ),
            (_, None) => return Err(UNPAIRED.into()),
        };
        steps.push(VerifiedStep {
            sql: statement,
            kind,
            reread,
            before,
            read_back: None,
        });
        rows.push(label);
    }
    Ok(Verification {
        steps,
        rows,
        identities,
    })
}

/// The sentence for a statement whose write cannot be checked against the
/// server: it is not one statement of the kind it was written as.
const UNCHECKABLE: &str =
    "dbdelve refused to run a statement it wrote itself: it cannot be checked against the server.";

/// [`verification`] for the one statement of a Row send, a New row form's
/// `INSERT` or a lone Delete row's `DELETE`: one step, cut from the text the
/// gate admitted. An `UPDATE` or a `DELETE` is paired with `row`, the row as
/// it was fetched (`ResultGrid::fetched_cells`) and what it is called
/// (`ResultGrid::row_label`), and with the read that brings it back; an
/// `INSERT` has no row to read and is called `new row` in what the apply says
/// of it.
/// `read_back` is the read a Row send takes of its row inside the transaction.
///
/// Refused, so nothing is sent, when the statement is not exactly one of the
/// kind named or its row cannot be read again or named: an apply that cannot
/// be verified is not offered for a target that verifies.
fn lone_verification(
    engine: Engine,
    statement: &str,
    kind: StepKind,
    row: Option<(Vec<db::FetchedCell>, Option<String>)>,
    read_back: Option<String>,
) -> Result<Verification, String> {
    let written = sql::generated_writes(engine, statement);
    let Some([one]) = written.as_deref() else {
        return Err(UNCHECKABLE.into());
    };
    let (reread, before, label) = match (kind, row) {
        (StepKind::Insert, None) => (None, Vec::new(), "new row".to_string()),
        (StepKind::Update | StepKind::Delete, Some((before, label))) => (
            Some(sql::row_reread(engine, one).ok_or(NOT_ONE_ROW)?),
            before,
            label.ok_or(NOT_ONE_ROW)?,
        ),
        (StepKind::Insert | StepKind::Update | StepKind::Delete, _) => {
            return Err(UNCHECKABLE.into());
        }
    };
    Ok(Verification {
        steps: vec![VerifiedStep {
            sql: one.clone(),
            kind,
            reread,
            before,
            read_back,
        }],
        rows: vec![label],
        identities: Vec::new(),
    })
}

/// The sentence for a row write whose row cannot be read by its key.
const NOT_ONE_ROW: &str =
    "dbdelve refused to run a statement it wrote itself: it is not the read of one row.";

#[cfg(test)]
mod tests {
    use super::{lone_verification, typed_page};
    use crate::{db::Engine, db::StepKind, sql};

    /// A lone `INSERT` and a lone `DELETE` the writers produce are accepted by
    /// `lone_verification` on every engine that verifies an apply, since a
    /// refusal there is a New row form or a Delete row that stops working.
    #[test]
    fn the_new_row_insert_and_the_delete_rows_delete_are_accepted_as_one_step() {
        for (engine, types) in [
            (Engine::Sqlite, [("id", "integer"), ("name", "text")]),
            (Engine::DuckDb, [("id", "INTEGER"), ("name", "VARCHAR")]),
            (Engine::Postgres, [("id", "int4"), ("name", "text")]),
        ] {
            let types: Vec<(String, String)> = types
                .iter()
                .map(|(column, kind)| (column.to_string(), kind.to_string()))
                .collect();
            let insert = sql::insert_row(
                engine,
                "public",
                "birds",
                &[("id", Some("3")), ("name", Some("robin"))],
                &types,
            )
            .expect("the form's INSERT is written");
            let verify = lone_verification(engine, &insert, StepKind::Insert, None, None)
                .unwrap_or_else(|why| panic!("{engine:?}: {insert}: {why}"));
            assert_eq!(verify.rows, ["new row"]);

            let delete =
                sql::delete_row_matching(engine, "public", "birds", &[("id", "2")], &[], &types)
                    .expect("Delete row's DELETE is written");
            let verify = lone_verification(
                engine,
                &delete,
                StepKind::Delete,
                Some((Vec::new(), Some("id = 2".into()))),
                None,
            )
            .unwrap_or_else(|why| panic!("{engine:?}: {delete}: {why}"));
            assert_eq!(verify.steps.len(), 1);
            assert!(verify.steps[0].reread.is_some(), "{engine:?}: {delete}");
        }
    }

    #[test]
    fn a_typed_page_is_the_offset_it_names() {
        assert_eq!(typed_page("1"), Some(0));
        assert_eq!(typed_page("20"), Some(19));
        assert_eq!(typed_page("0"), None);
        assert_eq!(typed_page("-1"), None);
        assert_eq!(typed_page("twenty"), None);
        assert_eq!(typed_page(""), None);
    }
}

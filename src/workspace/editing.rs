//! Changing rows: inserting, editing, nulling, deleting, paging, sorting and exporting.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::session::{StaleEdit, StaleResume};

impl Workspace {
    /// Open the "New row" form over the preview in front, one field per column
    /// of the relation's loaded structure (spec §4).
    ///
    /// Requires a schema and a table and not a primary key, which is why this
    /// is offered on relations the grid refuses to edit.
    pub(crate) fn new_row(&mut self, _: &NewRow, window: &mut Window, cx: &mut Context<Self>) {
        // Does not route through `editable`: an insert has no existing row to
        // check a column of, so the mode is asked outright.
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let Tab::Object(id) = profile.session.active else {
            return;
        };
        let Some(tab) = profile.session.objects.iter().find(|tab| tab.id == id) else {
            return;
        };
        let ObjectBody::Relation { structure, .. } = &tab.body else {
            return;
        };
        // The columns are the form: without them there is nothing to draw, and
        // guessing at them would be inventing a table.
        let StructureState::Loaded(structure) = structure else {
            self.note(
                "DBDelve has not read this relation's columns yet.".into(),
                cx,
            );
            return;
        };
        let (schema, table) = (tab.schema.clone(), tab.name.clone());
        let columns: Vec<(String, String)> = structure
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.data_type.clone()))
            .collect();

        let mut fields = Vec::with_capacity(columns.len());
        for (index, (column, data_type)) in columns.into_iter().enumerate() {
            let input = cx.new(|cx| InputState::new(window, cx));
            // The first keystroke is what makes this field a value rather than
            // an omission, so the change event is where `touched` is set.
            cx.subscribe(&input, move |workspace, _, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) {
                    workspace.touch_insert_field(index);
                }
            })
            .detach();
            fields.push(InsertField {
                column,
                data_type,
                input,
                nulled: false,
                touched: false,
            });
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.insert_form = Some(InsertForm {
                tab: Tab::Object(id),
                schema,
                table,
                fields,
            });
        }
        cx.notify();
    }

    pub(crate) fn touch_insert_field(&mut self, index: usize) {
        if let Some(profile) = self.profile_mut()
            && let Some(form) = &mut profile.session.insert_form
            && let Some(field) = form.fields.get_mut(index)
        {
            field.touched = true;
        }
    }

    pub(crate) fn toggle_insert_null(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut()
            && let Some(form) = &mut profile.session.insert_form
            && let Some(field) = form.fields.get_mut(index)
        {
            field.nulled = !field.nulled;
            cx.notify();
        }
    }

    /// Generate the `INSERT` and show it. **Nothing runs here** — Run in the
    /// review panel is the ask (`AGENTS.md` rule 1).
    pub(crate) fn confirm_new_row(&mut self, cx: &mut Context<Self>) {
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
        let borrowed: Vec<(&str, Option<&str>)> = filled
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_deref()))
            .collect();

        let Some(statement) = sql::insert_row(engine, &schema, &table, &borrowed) else {
            self.note("There is nothing in this row to insert.".into(), cx);
            return;
        };
        // The gate every generated statement passes before anything executes
        // (`AGENTS.md` rule 2). Failing it is dbdelve disagreeing with itself --
        // a bug in dbdelve rather than a user error -- so it is said and not run.
        if !sql::is_generated_write(&statement) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not an INSERT.".into(),
                cx,
            );
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.insert_form = None;
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "New row",
                sql: statement,
            });
        }
        cx.notify();
    }

    /// Put the form away. Nothing has been generated yet, so there is nothing
    /// to keep.
    pub(crate) fn close_new_row(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
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
            true => matches!(query, QueryState::Complete { rows, .. } if *rows >= *limit),
            false => *offset > 0,
        };
        if !can_turn {
            return;
        }
        self.requery_relation(
            id,
            move |_, _, limit, offset| {
                match forward {
                    true => *offset += *limit,
                    false => *offset = offset.saturating_sub(*limit),
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
        if typed.is_empty() {
            return;
        }
        let Some(page) = typed_page(&typed) else {
            self.note(format!("{typed} is not a page number."), cx);
            return;
        };
        self.requery_relation(
            id,
            move |_, _, limit, offset| {
                *offset = page * *limit;
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
                ObjectBody::Relation { limit, offset, .. } => Some(offset / limit + 1),
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

    /// A column header was clicked: put that column into the statement's
    /// `ORDER BY` and run it again.
    ///
    /// The sort is the server's, not the grid's. Ordering the rows already
    /// fetched would sort one page of a table and call it sorted; asking the
    /// database means the top of the sort is the table's, not the page's.
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

        match profile.session.active {
            Tab::Object(id) => self.relation_sort(id, column, cx),
            Tab::Query(_) => self.query_sort(column, window, cx),
        }
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
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let results = tab.results.clone();

        let Some(expression) =
            sort_expression(engine, results.read(cx).delegate().columns(), column)
        else {
            return;
        };

        let (text, cursor) = {
            let editor = editor.read(cx);
            (editor.value().to_string(), editor.cursor())
        };
        let Some(range) = Buffer::parse(&text).statement_at(cursor) else {
            return;
        };
        let statement = &text[range.clone()];

        let Some(mut keys) = sql::order_by(statement) else {
            self.note(
                "dbdelve cannot add an ORDER BY to this statement without rewriting it.".into(),
                cx,
            );
            return;
        };
        cycle(&mut keys, &expression);
        let Some(sorted) = sql::with_order_by(statement, &keys) else {
            self.note("This statement cannot carry an ORDER BY.".into(), cx);
            return;
        };

        let mut replaced = text.clone();
        replaced.replace_range(range, &sorted);
        editor.update(cx, |editor, cx| editor.set_value(replaced, window, cx));
        self.execute_sql(sorted, Tab::Query(tab_id), cx);
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
        if profile.session.apply_review.is_some() {
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
        let (traced, snapshot) = {
            let grid = results.read(cx).delegate();
            (
                (0..grid.columns().len()).any(|col| grid.editable(row, col)),
                grid.captured().is_some(),
            )
        };
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
            match (traced, wrote, snapshot) {
                (true, _, _) => "This column cannot be edited.".into(),
                (false, true, _) => {
                    "These rows came from a statement that writes. Fetch them with a SELECT to edit them.".into()
                }
                (false, false, true) => match tab {
                    Tab::Query(_) => {
                        "These rows are from an earlier session. Run the query again to edit them."
                            .into()
                    }
                    _ => "These rows are from an earlier session. Refresh them to edit.".into(),
                },
                (false, false, false) => {
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
    /// an input cannot produce: committing an emptied input on a cell that came
    /// back empty is the no-op `set_pending` drops.
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

    /// The one body behind all three, so the mode prompt and the refusal cannot
    /// drift between them.
    fn stage_value(&mut self, value: NewValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        // The gate `edit_cell` holds, for its reason: a batch already on screen
        // was generated from the pending set as it stood.
        if profile.session.apply_review.is_some() {
            return;
        }
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        let Some((row, col)) = results.read(cx).delegate().active() else {
            return;
        };
        if results.read(cx).delegate().editable(row, col)
            && !self.confirm_stale(StaleResume::Stage(value.clone()), cx)
        {
            return;
        }
        if results.update(cx, |table, cx| {
            let staged = table.delegate_mut().stage(row, col, value);
            cx.notify();
            staged
        }) {
            // The input this just closed had focus, and a window with nothing
            // focused has no dispatch path at all.
            results.focus_handle(cx).focus(window, cx);
            return;
        }
        // Asked after the attempt rather than before it, so staging a value the
        // cell already holds -- which changes nothing -- does not ask a
        // Read-only connection to raise its mode for it.
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.note("This column cannot be edited.".into(), cx);
    }

    /// Whether an edit may go ahead on the grid in front, raising the stale-rows
    /// prompt when it may not. Callers ask only once the grid would otherwise
    /// take the edit, so the prompt never stands in front of a refusal.
    fn confirm_stale(&mut self, resume: StaleResume, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let Some(results) = profile.session.active_results().cloned() else {
            return false;
        };
        if !results.read(cx).delegate().unconfirmed() {
            return true;
        }
        if profile.confirmed_stale {
            results.update(cx, |table, _| table.delegate_mut().confirm_stale());
            return true;
        }
        profile.session.stale_edit = Some(StaleEdit {
            resume,
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
            Some((id, select)) => self.execute_sql(select, Tab::Query(id), cx),
            None => self.run_query(&RunQuery, window, cx),
        }
    }

    /// Accept the rows as they are and carry on with the edit that asked.
    pub(crate) fn edit_stale_anyway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(StaleEdit { resume, dont_ask }) = profile.session.stale_edit.take() else {
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
            StaleResume::Stage(value) => self.stage_value(value, window, cx),
            StaleResume::Delete => self.delete_row(&DeleteRow, window, cx),
        }
        cx.notify();
    }

    /// The grid's own commit was refused by the mode. It has nowhere to say so
    /// and no way to offer the one thing that answers it, so the input stays
    /// open behind this prompt and `enter` again commits what was typed.
    pub(crate) fn request_write_mode(
        &mut self,
        _: &RequestWriteMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.require(Mode::ReadWrite, cx);
    }

    /// Generate the one-row `DELETE` and show it. **Nothing runs here** — Run in
    /// the review panel is the ask (`AGENTS.md` rule 1, spec §5).
    ///
    /// Deliberately not on a keybinding, for the reason [`Workspace::apply_edits`]
    /// is not: a destructive write one fat finger from a navigation key is a
    /// write nobody asked for. The palette row is how it is reached.
    pub(crate) fn delete_row(&mut self, _: &DeleteRow, _: &mut Window, cx: &mut Context<Self>) {
        // Does not route through `editable`: `row_key` answers "can this row be
        // named" and is true regardless of mode, so the mode is asked outright.
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.clear_notice();
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        // A statement already generated is the one the user is reading; another
        // behind it would leave that panel describing something else.
        if profile.session.apply_review.is_some() || profile.session.insert_form.is_some() {
            return;
        }
        // Browsing surfaces only. A query tab's grid is a view of the user's own
        // statement, and a delete there would be dbdelve writing into a buffer to
        // destroy rows.
        let tab = profile.session.active;
        if !matches!(tab, Tab::Object(_)) {
            self.note(
                "Deleting a row is offered on a table's rows, not on a query's results.".into(),
                cx,
            );
            return;
        }
        let Some(results) = profile.session.active_results() else {
            return;
        };
        let grid = results.read(cx);
        let key = grid
            .delegate()
            .active()
            .and_then(|(row, _)| grid.delegate().row_key(row));
        let Some((schema, table, keys)) = key else {
            self.note(
                "dbdelve cannot name this row by its primary key, so it will not delete it.".into(),
                cx,
            );
            return;
        };
        if !self.confirm_stale(StaleResume::Delete, cx) {
            return;
        }

        let borrowed: Vec<(&str, &str)> = keys
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_str()))
            .collect();
        let Some(statement) = sql::delete_row(engine, &schema, &table, &borrowed) else {
            self.note(
                "dbdelve cannot name this row by its primary key, so it will not delete it.".into(),
                cx,
            );
            return;
        };
        // Both halves of the admission, before anything is shown: the gate every
        // generated statement passes (`AGENTS.md` rule 2), and the readout it
        // cannot give alone — that the predicate is this row's key and not some
        // other set of columns. Failing either is dbdelve disagreeing with itself,
        // a bug in dbdelve rather than a user error, so it is said and not run.
        let columns: Vec<&str> = borrowed.iter().map(|&(column, _)| column).collect();
        if !sql::is_generated_write(&statement) || !sql::delete_matches_key(&statement, &columns) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not a DELETE of one row \
                 by its primary key."
                    .into(),
                cx,
            );
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "Delete row",
                sql: statement,
            });
        }
        cx.notify();
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

    /// One field of the row panel, taken from the fetched cell rather than the
    /// re-indented, clipped text the panel paints. The field's button shows a
    /// tick for a moment after, since a copy otherwise changes nothing on
    /// screen.
    pub(crate) fn copy_row_field(&mut self, row_ix: usize, col_ix: usize, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
        else {
            return;
        };
        let Some(value) = results
            .read(cx)
            .delegate()
            .cell(row_ix, col_ix)
            .map(str::to_string)
        else {
            return;
        };
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

    /// Write the result set in front of the user to a file they pick.
    ///
    /// The rows on screen and only those. A relation tab holds what its
    /// row-limit chip asked for, and an export that quietly re-fetched the whole
    /// table behind that chip would make the number on it a lie — the tab has no
    /// buffer to show a larger statement in, so nothing would be on screen to
    /// read it off. Pending edits are not written either: this is the result set
    /// the server returned, and applying them is a separate, visible act.
    pub(crate) fn export_results(&mut self, format: Format, cx: &mut Context<Self>) {
        // A restored snapshot holds at most `GRID_ROW_CAP` rows of a larger
        // result. Exporting those is a file that is quietly short of what the
        // status bar says the tab is showing, so it refuses and says how to get
        // the rest -- rather than writing a wrong file successfully.
        let capped = self.profile().and_then(|profile| {
            let grid = profile.session.active_results()?.read(cx).delegate();
            let showing = grid.result().rows.len();
            (showing < grid.total_rows()).then(|| (showing, grid.total_rows()))
        });
        if let Some((showing, total)) = capped {
            self.note(
                format!(
                    "This tab is showing {} of {} rows from a snapshot. Refresh it before exporting.",
                    group_thousands(showing as u64),
                    group_thousands(total as u64)
                ),
                cx,
            );
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(results) = profile.session.active_results() else {
            return;
        };
        // ponytail: the whole result set is copied here, on the frame thread,
        // before the panel even opens -- a wasted copy if the user cancels. It
        // is taken now rather than after the await because the panel is
        // modeless: what the user was looking at when they asked is the only
        // unambiguous answer to what they asked to export. `ResultGrid` holding
        // its `QueryResult` behind an `Arc` is the upgrade path if a large
        // export is ever seen to stutter.
        let result = results.read(cx).delegate().result().clone();
        if result.columns.is_empty() {
            return;
        }

        // What the tab calls itself, so the file lands named after the thing the
        // user was looking at rather than after the statement that built it.
        let stem = match profile.session.active_object() {
            Some(tab) => tab.name.clone(),
            None => profile
                .session
                .open_query()
                .map(str::to_string)
                .unwrap_or_else(|| "results".to_string()),
        };
        let id = profile.id.clone();
        let generation = profile.generation;
        let rows = result.rows.len();
        let suggested = format!("{stem}.{}", format.extension());
        let directory = crate::store::home().unwrap_or_else(|_| PathBuf::from("/"));
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));

        cx.spawn(async move |workspace, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            // Rendering a hundred thousand rows is not work to do on the frame
            // thread, and the write even less so.
            let written = cx
                .background_executor()
                .spawn(async move {
                    // The extension decides the format, not the row that started
                    // this: someone who typed `.json` over the suggested `.csv`
                    // asked for JSON. Which is why the notice says which one it
                    // wrote -- that rename is the one thing they could have got
                    // wrong, and the file name alone does not read it back.
                    let format = Format::for_path(&path);
                    let text = export::render(format, &result);
                    match std::fs::write(&path, text) {
                        Ok(()) => Ok((path, format)),
                        Err(error) => Err(format!("Could not write {}: {error}", path.display())),
                    }
                })
                .await;
            _ = workspace.update(cx, |workspace, cx| {
                // The panel is modeless, so the profile can have moved under
                // this task while it was open. `note` writes to whichever
                // profile is active now, which would announce an export in a
                // session that never ran the query behind it.
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                profile.session.notice = Some(match written {
                    Ok((path, format)) => format!(
                        "Exported {} {} as {} to {}.",
                        group_thousands(rows as u64),
                        if rows == 1 { "row" } else { "rows" },
                        format.extension().to_uppercase(),
                        path.display()
                    ),
                    Err(error) => error,
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Turn the grid's pending edits into SQL and put it where the user can read
    /// it: the query tab's buffer, or a relation tab's modal.
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
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        let pending = results.read(cx).delegate().pending_updates();
        let Some(batch) = update_batch(self.engine(), &pending) else {
            self.note(
                match pending.is_empty() {
                    true => "There are no edits to apply.".into(),
                    // Nothing partial runs: a batch missing one of its rows is
                    // not the change the user made.
                    false => "dbdelve cannot name an edited row by its primary key.".into(),
                },
                cx,
            );
            return;
        };
        // The gate every generated statement passes before anything executes
        // (`AGENTS.md` rule 2). Failing it means dbdelve wrote something outside
        // the shapes the gate names, which is a bug in dbdelve rather than a user
        // error.
        if !sql::is_generated_write(&batch) {
            self.note(
                "dbdelve refused to run a statement it wrote itself: it is not an UPDATE.".into(),
                cx,
            );
            return;
        }

        match tab {
            Tab::Query(_) => self.apply_in_buffer(batch, window, cx),
            Tab::Object(_) => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.apply_review = Some(ApplyReview {
                        tab,
                        title: "Apply edits",
                        sql: batch,
                    });
                    cx.notify();
                }
            }
        }
    }

    /// The query tab: the batch is appended to the user's buffer, runs from
    /// there, and the `SELECT` that produced the grid runs after it.
    ///
    /// The append is what keeps a failure readable. `execute_sql` clears the
    /// grid as it starts, so the pending edits are gone either way — but the
    /// statement that was attempted is still in the buffer.
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
        let Some(select) = tab.last_query.clone() else {
            self.note(
                "dbdelve does not know which statement produced these rows.".into(),
                cx,
            );
            return;
        };

        let text = editor.read(cx).value().to_string();
        let appended = appended_statement(&text, &batch);
        editor.update(cx, |editor, cx| editor.set_value(appended, window, cx));
        self.execute_and_then(
            batch,
            Tab::Query(id),
            Some(Refresh::Statement(select)),
            false,
            None,
            cx,
        );
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
        let Tab::Object(id) = tab else {
            return;
        };
        // Without the refresh the grid would show the UPDATE's empty result set
        // and the user would watch their table vanish.
        self.execute_and_then(sql, tab, Some(Refresh::Relation(id)), false, None, cx);
    }

    /// Put the batch away, leaving the edits pending: reading a statement and
    /// deciding not to run it is not the same as throwing the edits out, which
    /// is what Discard is for.
    pub(crate) fn close_apply_review(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let closed = profile.session.apply_review.take().is_some();
        if closed {
            cx.notify();
        }
        closed
    }

    /// Back to exactly the rows the server sent.
    pub(crate) fn discard_edits(
        &mut self,
        _: &DiscardEdits,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        results.update(cx, |table, cx| {
            table.delegate_mut().discard_pending();
            table.refresh(cx);
        });
        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = None;
        }
        cx.notify();
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

#[cfg(test)]
mod tests {
    use super::typed_page;

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

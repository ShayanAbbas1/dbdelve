//! Running statements, and the saved queries and buffers they come from.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::session::{PendingRun, Resume};

impl Workspace {
    /// Edits sitting in the visible grid, waiting to be written back. Read off
    /// the grid rather than held anywhere, so nothing can disagree with the
    /// cells about whether there is something to apply.
    pub(crate) fn has_pending_edits(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| results.read(cx).delegate().has_pending())
        })
    }

    /// Whether the cell the ring is on can be written at all. Read off the grid
    /// for the reason [`Workspace::has_pending_edits`] is: nothing may disagree
    /// with the cells about what is editable.
    pub(crate) fn has_editable_cell(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile.session.active_results().is_some_and(|results| {
                let grid = results.read(cx);
                grid.delegate()
                    .active()
                    .is_some_and(|(row, col)| grid.delegate().editable(row, col))
            })
        })
    }

    /// Whether the row the ring is on can be named by its primary key, which is
    /// the whole of what makes it deletable. Read off the grid for the reason
    /// [`Workspace::has_editable_cell`] is.
    pub(crate) fn has_nameable_row(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile.session.active_results().is_some_and(|results| {
                let grid = results.read(cx);
                grid.delegate()
                    .active()
                    .is_some_and(|(row, _)| grid.delegate().row_key(row).is_some())
            })
        })
    }

    /// Whether the surface in front has a result set to write out. Columns, not
    /// rows: a statement that matched nothing still has a shape, and a
    /// header-only CSV is a truthful answer to it.
    pub(crate) fn has_results(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| !results.read(cx).delegate().result().columns.is_empty())
        })
    }

    /// Ask the server to stop whatever the active profile is running.
    ///
    /// Nothing is marked cancelled here. The statement is still in flight until
    /// the driver returns, and what it returns — rows, or the server's own word
    /// for having been stopped — is what the surface shows, through the same
    /// completion every other run goes through.
    ///
    /// What the slot does record is that the request went out, which is true
    /// and is not a result. Without it the button stayed live and said
    /// "Cancel", so a click looked like it had done nothing and the next one
    /// sent the whole cancel again — on MySQL a fresh connection, auth and
    /// `KILL QUERY` per click.
    ///
    /// On the background executor because the Postgres path opens a socket and
    /// spins a current-thread tokio runtime inside `cancel_query` to do it.
    /// That is legal for exactly the reason connecting is (AGENTS.md, "Do not
    /// add tokio"): the runtime belongs to the blocking driver and lives and
    /// dies on the thread the driver is running on. Nothing tokio-shaped is
    /// handed to GPUI's executor, which is the thing that panics.
    pub(crate) fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.profile().and_then(Profile::connection) else {
            return;
        };
        if let Some(profile) = self.profile_mut() {
            let tab = profile.session.active;
            // ponytail: per-slot UI truth about a request having been sent, not
            // a claim that anything stopped. It bounds the repeat clicks to one
            // cancel per run; a cancel that the server ignores has no answer
            // here, and would need the driver to report one.
            if let Some((QueryState::Running { cancelling }, _)) = profile.session.slot(tab) {
                if *cancelling {
                    return;
                }
                *cancelling = true;
                cx.notify();
            }
        }
        let cancel_task = cx
            .background_executor()
            .spawn(async move { connection.cancel() });

        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    pub(crate) fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        // An object tab has no buffer of its own: running it again is a refresh
        // of the rows dbdelve fetched, which is the only thing there is to run.
        if let Tab::Object(id) = tab {
            self.refresh_relation(id, cx);
            return;
        }
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };

        let Some(sql) = self.sql_to_run(&editor, window, cx) else {
            if let Some(profile) = self.profile_mut()
                && let Some((state, _)) = profile.session.slot(tab)
            {
                *state = QueryState::Failed(DbError {
                    message: "There is no statement to run.".into(),
                    position: None,
                });
            }
            cx.notify();
            return;
        };

        self.execute_sql(sql, tab, cx);
    }

    /// Ask the server how it would run the statement the user is pointing at.
    ///
    /// The same statement `run_query` would run — the selection if there is one,
    /// otherwise the statement under the cursor — with the engine's `EXPLAIN`
    /// in front of it. The prefix goes onto a copy and never into the buffer:
    /// the buffer is the user's (hard rule 1), and a plan is a question about a
    /// statement rather than a change to one.
    pub(crate) fn explain_query(
        &mut self,
        action: &ExplainQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        // An object tab's rows come from SQL dbdelve wrote, and its surface has no
        // buffer to point at. Nobody has asked to explain a preview.
        let Tab::Query(_) = tab else {
            return;
        };
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };
        let engine = profile.config.engine();

        let failure = |workspace: &mut Self, message: &str, cx: &mut Context<Self>| {
            if let Some(profile) = workspace.profile_mut()
                && let Some((state, _)) = profile.session.slot(tab)
            {
                *state = QueryState::Failed(DbError {
                    message: message.into(),
                    position: None,
                });
            }
            cx.notify();
        };

        let Some(prefix) = engine.explain_prefix(action.mode) else {
            // Reachable only if a menu offers a mode the engine does not have,
            // which is what `explain_prefix` returning `None` is there to stop.
            failure(
                self,
                &format!(
                    "{} cannot {}.",
                    engine.label(),
                    action.mode.label().to_lowercase()
                ),
                cx,
            );
            return;
        };
        let Some(sql) = self.sql_to_run(&editor, window, cx) else {
            failure(self, "There is no statement to explain.", cx);
            return;
        };

        self.execute_and_then(
            format!("{prefix}{sql}"),
            tab,
            None,
            false,
            Some(action.mode),
            cx,
        );
    }

    /// Flip the query tab's results pane between its rows and its plan.
    pub(crate) fn show_plan(&mut self, showing: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Tab::Query(id) = profile.session.active else {
            return;
        };
        let Some(tab) = profile.session.query_tab_mut(id) else {
            return;
        };
        // Nothing to turn to. The toggle is not drawn in that case, so this is
        // the palette's row and a stale keystroke rather than a button.
        if showing && tab.plan.is_none() {
            return;
        }
        tab.showing_plan = showing;
        cx.notify();
    }

    pub(crate) fn persist_buffer(&self, cx: &App) -> Result<(), String> {
        match self.profile() {
            Some(profile) => {
                write_grids(profile, cx);
                write_buffer(profile, cx)
            }
            None => Ok(()),
        }
    }

    /// Every profile's buffer, for the one moment there is nowhere to report a
    /// failure to: the application is closing.
    pub(crate) fn persist_buffers(&self, cx: &App) {
        for profile in &self.profiles {
            write_grids(profile, cx);
            let _ = write_buffer(profile, cx);
        }
    }

    pub(crate) fn save_query(
        &mut self,
        _: &SaveQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A named query is written on every swap and on quit, so calling this
        // on one is a confirmation rather than a decision. Only a buffer with
        // nowhere to go has to ask for a name.
        if self.named() {
            match self.persist_buffer(cx) {
                Ok(()) => self.note("Saved query.".into(), cx),
                Err(message) => self.note(message, cx),
            }
            return;
        }
        self.ask_for_name(String::new(), window, cx);
    }

    /// Whether the visible buffer already has a name — which is what makes the
    /// difference between saving it and renaming it. A relation's tab never
    /// does: it holds SQL dbdelve wrote, not a file the user opened.
    pub(crate) fn named(&self) -> bool {
        self.profile().is_some_and(|profile| {
            matches!(profile.session.active, Tab::Query(_))
                && profile.session.open_query().is_some()
        })
    }

    pub(crate) fn rename_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .profile()
            .and_then(|profile| profile.session.open_query().map(str::to_string))
        else {
            return;
        };
        // Prefilled with its own name, unlike a save: the point of a rename is
        // to edit the name that is already there.
        self.ask_for_name(name, window, cx);
    }

    pub(crate) fn ask_for_name(
        &mut self,
        prefill: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.naming = true;
        profile.session.save_name_needs_focus = true;
        profile.session.notice = None;
        let save_name = profile.session.save_name.clone();
        save_name.update(cx, |input, cx| input.set_value(prefill, window, cx));
        cx.notify();
    }

    pub(crate) fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let name = profile
            .session
            .save_name
            .read(cx)
            .value()
            .trim()
            .to_string();
        if let Err(message) = store::validate_query_name(&name) {
            self.note(message, cx);
            return;
        }
        let id = profile.id.clone();
        let tab = profile.session.active;
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };
        // The name the buffer is leaving behind, if it had one. Present only
        // for a rename, since saving a query that already has a name never
        // asks.
        let previous = match tab {
            Tab::Query(id) => profile
                .session
                .query_tab(id)
                .and_then(|tab| tab.open_query.clone()),
            Tab::Object(_) => None,
        };
        if previous.as_deref() != Some(name.as_str())
            && profile.session.saved_queries.contains(&name)
        {
            self.note(format!("A query named {name} already exists."), cx);
            return;
        }

        let sql = editor.read(cx).value().to_string();
        if let Err(message) = store::write_query(&id, &name, &sql) {
            self.note(message, cx);
            return;
        }
        // Written first, then the old name dropped: a failed delete leaves two
        // copies, which is recoverable, and the other order loses the query.
        if let Some(previous) = previous.filter(|previous| previous != &name)
            && let Err(message) = store::delete_query(&id, &previous)
        {
            self.note(message, cx);
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.naming = false;
        }
        // Naming a relation's buffer is how it stops being a relation's buffer:
        // it leaves the object world entirely and becomes a saved query, which
        // is the only place a name means anything.
        match tab {
            Tab::Object(object) => {
                self.close_object(object, cx);
                self.open_saved_query(name.clone(), window, cx);
            }
            Tab::Query(id) => {
                if let Some(profile) = self.profile_mut() {
                    if let Some(tab) = profile.session.query_tab_mut(id) {
                        tab.open_query = Some(name.clone());
                    }
                    profile.session.editor_needs_focus = true;
                }
            }
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = Some(format!("Saved {name}."));
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// A new empty buffer, beside the ones already open.
    ///
    /// It used to clear the buffer in front, which is why a dirty scratch was
    /// persisted and then emptied: one editor meant a new query had nowhere to
    /// go but on top of the old one.
    pub(crate) fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let id = profile.session.next_query_id;
        profile.session.next_query_id += 1;
        profile.session.naming = false;
        profile.session.notice = None;
        let profile_id = profile.id.clone();

        let (tab, _) = QueryTab::restore(
            &profile_id,
            &store::StoredQueryTab {
                id,
                name: None,
                active: true,
            },
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        profile.session.queries.push(tab);
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), cx);
    }

    /// Close an unsaved buffer. Its scratch file goes with it: an unnamed
    /// buffer is its text, and closing one is discarding both.
    pub(crate) fn close_buffer(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let position = profile.session.queries.iter().position(|tab| tab.id == id);
        let Some(position) = position else {
            return;
        };
        profile.session.queries.remove(position);
        let profile_id = profile.id.clone();

        if profile.session.active == Tab::Query(id) {
            // The neighbour on the left, or the one that slid into this slot.
            let next = profile
                .session
                .queries
                .get(position.saturating_sub(1))
                .map(|tab| tab.id);
            if let Some(next) = next {
                profile.session.active = Tab::Query(next);
                profile.session.editor_needs_focus = true;
            }
        }

        if let Err(message) = store::delete_scratch(&profile_id, id) {
            self.note(message, cx);
        }
        // The tab is gone, so its snapshot has nothing left to come back to --
        // and the ids are reused, so a leftover file would open as another
        // buffer's rows.
        let _ = store::remove_grid(&profile_id, &store::query_grid_key(id));
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Bring a saved query up: its own tab if one is already open, a new one
    /// otherwise. It never lands on top of a buffer someone is writing in.
    pub(crate) fn open_saved_query(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .profile()
            .and_then(|profile| profile.session.tab_holding(&name))
        {
            self.activate_tab(Tab::Query(id), cx);
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        // Read before the tab is built, so a query whose file has gone since
        // the strip was drawn says so instead of opening an empty buffer.
        match store::read_query(&profile_id, &name) {
            Ok(Some(_)) => {}
            Ok(None) => {
                profile.session.saved_queries = store::saved_queries(&profile_id);
                profile.session.notice = Some(format!("{name} no longer exists."));
                cx.notify();
                return;
            }
            Err(message) => {
                profile.session.notice = Some(message);
                cx.notify();
                return;
            }
        }

        let id = profile.session.next_query_id;
        profile.session.next_query_id += 1;
        profile.session.notice = None;

        let (tab, notice) = QueryTab::restore(
            &profile_id,
            &store::StoredQueryTab {
                id,
                name: Some(name),
                active: true,
            },
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.queries.push(tab);
        profile.session.notice = notice;
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), cx);
    }

    /// A statement out of the history, back in the buffer.
    ///
    /// Appended rather than swapped in, for the reason `apply_in_buffer`
    /// appends: recalling a statement is not a reason to take away what is
    /// already written, and the statement that runs is the statement on screen.
    /// The cursor lands on it, because that is what `cmd+enter` reads to decide
    /// what to send.
    pub(crate) fn recall_statement(
        &mut self,
        sql: String,
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
        let text = editor.read(cx).value().to_string();
        let appended = appended_statement(&text, &sql);
        let line = appended.lines().count().saturating_sub(sql.lines().count()) as u32;
        editor.update(cx, |editor, cx| {
            editor.set_value(appended, window, cx);
            editor.set_cursor_position(Position::new(line, 0), window, cx);
        });
        self.activate_tab(Tab::Query(id), cx);
    }

    /// The first unsaved buffer, or a new one when every open tab has a name.
    ///
    /// It used to swap the scratch file into the single editor, which is what
    /// made "New Query" a place rather than a tab.
    pub(crate) fn open_scratch_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.profile().and_then(|profile| {
            profile
                .session
                .queries
                .iter()
                .find(|tab| tab.open_query.is_none())
                .map(|tab| tab.id)
        });
        match unsaved {
            Some(id) => self.activate_tab(Tab::Query(id), cx),
            None => self.new_query(&NewQuery, window, cx),
        }
    }

    /// The chip's own delete: the first click arms it and the second one means
    /// it. Quieter than the dialog `cmd+w` raises, because the trash icon is
    /// already an unambiguous ask and the tab it belongs to is right there.
    pub(crate) fn arm_delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.pending_delete.as_deref() != Some(&name) {
            profile.session.pending_delete = Some(name);
            cx.notify();
            return;
        }
        self.delete_saved_query(name, cx);
    }

    pub(crate) fn delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let id = profile.id.clone();
        // Read before the delete, because afterwards nothing on the session
        // still points at the file and only this says which tab did.
        let was_open = profile.session.tab_holding(&name);
        if let Err(message) = store::delete_query(&id, &name) {
            self.note(message, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.pending_delete = None;
            profile.session.pending_close = None;
            profile.session.notice = Some(format!("Deleted {name}."));

            // The tab goes with the file. Its text was the query, and the query
            // is what was deleted -- keeping it in an untitled buffer would
            // leave `cmd+w` looking like it had done nothing.
            if let Some(open) = was_open {
                if profile.session.queries.len() > 1 {
                    profile.session.queries.retain(|tab| tab.id != open);
                    // With the tab, as in `close_buffer`: a snapshot with no
                    // tab left to come back to is rows the next buffer to be
                    // handed this id would show as its own.
                    let _ = store::remove_grid(&id, &store::query_grid_key(open));
                    if profile.session.active == Tab::Query(open)
                        && let Some(next) = profile.session.queries.first().map(|tab| tab.id)
                    {
                        profile.session.active = Tab::Query(next);
                        profile.session.editor_needs_focus = true;
                    }
                } else if let Some(tab) = profile.session.query_tab_mut(open) {
                    // The only buffer. A profile always has somewhere to write,
                    // so it is unnamed from here rather than closed, and the
                    // strip keeps a place to type in.
                    tab.open_query = None;
                }
            }
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Run `sql` against the active profile.
    ///
    /// There is deliberately no "not connected" branch: SQL is only reachable
    /// through a profile's own editor or explorer, so the absence of one is not
    /// a state the user can be shown an error about.
    pub(crate) fn execute_sql(&mut self, sql: String, tab: Tab, cx: &mut Context<Self>) {
        self.execute_and_then(sql, tab, None, false, None, cx);
    }

    /// Runs a statement, if the connection's mode allows it.
    ///
    /// The check lives here rather than in each caller because every path that
    /// runs SQL routes through this one -- `connection.query` has exactly one
    /// call site in the app, inside `execute_unchecked`. A stopped statement is
    /// held on `pending_run` rather than run: nothing here sets
    /// `QueryState::Running` or appends to history, because a statement that
    /// did not run is not history and must not leave a spinner behind.
    pub(crate) fn execute_and_then(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        explain: Option<ExplainMode>,
        cx: &mut Context<Self>,
    ) {
        let verdict = sql::classify(self.engine(), &sql);
        let stopped = self
            .profile()
            .and_then(|profile| sql::gate(&verdict, profile.mode, &profile.confirmed));

        if stopped.is_some() {
            if let Some(profile) = self.profile_mut() {
                profile.session.pending_run = Some(PendingRun {
                    resume: Some(Resume {
                        sql,
                        tab,
                        refresh,
                        keep_rows,
                        explain,
                    }),
                    verdict,
                    dont_ask: false,
                });
            }
            cx.notify();
            return;
        }

        self.execute_unchecked(sql, tab, refresh, keep_rows, explain, cx);
    }

    /// Runs a statement without consulting the connection's mode. Only two
    /// callers: `execute_and_then`, once the mode has allowed it, and the
    /// prompt's own Run.
    ///
    /// `keep_rows` leaves whatever the grid is showing in place until the new
    /// result lands, for the refresh of a tab whose rows came off disk. Every
    /// other run clears them first, because rows from the previous statement
    /// sitting under the one now running cannot be told from fresh ones.
    ///
    /// `explain` says this submission is an `EXPLAIN`, and diverts its result
    /// away from the grid and into the tab's plan. It routes through here rather
    /// than down a path of its own because everything around the result — the
    /// single-flight guard, the generation check that drops a stale run, the
    /// cancel handle, the connection — is the same for a plan as for rows, and a
    /// second copy of it is a second place for those to go wrong.
    ///
    /// Chained inside the completion rather than called after it: `execute_sql`
    /// refuses to start while a query is running, so a second call made here
    /// would be dropped on the floor. Nothing follows a failure — the error is
    /// what there is to see, and a refresh would replace it with rows.
    pub(crate) fn execute_unchecked(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        explain: Option<ExplainMode>,
        cx: &mut Context<Self>,
    ) {
        // Read before the task, which outlives the borrow of `self`.
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let connection = profile.connection();
        let id = profile.id.clone();
        let generation = profile.generation;
        let Some((state, results)) = profile.session.slot(tab) else {
            return;
        };
        // Guarded here rather than in each caller: every path that runs SQL
        // routes through this one, and a caller that forgets would let two
        // results race into the grid with the older one landing last.
        if matches!(state, QueryState::Running { .. }) {
            return;
        }

        let Some(connection) = connection else {
            *state = QueryState::Failed(DbError {
                message: "The connection is not open.".into(),
                position: None,
            });
            cx.notify();
            return;
        };
        *state = QueryState::Running { cancelling: false };
        // Whatever plan is on screen describes the last statement, not this one.
        // Turning the pane back to the rows is what puts the spinner and Cancel
        // in front of a run that is in flight -- and what stops a plain Run from
        // landing rows behind a plan the user is still looking at. An `EXPLAIN`
        // turns it back when its own answer arrives.
        if let Tab::Query(query) = tab
            && let Some(tab) = profile.session.query_tab_mut(query)
        {
            tab.showing_plan = false;
        }

        // Rows from the previous statement must not sit under the one now on
        // screen -- a reader cannot tell stale rows from fresh ones. An
        // `EXPLAIN` never reaches the grid at all, so the rows already there are
        // not the previous statement's: they are still this tab's own result,
        // and are what the user flips back to.
        if !keep_rows && explain.is_none() {
            results.update(cx, |table, cx| {
                *table.delegate_mut() = ResultGrid::empty();
                // The inspector reads whatever row is selected, and a row index
                // means nothing once the rows behind it are gone.
                table.clear_selection(cx);
                table.refresh(cx);
            });
        }
        cx.notify();

        // Read from the statement that is about to run, so the headers say what
        // the rows on screen are actually ordered by rather than what dbdelve
        // last intended to ask for.
        let keys = sql::order_by(&sql);
        let sortable = keys.is_some();
        let keys = keys.unwrap_or_default();
        // Kept only where it is read back: the query tab's grid has to be able
        // to say which statement produced it. An `EXPLAIN` produces no rows to
        // describe and belongs in nobody's history -- it is dbdelve's prefix over
        // the user's statement, and the statement itself is already there.
        let statement = (matches!(tab, Tab::Query(_)) && explain.is_none()).then(|| sql.clone());
        // What the plan pane says it is a plan of: the user's statement, without
        // the prefix dbdelve put in front of it.
        let explained = explain.map(|mode| {
            let prefix = engine.explain_prefix(mode).unwrap_or_default();
            sql.strip_prefix(prefix).unwrap_or(&sql).to_string()
        });
        // Recorded on the way out rather than on the way back: the history is
        // what the user ran, and a statement that failed is exactly the one
        // worth getting back. Only the buffer's — a relation's preview is SQL
        // dbdelve wrote, and nobody asked to keep it.
        if let Some(statement) = &statement
            && let Some(profile) = self.profile_mut()
        {
            // A line that could not be written is not worth a notice on every
            // run: the statement is still in the buffer, so nothing is lost.
            let _ = store::append_history(&profile.id, statement);
            remember_statement(&mut profile.session.history, statement);
        }
        let query_task = cx
            .background_executor()
            .spawn(async move { connection.query(&sql) });

        cx.spawn(async move |workspace, cx| {
            let result = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    // The plan is carried out of this block rather than stored
                    // inside it: `slot` holds the session borrowed, and the tab
                    // it belongs on has to be reached through the same session.
                    let (succeeded, produced_grid, plan) = {
                        let Some(profile) = workspace.issued_to(&id, generation) else {
                            workspace.drop_stale_run(&id, tab, cx);
                            return;
                        };
                        // Read before `slot`, which borrows the session and not
                        // this field -- but the mode a result lands under has to
                        // be the mode at landing time, not a stale default.
                        let mode = profile.mode;
                        let Some((state, results)) = profile.session.slot(tab) else {
                            return;
                        };

                        match result {
                            // An `EXPLAIN` describes a statement rather than
                            // returning its rows, so nothing here reaches the
                            // grid: it keeps whatever the last real run put in
                            // it, which is what the Data tab flips back to.
                            Ok(result) if explain.is_some() => {
                                let mode = explain.unwrap_or_default();
                                *state = QueryState::Explained {
                                    elapsed: result.elapsed,
                                    mode,
                                };
                                let columns: Vec<String> = result
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect();
                                let plan = explain::parse(&columns, &result.rows);
                                (true, false, Some(plan))
                            }
                            Ok(mut result) => {
                                // An `INSERT … RETURNING` grid traces to its
                                // table like any select, but applying an edit
                                // re-runs the statement behind the grid to
                                // reload it -- which would repeat the write.
                                if statement
                                    .as_deref()
                                    .is_some_and(|statement| !sql::rerunnable(engine, statement))
                                {
                                    result.edit = None;
                                }
                                *state = QueryState::Complete {
                                    rows: result.rows.len(),
                                    bytes: result.bytes,
                                    elapsed: result.elapsed,
                                    rows_affected: result.rows_affected,
                                };
                                let produced_grid = !result.columns.is_empty();
                                results.update(cx, |table, cx| {
                                    let sort = sort_columns(engine, &keys, &result.columns);
                                    *table.delegate_mut() =
                                        ResultGrid::new(result, mode).with_sort(sort, sortable);
                                    table.refresh(cx);
                                });
                                (true, produced_grid, None)
                            }
                            Err(error) => {
                                *state = QueryState::Failed(error);
                                (false, false, None)
                            }
                        }
                    };

                    if succeeded && let Some(profile) = workspace.issued_to(&id, generation) {
                        // A statement that returned no columns produced no grid,
                        // so it is not the statement to go back to — which is
                        // what keeps an applied UPDATE from becoming the query
                        // an apply re-runs.
                        if produced_grid
                            && let Some(statement) = statement
                            && let Tab::Query(query) = tab
                            && let Some(tab) = profile.session.query_tab_mut(query)
                        {
                            tab.last_query = Some(statement);
                        }
                        // Shown as soon as it lands: asking for a plan is asking
                        // to read one, so the pane turns to it rather than
                        // leaving the answer behind a tab the user has to find.
                        if let Some(plan) = plan
                            && let Some(mode) = explain
                            && let Tab::Query(query) = tab
                            && let Some(tab) = profile.session.query_tab_mut(query)
                        {
                            tab.plan = Some(Explained {
                                plan,
                                mode,
                                sql: explained.unwrap_or_default(),
                            });
                            tab.showing_plan = true;
                        }
                        // Nothing left to read once the batch it was showing has
                        // run.
                        if refresh.is_some() {
                            profile.session.apply_review = None;
                        }
                    }
                    // The other side of the pair in `load_structure`: a run
                    // replaces the whole delegate, so a fresh grid has to be
                    // marked again from the structure the tab already holds.
                    if succeeded && let Tab::Object(object) = tab {
                        workspace.mark_columns(object, cx);
                    }
                    cx.notify();

                    if succeeded && let Some(refresh) = refresh {
                        match refresh {
                            Refresh::Statement(sql) => workspace.execute_sql(sql, tab, cx),
                            Refresh::Relation(id) => workspace.refresh_relation(id, cx),
                        }
                    }
                })
                .ok();
        })
        .detach();
    }

    /// A result dropped because its generation was retired -- a reconnect or a
    /// profile switch bumped it while the statement was in flight.
    ///
    /// The tab it was for is still `Running`, showing a spinner over rows
    /// nothing is going to replace, and `load_relation` will not re-query a tab
    /// that is not `Idle` or `Failed`. `Idle` rather than `Failed`, because
    /// nothing failed: the run was abandoned, and `Idle` is what makes the next
    /// visit to the tab run it again.
    pub(crate) fn drop_stale_run(&mut self, id: &str, tab: Tab, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let Some((state, _)) = profile.session.slot(tab) else {
            return;
        };
        if matches!(state, QueryState::Running { .. }) {
            *state = QueryState::Idle;
            cx.notify();
        }
    }

    /// Rewrite the buffer as formatted SQL. Invoked by hand only -- running,
    /// saving and leaving the buffer all leave what was typed alone.
    pub(crate) fn format_query(
        &mut self,
        _: &FormatQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let editor = tab.editor.clone();
        let (text, cursor) = {
            let editor = editor.read(cx);
            (editor.value().to_string(), editor.cursor())
        };
        if text.trim().is_empty() {
            return;
        }

        // Said out loud rather than left as a no-op: a Format that appears to
        // do nothing reads as a broken Format, not as a deliberate refusal.
        let Some(formatted) = crate::sql::format(&text) else {
            self.note(
                "Not formatting: a dollar-quoted body would be rewritten.".into(),
                cx,
            );
            return;
        };
        if formatted == text {
            return;
        }

        // ponytail: the cursor lands at the top of the statement it was in,
        // not on the token it was on -- a reflow moves every offset, and the
        // statement is the unit the user was working in. Map the token too if
        // the jump ever reads as losing your place.
        let was_in = Buffer::parse(&text)
            .statement_at(cursor)
            .and_then(|range| {
                statement_starts(&text)
                    .iter()
                    .position(|start| *start == range.start)
            })
            .and_then(|index| statement_starts(&formatted).get(index).copied());
        let position = was_in.map_or(Position::new(0, 0), |offset| {
            position_at(&formatted, offset)
        });

        editor.update(cx, |editor, cx| {
            editor.set_value(formatted, window, cx);
            editor.set_cursor_position(position, window, cx);
        });
    }

    pub(crate) fn sql_to_run(
        &self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let selection = editor.update(cx, |editor, cx| {
            let selection = editor.selected_text_range(false, window, cx)?;
            if selection.range.is_empty() {
                return None;
            }

            let mut adjusted_range = None;
            editor.text_for_range(selection.range, &mut adjusted_range, window, cx)
        });

        if selection.is_some() {
            return selection;
        }

        let editor = editor.read(cx);
        let sql = editor.value();
        let range = Buffer::parse(&sql).statement_at(editor.cursor())?;
        Some(sql[range].to_string())
    }
}

/// Where each statement begins, in source order. `Buffer` keeps its ranges to
/// itself outside its own tests, so `statement_at` is the only way in.
///
/// ponytail: one probe per byte, which is nothing next to the parse that
/// precedes it. Ask `Buffer` for the ranges directly if a buffer ever gets big
/// enough to feel it.
fn statement_starts(sql: &str) -> Vec<usize> {
    let buffer = Buffer::parse(sql);
    let mut starts: Vec<usize> = Vec::new();
    for range in (0..=sql.len()).filter_map(|offset| buffer.statement_at(offset)) {
        if starts.last() != Some(&range.start) {
            starts.push(range.start);
        }
    }
    starts
}

fn position_at(text: &str, offset: usize) -> Position {
    let before = &text[..offset];
    Position::new(
        before.matches('\n').count() as u32,
        before
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count() as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_starts_finds_every_statement_in_order() {
        let sql = "select 1;\n\n-- a comment\nselect 2;\nselect 3";
        let starts = statement_starts(sql);
        assert_eq!(starts.len(), 3);
        assert!(starts.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(&sql[starts[1]..starts[1] + 8], "select 2");
    }

    #[test]
    fn a_position_lands_on_the_line_the_offset_is_on() {
        let sql = "select 1;\n  select 2;";
        let offset = statement_starts(sql)[1];
        assert_eq!(position_at(sql, offset), Position::new(1, 2));
    }
}

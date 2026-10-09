//! Running statements, and the saved queries and buffers they come from.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use std::ops::Range;

use gpui::Pixels;
use gpui_component::table::TableState;

use crate::db::{Feed, Fetch, QueryResult, Stopped};

use super::*;
use crate::session::{Finished, Lane, PendingRun, Queue, Resume, Step, TabKey, next_step};

impl Workspace {
    /// Edits sitting in the visible grid, waiting to be written back. Read off
    /// the grid rather than held anywhere, so nothing can disagree with the
    /// cells about whether there is something to apply.
    pub(crate) fn has_pending_edits(&self, cx: &App) -> bool {
        self.pending_edit_count(cx) > 0
    }

    pub(crate) fn pending_edit_count(&self, cx: &App) -> usize {
        self.profile()
            .and_then(|profile| profile.session.active_results())
            .map_or(0, |results| results.read(cx).delegate().pending_count())
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
    pub(crate) fn has_active_cell(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| results.read(cx).delegate().active().is_some())
        })
    }

    pub(crate) fn has_results(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| !results.read(cx).delegate().result().columns.is_empty())
        })
    }

    /// Ask the server to stop what the active tab is running.
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
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let tab = profile.session.active;
        // A relation tab whose rows are not loading may be counting them, and
        // that is the run in front of the user.
        if let Tab::Object(id) = tab
            && !matches!(
                profile.session.slot(tab),
                Some((QueryState::Running { .. }, _))
            )
        {
            self.cancel_count(id, cx);
            return;
        }
        let now = std::time::Instant::now();
        // A queue's statement running at once is stopped on its own when it is
        // the one on screen, and the rest carry on. Read off its own slot,
        // which is what its chip and the status bar show.
        let cancels = if let Tab::Query(id) = tab
            && let Some(queue) = profile
                .session
                .query_tab_mut(id)
                .and_then(|query| query.queue.as_mut())
            && let Some(Finished {
                state:
                    QueryState::Running {
                        cancelling, cancel, ..
                    },
                ..
            }) = queue.done.get_mut(queue.showing)
        {
            if cancelling.is_some() {
                return;
            }
            *cancelling = Some(now);
            vec![cancel.clone()]
        } else {
            // ponytail: per-slot UI truth about a request having been sent,
            // not a claim that anything stopped. It bounds the repeat clicks
            // to one cancel per run; a cancel that the server ignores has no
            // answer here, and would need the driver to report one.
            let Some((
                QueryState::Running {
                    cancelling, cancel, ..
                },
                _,
            )) = profile.session.slot(tab)
            else {
                return;
            };
            if cancelling.is_some() {
                return;
            }
            *cancelling = Some(now);
            let mut cancels = vec![cancel.clone()];
            // With a finished result in front, Cancel means the whole run, so
            // a queue's statements still running at once are stopped too,
            // each by its own token, and each slot marked, since that is what
            // decides whether it keeps the rows it had.
            if let Tab::Query(id) = tab
                && let Some(queue) = profile
                    .session
                    .query_tab_mut(id)
                    .and_then(|query| query.queue.as_mut())
            {
                for finished in &mut queue.done {
                    if let QueryState::Running {
                        cancelling: cancelling @ None,
                        cancel,
                        ..
                    } = &mut finished.state
                    {
                        *cancelling = Some(now);
                        cancels.push(cancel.clone());
                    }
                }
            }
            // An explicit stop ends the queue the statement was part of:
            // nothing behind it is sent, and no decision is raised -- this was
            // the decision. The cancelled statement itself still lands, as the
            // queue's last result, with whatever rows it kept or the error it
            // ended in.
            self.drop_rest_of_queue(tab);
            cancels
        };
        cx.notify();
        let cancel_task = cx.background_executor().spawn(on_own_thread(move || {
            cancels
                .iter()
                .map(|cancel| connection.cancel(cancel))
                .fold(Ok(()), Result::and)
        }));

        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    pub(crate) fn set_query_limit(
        &mut self,
        action: &SetQueryLimit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.profile_mut().and_then(|profile| {
            let Tab::Query(id) = profile.session.active else {
                return None;
            };
            profile.session.query_tab_mut(id)
        }) else {
            return;
        };
        tab.row_limit = action.rows;
        cx.notify();
    }

    pub(crate) fn set_default_row_limit_action(
        &mut self,
        action: &SetDefaultRowLimit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_default_row_limit(action.rows, cx);
    }

    pub(crate) fn custom_row_limit(
        &mut self,
        action: &CustomRowLimit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = action.target;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Rows"));
        cx.subscribe_in(
            &input,
            window,
            move |workspace, input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let typed = input.read(cx).value().replace([',', '_', ' '], "");
                    match typed.parse::<usize>().map_err(|error| *error.kind()) {
                        Ok(rows) if (1..=explorer::MAX_ROW_LIMIT).contains(&rows) => {
                            // The field goes while it holds focus, and focus
                            // left with nothing holding it takes every
                            // keybinding with it.
                            workspace.custom_limit = None;
                            workspace.refocus_front();
                            workspace.apply_row_limit(target, rows, window, cx);
                        }
                        Ok(1..) | Err(std::num::IntErrorKind::PosOverflow) => workspace.note(
                            format!(
                                "A row limit is at most {}.",
                                ui::group_thousands(explorer::MAX_ROW_LIMIT as u64)
                            ),
                            cx,
                        ),
                        _ => workspace.note("A row limit is a whole number above zero.".into(), cx),
                    }
                }
                InputEvent::Blur => {
                    workspace.custom_limit = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        self.custom_limit = Some((target, input));
        self.custom_limit_needs_focus = true;
        cx.notify();
    }

    fn apply_row_limit(
        &mut self,
        target: LimitTarget,
        rows: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            LimitTarget::Query(id) => {
                if let Some(tab) = self
                    .profile_mut()
                    .and_then(|profile| profile.session.query_tab_mut(id))
                {
                    tab.row_limit = Some(rows);
                }
                cx.notify();
            }
            LimitTarget::Relation(_) => {
                self.set_row_limit(&SetRowLimit { rows: Some(rows) }, window, cx)
            }
            LimitTarget::Default => self.set_default_row_limit(Some(rows), cx),
        }
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
        // Before anything on the tab is touched: a new queue in place of one
        // with a statement still out would take that statement's result as
        // one of its own, or leave a queue run at once nowhere to land.
        if profile.session.running(tab) {
            return;
        }
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };

        // A selection holding more than one statement runs each of them in
        // turn, keeping every result. Anything else -- one selected statement,
        // or the one under the cursor -- is sent the way it always was, which
        // is verbatim, so a selection's comments and spacing still reach the
        // server untouched.
        let (text, selection) = {
            let editor = editor.read(cx);
            (editor.value().to_string(), editor.selected_range())
        };
        if !selection.is_empty() {
            let engine = self.engine();
            let statements = sql::queued_statements(engine, &text, Some(selection.clone()));
            // SQL Server's unit is the batch, so `SELECT 1; SELECT 2` is one
            // submission whose sets arrive in turn. Plain reads go out a
            // statement apiece at once instead, as they do on every other
            // engine; any batch a split would change stays whole.
            let reads = sql::split_reads(engine, &text, &statements, selection.clone());
            let reads_at_once = reads.len() > 1
                && self.profile().is_some_and(|profile| {
                    profile.in_turn().is_none() && {
                        let texts: Vec<&str> =
                            reads.iter().map(|range| &text[range.clone()]).collect();
                        session::runs_at_once(engine, profile.mode, &texts)
                    }
                });
            if reads_at_once {
                if let Err(message) = sql::batch_counts(engine, &text[selection]) {
                    self.refuse_run(tab, message, cx);
                    return;
                }
                self.run_at_once(tab, text, reads, window, cx);
                return;
            }
            if statements.len() > 1 {
                // A `GO 5` repeats its batch, and running it once is not what
                // the selection says. Refused by name here as it is for a
                // single batch, rather than quietly dropped.
                if let Err(message) = sql::batch_counts(engine, &text[selection]) {
                    self.refuse_run(tab, message, cx);
                    return;
                }
                self.run_statements(tab, text, statements, window, cx);
                return;
            }
        }

        let (start, sql) = match self.sql_to_run(&editor, cx) {
            Some(Ok(statement)) => statement,
            refused => {
                let message = refused
                    .and_then(Result::err)
                    .unwrap_or_else(|| "There is no statement to run.".into());
                self.refuse_run(tab, message, cx);
                return;
            }
        };

        // One statement is its own result, so it replaces whatever queue the
        // tab was showing rather than appending to it. On SQL Server the
        // submission is a whole batch, which can answer with a set per
        // statement in it, so it gets a queue carrying nothing but the empty
        // grids those sets will need -- `advance_queue` drops it again when
        // only one set arrives.
        if let Tab::Query(id) = tab {
            let spare: Vec<_> = (1..sql::expected_sets(self.engine(), &sql))
                .map(|_| crate::result_grid::new_grid(window, cx))
                .collect();
            if let Some(query) = self
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(id))
            {
                query.queue = (!spare.is_empty()).then(|| Queue {
                    sql: text.clone(),
                    remaining: Vec::new(),
                    done: Vec::new(),
                    showing: 0,
                    awaiting: true,
                    spare,
                });
                // With the old queue goes the claim on the snapshots it was
                // restored from, or the prune would keep files this tab will
                // never show again and the next relaunch would raise a strip
                // over one result.
                query.queued_results = 0;
            }
        }
        self.sent_from(tab, start, &sql);
        self.execute_sql(sql, tab, cx);
    }

    /// Say why nothing ran, where the result would have gone. A refusal is the
    /// tab's answer to the run, not a notice beside it -- unless a run is still
    /// out there, which owns the slot and the token Cancel reaches it by.
    fn refuse_run(&mut self, tab: Tab, message: String, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            if profile.session.running(tab) {
                profile.session.notice = Some(message);
            } else if let Some((state, _)) = profile.session.slot(tab) {
                *state = QueryState::Failed(DbError {
                    message,
                    position: None,
                });
            }
        }
        cx.notify();
    }

    /// Run each of a selection's statements in turn, keeping every result.
    ///
    /// Each goes out through `execute_sql` like any other, so classify and the
    /// mode gate answer for each of them separately. A statement the gate stops
    /// parks a `PendingRun` and the rest wait on the tab until it is answered.
    ///
    /// Called only with more than one statement: one is an ordinary run, and
    /// takes the verbatim path in `run_query` instead.
    fn run_statements(
        &mut self,
        tab: Tab,
        text: String,
        statements: Vec<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let engine = profile.config.engine();
        let texts: Vec<&str> = statements
            .iter()
            .map(|range| &text[range.clone()])
            .collect();
        if session::runs_at_once(engine, profile.mode, &texts) {
            match profile.in_turn() {
                Some(why) => profile.session.notice = Some(why.into()),
                None => {
                    self.run_at_once(tab, text, statements, window, cx);
                    return;
                }
            }
        }
        let Some((first, rest)) = statements.split_first() else {
            return;
        };
        // One grid per result the run can land, less the first, which the tab's
        // own slot holds. That is one per statement on four engines; on SQL
        // Server a batch can answer with a set per statement inside it.
        let spares = statements
            .iter()
            .map(|range| sql::expected_sets(engine, &text[range.clone()]))
            .sum::<usize>()
            .saturating_sub(1);
        let queue = Queue {
            sql: text.clone(),
            remaining: rest.to_vec(),
            done: Vec::new(),
            showing: 0,
            awaiting: true,
            spare: (0..spares)
                .map(|_| crate::result_grid::new_grid(window, cx))
                .collect(),
        };
        let sql = text[first.clone()].to_string();
        if let Some(query) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        {
            query.queue = Some(queue);
        }
        self.sent_from(tab, first.start, &sql);
        self.execute_sql(sql, tab, cx);
    }

    /// Run a selection's statements all at once, each on a connection of its
    /// own, keeping every result in its own place in the strip whatever order
    /// they finish in. Only for a queue `session::runs_at_once` admits: plain
    /// reads, with nothing to keep in order and nothing for a failure to
    /// protect, so one that fails says so on its chip and the rest carry on.
    ///
    /// The tab's own slot holds the run while it is out, under a token that
    /// names it (each statement is stopped by one of its own), so nothing else
    /// is run on the tab meanwhile. It is given the last statement's result
    /// once the last has landed, as a queue run in turn leaves it.
    fn run_at_once(
        &mut self,
        tab: Tab,
        text: String,
        statements: Vec<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Tab::Query(id) = tab else {
            return;
        };
        let engine = self.engine();
        let spares = statements
            .iter()
            .map(|range| sql::expected_sets(engine, &text[range.clone()]) - 1)
            .sum::<usize>();
        let done = statements
            .iter()
            .map(|range| Finished {
                sql: text[range.clone()].to_string(),
                start: range.start,
                state: QueryState::Idle,
                grid: crate::result_grid::new_grid(window, cx),
            })
            .collect();
        let spare = (0..spares)
            .map(|_| crate::result_grid::new_grid(window, cx))
            .collect();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let issued = (profile.id.clone(), profile.generation);
        let Some(query) = profile.session.query_tab_mut(id) else {
            return;
        };
        query.queue = Some(Queue {
            sql: text,
            remaining: Vec::new(),
            done,
            showing: 0,
            awaiting: false,
            spare,
        });
        query.query = QueryState::Running {
            started: std::time::Instant::now(),
            cancelling: None,
            cancel: CancelToken::alongside(),
        };
        for _ in 0..session::STATEMENTS_AT_ONCE {
            self.start_lane(tab, &issued.0, issued.1, cx);
        }
        cx.notify();
    }

    /// Send the first of a queue's statements still waiting to run at once
    /// with the others, on the profile the run was issued to.
    fn start_lane(&mut self, tab: Tab, profile: &str, generation: u64, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some((lane, sql)) = self.issued_to(profile, generation).and_then(|issued| {
            let query = issued.session.query_tab(id)?;
            let QueryState::Running { cancel, .. } = &query.query else {
                return None;
            };
            let waiting = query
                .queue
                .as_ref()?
                .done
                .iter()
                .find(|finished| matches!(finished.state, QueryState::Idle))?;
            let lane = Lane {
                profile: profile.to_string(),
                generation,
                run: cancel.clone(),
                start: waiting.start,
            };
            Some((lane, waiting.sql.clone()))
        }) else {
            return;
        };
        self.execute_unchecked(sql, tab, None, false, None, Some(lane), cx);
    }

    /// A statement run at once with others has landed in its own entry: put
    /// any further sets of its batch beside it, send the next waiting
    /// statement, and once none is out, hand the tab back.
    fn land_lane(&mut self, tab: Tab, lane: Lane, rest: Vec<QueryResult>, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let (mode, client_keys) = Self::landing_view(
            self.issued_to(&lane.profile, lane.generation)
                .map(|profile| &*profile),
            tab,
        );
        let (extra, dropped, out) = {
            let Some(queue) = self
                .issued_to(&lane.profile, lane.generation)
                .and_then(|profile| profile.session.query_tab_mut(id))
                .and_then(|query| query.queue.as_mut())
            else {
                return;
            };
            let Some(at) = queue.lane(lane.start) else {
                return;
            };
            let (extra, dropped) = queue.land_sets(at + 1, lane.start, rest);
            let out = queue.done.iter().any(|finished| {
                matches!(
                    finished.state,
                    QueryState::Running { .. } | QueryState::Idle
                )
            });
            (extra, dropped, out)
        };
        self.show_sets(extra, dropped, mode, client_keys.as_deref(), cx);
        if out {
            self.start_lane(tab, &lane.profile, lane.generation, cx);
            return;
        }

        let Some(query) = self
            .issued_to(&lane.profile, lane.generation)
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        let Some(queue) = &mut query.queue else {
            return;
        };
        let last = queue.done.iter().map(|finished| finished.start).max();
        if let Some(last) = queue
            .done
            .iter()
            .find(|finished| Some(finished.start) == last)
        {
            query.query = last.state.clone();
            query.results = last.grid.clone();
            query.ran_from = Some((last.start, last.sql.clone()));
            if matches!(last.state, QueryState::Complete { .. })
                && !last.grid.read(cx).delegate().result().columns.is_empty()
            {
                query.last_query = Some(last.sql.clone());
            }
        } else {
            query.query = QueryState::Idle;
        }
        // Cancelled before the rest went out, one result is no queue at all.
        if queue.done.len() < 2 {
            query.queue = None;
        }
        cx.notify();
    }

    /// Land the statement that just finished in the queue's history, and send
    /// the next one — or park the decision when it failed with more to run.
    ///
    /// `rest` is the submission's result sets after the first, which only a
    /// SQL Server batch ever has. They land as results of their own, so the
    /// strip reads one chip per result set however many submissions produced
    /// them.
    fn advance_queue(&mut self, tab: Tab, rest: Vec<db::QueryResult>, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let (mode, client_keys) = Self::landing_view(self.profile(), tab);
        let (step, extra, dropped) = {
            let Some(profile) = self.profile_mut() else {
                return;
            };
            let Some(query) = profile.session.query_tab_mut(id) else {
                return;
            };
            if query.queue.is_none() {
                return;
            }
            let failed = matches!(query.query, QueryState::Failed(_));
            let state = query.query.clone();
            let grid = query.results.clone();
            // Always set for a statement sent from the buffer, which every
            // statement of a queue is.
            let (start, sent) = query.ran_from.clone().unwrap_or_default();
            let Some(queue) = &mut query.queue else {
                return;
            };
            // Only a statement the queue itself sent is one of its results.
            if !std::mem::take(&mut queue.awaiting) {
                return;
            }
            // Only follow the result that just landed if the statement in
            // flight was what the user was watching. Someone reading an
            // earlier result stays on it.
            let following = queue.showing >= queue.done.len();
            queue.done.push(Finished {
                sql: sent,
                start,
                state,
                grid,
            });

            let (extra, dropped) = queue.land_sets(queue.done.len(), start, rest);
            if following {
                queue.showing = queue.done.len() - 1;
            }
            (next_step(failed, &queue.remaining), extra, dropped)
        };

        self.show_sets(extra, dropped, mode, client_keys.as_deref(), cx);

        match step {
            Step::Finished => {
                // A batch that answered with one set after all leaves a queue
                // holding a single result, which is no queue at all: dropped,
                // so the tab is exactly what an ordinary run leaves behind and
                // nothing writes a second snapshot of the same grid.
                if let Some(query) = self
                    .profile_mut()
                    .and_then(|profile| profile.session.query_tab_mut(id))
                    && query
                        .queue
                        .as_ref()
                        .is_some_and(|queue| queue.done.len() < 2 && queue.remaining.is_empty())
                {
                    query.queue = None;
                }
                cx.notify();
            }
            Step::Ask => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.queue_failure = Some(tab);
                }
                cx.notify();
            }
            Step::Next(_) => self.send_next_statement(tab, cx),
        }
    }

    /// The mode and in-memory sort a queue's result lands under, read before
    /// the session is borrowed, for the reason the completion handler reads
    /// them before `slot`.
    fn landing_view(profile: Option<&Profile>, tab: Tab) -> (Mode, Option<Vec<SortKey>>) {
        let mode = profile.map(|profile| profile.mode).unwrap_or_default();
        let client_keys = profile
            .and_then(|profile| profile.session.sorting(tab))
            .and_then(Sorting::client_keys)
            .map(<[SortKey]>::to_vec);
        (mode, client_keys)
    }

    /// Fill the grids `Queue::land_sets` took for a batch's further sets, and say
    /// how many found none.
    fn show_sets(
        &mut self,
        extra: Vec<(Entity<TableState<ResultGrid>>, QueryResult)>,
        dropped: usize,
        mode: Mode,
        client_keys: Option<&[SortKey]>,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        for (grid, set) in extra {
            // The server's sort is not carried over -- a header click reads
            // the statement in the buffer, and this set is not the one the
            // buffer names -- but the view's in-memory keys are, and there is
            // no layout to carry. A multi-set batch is uneditable, so there is
            // no target.
            grid.update(cx, |table, cx| {
                *table.delegate_mut() = ResultGrid::new(set, mode)
                    .with_engine(engine)
                    .with_client_sort(client_keys);
                table.refresh(cx);
            });
        }
        if dropped > 0 {
            let (sets, them) = match dropped {
                1 => ("set", "it is"),
                _ => ("sets", "they are"),
            };
            self.note(
                format!(
                    "The batch returned {dropped} more result {sets} than there were grids \
                     reserved for it, and {them} not shown."
                ),
                cx,
            );
        }
    }

    /// Take the next statement off the queue and send it, with the empty grid
    /// it will fill — the one the last statement produced is in `done` and is
    /// never written to again.
    fn send_next_statement(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(query) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        let Some(queue) = &mut query.queue else {
            return;
        };
        if queue.remaining.is_empty() {
            return;
        }
        let range = queue.remaining.remove(0);
        let sql = queue.sql[range.clone()].to_string();
        queue.awaiting = true;
        // Follow the statement going out only for someone already on the
        // newest result; a reader parked on an earlier one is left there.
        if queue.showing + 1 >= queue.done.len() {
            queue.showing = queue.done.len();
        }
        if let Some(grid) = queue.spare.pop() {
            query.results = grid;
        }
        query.sent_from = Some((range.start, sql.clone()));
        self.execute_sql(sql, tab, cx);
    }

    /// Show one of a queue's results. `index` is a position in `done`, or
    /// `done.len()` for the statement still in flight.
    ///
    /// Nothing moves: `QueryTab::shown` reads the result out of `done`, so
    /// selecting one is only ever an index. That is what makes this safe while
    /// a later statement is running — the slot that run writes into is the
    /// tab's own, and no chip but the last points at it.
    ///
    /// ponytail: `ran_from` stays on the statement that ran last, so a
    /// selected failure offers no jump into the buffer (`error_span` refuses
    /// text that is not there). Keep the statement beside its `start` in
    /// `Finished` and read it from there if the jump is wanted.
    pub(crate) fn show_queued_result(&mut self, tab: Tab, index: usize, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(queue) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
            .and_then(|query| query.queue.as_mut())
        else {
            return;
        };
        // The in-flight slot is only a chip while something is in it, and a
        // statement not yet sent has nothing to show.
        let last = queue.done.len();
        if index > last
            || (index == last && !queue.awaiting)
            || queue
                .done
                .get(index)
                .is_some_and(|finished| matches!(finished.state, QueryState::Idle))
        {
            return;
        }
        queue.showing = index;
        cx.notify();
    }

    /// Abandon the rest of a queue stopped by a failed statement, leaving
    /// what has run on screen.
    pub(crate) fn stop_queue(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(Tab::Query(id)) = profile.session.queue_failure.take() else {
            return;
        };
        if let Some(query) = profile.session.query_tab_mut(id)
            && let Some(queue) = &mut query.queue
        {
            queue.remaining.clear();
        }
        cx.notify();
    }

    /// Carry on with the statement after the one that failed.
    pub(crate) fn continue_queue(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(tab) = profile.session.queue_failure.take() else {
            return;
        };
        self.send_next_statement(tab, cx);
    }

    /// Stop whatever queue the named tab is part way through. Nothing that
    /// has run is touched: the results already on it are still results.
    pub(crate) fn stop_queue_on(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.drop_rest_of_queue(tab);
        self.release_queue(tab, cx);
    }

    /// Unsend whatever of a queue has not gone out, leaving the statement in
    /// flight to land.
    fn drop_rest_of_queue(&mut self, tab: Tab) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.queue_failure == Some(tab) {
            profile.session.queue_failure = None;
        }
        if let Some(query) = profile.session.query_tab_mut(id)
            && let Some(queue) = &mut query.queue
        {
            queue.remaining.clear();
            // Statements waiting to run at once with others are always the
            // last of `done`, and never the one on screen, so dropping them
            // moves no other result.
            queue
                .done
                .retain(|finished| !matches!(finished.state, QueryState::Idle));
        }
    }

    /// Let go of a statement the queue sent that will never land: one the
    /// mode gate refused, or one a reconnect abandoned.
    ///
    /// Only `advance_queue` clears `awaiting` otherwise, and it runs on a
    /// result. A statement that produces none would leave the flag set for
    /// good, and with it the strip keeps a chip for a statement that is not
    /// coming and holds the selection on it.
    fn release_queue(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(queue) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
            .and_then(|query| query.queue.as_mut())
        else {
            return;
        };
        if !std::mem::take(&mut queue.awaiting) {
            return;
        }
        // That chip is gone, so a selection resting on it falls back to the
        // last result that did run.
        queue.showing = queue.showing.min(queue.done.len().saturating_sub(1));
        cx.notify();
    }

    fn sent_from(&mut self, tab: Tab, start: usize, sql: &str) {
        if let Tab::Query(id) = tab
            && let Some(tab) = self
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(id))
        {
            tab.sent_from = Some((start, sql.to_string()));
        }
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
        _: &mut Window,
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
            workspace.refuse_run(tab, message.into(), cx);
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
        let (start, sql) = match self.sql_to_run(&editor, cx) {
            Some(Ok(statement)) => statement,
            Some(Err(message)) => return failure(self, &message, cx),
            None => return failure(self, "There is no statement to explain.", cx),
        };
        if let Err(message) = sql::explainable(engine, &sql) {
            return failure(self, &message, cx);
        }
        self.sent_from(tab, start, &sql);

        let suffix = engine.explain_suffix(action.mode);
        self.execute_and_then(
            format!("{prefix}{sql}{suffix}"),
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
        // No buffer in front is nothing to save, and a name asked for anyway
        // would have nowhere to go when it is given.
        if self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .is_none()
        {
            return;
        }
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
        let chip = match (tab, &previous) {
            (Tab::Query(_), Some(previous)) => Some(TabKey::Saved(previous.clone())),
            (Tab::Query(id), None) => Some(TabKey::Unsaved(id)),
            (Tab::Object(_), _) => None,
        };
        // Written first, then the old name dropped: a failed delete leaves two
        // copies, which is recoverable, and the other order loses the query.
        if let Some(previous) = previous.filter(|previous| previous != &name)
            && let Err(message) = store::delete_query(&id, &previous)
        {
            self.note(message, cx);
        }

        if let Some(profile) = self.profile_mut() {
            // Before the list it reads moves: the chip's old key is only in the
            // strip until then.
            if let Some(from) = chip {
                profile.session.rekey(&from, TabKey::Saved(name.clone()));
            }
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.naming = false;
        }
        // Naming a relation's buffer is how it stops being a relation's buffer:
        // it leaves the object world entirely and becomes a saved query, which
        // is the only place a name means anything.
        match tab {
            Tab::Object(object) => {
                self.close_object(object, window, cx);
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
                queued_results: 0,
            },
            self.engine(),
            Sorting::new(self.settings.client_sort),
            self.settings.row_limit,
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        profile.session.queries.push(tab);
        // Placed at the end of the strip rather than left for `strip_order`'s
        // default, which groups every unsaved buffer ahead of the saved
        // queries and objects regardless of when each was opened.
        profile.session.place_last(TabKey::Unsaved(id));
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), window, cx);
    }

    /// Close an unsaved buffer. Its scratch file goes with it: an unnamed
    /// buffer is its text, and closing one is discarding both.
    pub(crate) fn close_buffer(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let position = profile.session.queries.iter().position(|tab| tab.id == id);
        let Some(position) = position else {
            return;
        };
        self.stop_run(Tab::Query(id), cx);
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let in_front = (profile.session.active == Tab::Query(id))
            .then(|| profile.session.fallback(Tab::Query(id)));
        profile.session.queries.remove(position);
        let profile_id = profile.id.clone();
        if let Some(fallback) = in_front {
            self.front_after_close(fallback, window, cx);
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
            self.activate_tab(Tab::Query(id), window, cx);
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
                queued_results: 0,
            },
            self.engine(),
            Sorting::new(self.settings.client_sort),
            self.settings.row_limit,
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.queries.push(tab);
        profile.session.notice = notice;
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), window, cx);
    }

    /// A statement out of the history, back in the buffer.
    ///
    /// Appended rather than swapped in, for the reason `apply_in_buffer`
    /// appends: recalling a statement is not a reason to take away what is
    /// already written, and the statement that runs is the statement on screen.
    /// The cursor lands on it, because that is what `cmd+enter` reads to decide
    /// what to send. With no buffer in front it goes into a new one, rather
    /// than into whichever buffer happens to be behind the tab in front.
    pub(crate) fn recall_statement(
        &mut self,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .is_none()
        {
            self.new_query(&NewQuery, window, cx);
        }
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
        else {
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
        self.activate_tab(Tab::Query(id), window, cx);
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
            Some(id) => self.activate_tab(Tab::Query(id), window, cx),
            None => self.new_query(&NewQuery, window, cx),
        }
    }

    /// The chip's own delete: the first click arms it and the second one means
    /// it. Quieter than the dialog `cmd+w` raises, because the trash icon is
    /// already an unambiguous ask and the tab it belongs to is right there.
    pub(crate) fn arm_delete_saved_query(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.pending_delete.as_deref() != Some(&name) {
            profile.session.pending_delete = Some(name);
            cx.notify();
            return;
        }
        self.delete_saved_query(name, window, cx);
    }

    pub(crate) fn delete_saved_query(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let id = profile.id.clone();
        // Read before the delete, because afterwards nothing on the session
        // still points at the file and only this says which tab did.
        let was_open = profile.session.tab_holding(&name);
        let in_front = was_open
            .map(Tab::Query)
            .filter(|open| profile.session.active == *open)
            .map(|open| profile.session.fallback(open));
        if let Err(message) = store::delete_query(&id, &name) {
            self.note(message, cx);
            return;
        }
        // Here, once the file is gone, and not when `cmd+w` asked: answering
        // Cancel to that leaves the tab, and its statement, running.
        if let Some(open) = was_open {
            self.stop_run(Tab::Query(open), cx);
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
                profile.session.queries.retain(|tab| tab.id != open);
                // With the tab, as in `close_buffer`: a snapshot with no
                // tab left to come back to is rows the next buffer to be
                // handed this id would show as its own.
                let _ = store::remove_grid(&id, &store::query_grid_key(open));
            }
        }
        if let Some(fallback) = in_front {
            self.front_after_close(fallback, window, cx);
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
    /// runs a tab's statement routes through this one -- `connection.query` is
    /// called in one place, `execute_unchecked`, and `connection.generated`
    /// there and for the status bar's Count and the reference arrow's checks,
    /// which run the same gates themselves. A stopped statement is
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

        self.execute_unchecked(sql, tab, refresh, keep_rows, explain, None, cx);
    }

    /// Runs a statement without consulting the connection's mode. Only two
    /// callers: `execute_and_then`, once the mode has allowed it, and the
    /// prompt's own Run.
    ///
    /// `keep_rows` leaves whatever the grid is showing in place until the new
    /// result lands, for a relation's refresh: the same statement asked again,
    /// so the rows under it are what it is about to return. Every other run
    /// clears them first, because rows from the previous statement sitting
    /// under the one now running cannot be told from fresh ones.
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
    ///
    /// `lane` is a statement of a queue running at once with others: it runs
    /// on the profile it was issued to rather than the one in front, lands in
    /// its own entry of the queue, runs on a connection of its own under a
    /// token of its own, and hands its result to `land_lane` rather than
    /// `advance_queue`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_unchecked(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        explain: Option<ExplainMode>,
        lane: Option<Lane>,
        cx: &mut Context<Self>,
    ) {
        let profile = match &lane {
            Some(lane) => self.issued_to(&lane.profile, lane.generation),
            None => self.profile_mut(),
        };
        let Some(profile) = profile else {
            return;
        };
        // Read before the task, which outlives the borrow of `self`.
        let engine = profile.config.engine();
        let connection = profile.connection();
        let id = profile.id.clone();
        let generation = profile.generation;
        let read_only = profile.mode == Mode::ReadOnly;
        let alongside = lane.is_some();
        let Some((state, results)) = profile.session.slot_at(tab, lane.as_ref()) else {
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
            if let Some(lane) = lane {
                self.land_lane(tab, lane, Vec::new(), cx);
            }
            return;
        };
        // A lane's own, so a Cancel on its chip stops it and not the others.
        let cancel = match lane {
            Some(_) => CancelToken::alongside(),
            None => CancelToken::default(),
        };
        let started = std::time::Instant::now();
        let previous = std::mem::replace(
            state,
            QueryState::Running {
                started,
                cancelling: None,
                cancel: cancel.clone(),
            },
        );
        // What a cancelled refresh goes back to: the rows it kept on screen
        // are still the ones that state describes.
        let restored = keep_rows.then_some(previous);
        // The spinner beside the clock redraws every frame, but not under
        // reduce motion, so the clock cannot lean on it to tick.
        cx.spawn({
            let (id, lane) = (id.clone(), lane.clone());
            async move |workspace, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    let still_running = workspace.update(cx, |workspace, cx| {
                        let running = workspace
                            .issued_to(&id, generation)
                            .and_then(|profile| profile.session.slot_at(tab, lane.as_ref()))
                            .is_some_and(|(state, _)| {
                                matches!(state, QueryState::Running { started: at, .. } if *at == started)
                            });
                        if running {
                            cx.notify();
                        }
                        running
                    });
                    if !matches!(still_running, Ok(true)) {
                        break;
                    }
                }
            }
        })
        .detach();
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
        let shown = {
            let table = results.read(cx);
            let (names, widths) = table.delegate().layout();
            let vertical = table.vertical_scroll_handle.0.borrow().base_handle.offset();
            (
                names,
                widths,
                vertical,
                table.horizontal_scroll_handle.offset(),
            )
        };
        if !keep_rows && explain.is_none() {
            results.update(cx, |table, cx| {
                *table.delegate_mut() = ResultGrid::empty();
                // Rows dropped from the gutter's multi-row selection too, for
                // the same reason.
                table.delegate_mut().clear_row_selection();
                // The inspector reads whatever row is selected, and a row index
                // means nothing once the rows behind it are gone.
                table.clear_selection(cx);
                table.refresh(cx);
            });
        } else if keep_rows {
            // Edits staged on the rows kept go now, as they went when a
            // refresh blanked the grid: they were made against rows about to
            // be replaced, and the replacement would drop them unseen.
            results.update(cx, |table, cx| {
                table.delegate_mut().discard_pending();
                table.refresh(cx);
            });
        }
        cx.notify();

        // Read from the statement that is about to run, so the headers say what
        // the rows on screen are actually ordered by rather than what dbdelve
        // last intended to ask for.
        // A preview runs in the engine's own paging, which on SQL Server the
        // grammar cannot read; its sort is read from the spelling it was
        // generated in. A buffer's statement is read as typed.
        let keys = match tab {
            Tab::Object(_) => sql::order_by(engine, &sql::unpaged(&sql)),
            Tab::Query(_) => sql::order_by(engine, &sql),
        };
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
            let suffix = engine.explain_suffix(mode);
            let bare = sql.strip_prefix(prefix).unwrap_or(&sql);
            bare.strip_suffix(suffix).unwrap_or(bare).to_string()
        });
        if let Tab::Query(query) = tab
            && lane.is_none()
            && let Some(tab) = self
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(query))
        {
            let ran = statement.as_ref().or(explained.as_ref());
            tab.ran_from = tab.sent_from.take().filter(|(_, sent)| Some(sent) == ran);
        }
        // Recorded on the way out rather than on the way back: the history is
        // what the user ran, and a statement that failed is exactly the one
        // worth getting back. Only the buffer's — a relation's preview is SQL
        // dbdelve wrote, and nobody asked to keep it.
        if let Some(statement) = &statement
            && let Some(profile) = self.issued_to(&id, generation)
        {
            // A line that could not be written is not worth a notice on every
            // run: the statement is still in the buffer, so nothing is lost.
            let _ = store::append_history(&profile.id, statement);
            remember_statement(&mut profile.session.history, statement);
        }
        // Everything an object tab runs is dbdelve's, and a query tab runs
        // dbdelve's statement only when an edit is applied from its grid, the
        // one run that carries a refresh.
        let generated = matches!(tab, Tab::Object(_)) || refresh.is_some();
        // Marked on the way out, like the history: a statement that failed
        // part way may still have opened a transaction.
        if !generated && let Some(profile) = self.issued_to(&id, generation) {
            let leaves = sql::leaves(engine, explained.as_deref().unwrap_or(&sql));
            if let Some(open) = leaves.transaction {
                profile.in_transaction = open;
            }
            profile.session_changed |= leaves.settings;
        }
        // A plan is never cut short: its rows are the plan, not data.
        let limit = match tab {
            Tab::Query(query) if explain.is_none() => self
                .issued_to(&id, generation)
                .and_then(|profile| profile.session.query_tab(query))
                .and_then(|query| query.row_limit),
            _ => None,
        };
        // Rows land in the grid as they arrive, but only in a grid cleared for
        // them: a refresh keeps the rows it is replacing until the new ones are
        // whole, and a plan's rows never reach the grid at all. A relation's
        // preview is dbdelve's statement and streams like the user's; a query
        // tab's generated run is an edit, with no rows to stream.
        let streams = matches!(tab, Tab::Object(_)) || !generated;
        let feed = (!keep_rows && streams && explain.is_none()).then(Feed::default);
        if let Some(feed) = feed.clone() {
            let (id, results, lane) = (id.clone(), results.clone(), lane.clone());
            let (names, widths) = (shown.0.clone(), shown.1.clone());
            cx.spawn(async move |workspace, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                    let running = workspace.update(cx, |workspace, cx| {
                        let Some(profile) = workspace.issued_to(&id, generation) else {
                            return false;
                        };
                        let mode = profile.mode;
                        let running = profile.session.slot_at(tab, lane.as_ref()).is_some_and(|(state, _)| {
                            matches!(state, QueryState::Running { started: at, .. } if *at == started)
                        });
                        // The count in the status bar is the workspace's to
                        // draw, not the grid's. Only when something arrived:
                        // the clock has a tick of its own.
                        let began = results.read(cx).delegate().streamed_set();
                        if running && pour(&feed, &results, mode, engine, (&names, &widths), cx) {
                            cx.notify();
                        }
                        // A grid started for the rows lacks what the structure
                        // marked on the one before it, as a landed run's does.
                        if let Tab::Object(object) = tab
                            && results.read(cx).delegate().streamed_set() != began
                        {
                            workspace.mark_columns(object, cx);
                        }
                        running
                    });
                    if !matches!(running, Ok(true)) {
                        break;
                    }
                }
            })
            .detach();
        }
        let fed = feed.clone();
        let reads_only = sql::stoppable(engine, &sql);
        let work = move || {
            let fetch = Fetch {
                limit,
                feed: fed.as_ref(),
                reads_only,
                held: None,
            };
            let result = match (generated, alongside) {
                (true, _) => connection.generated(&sql, &cancel, fed.as_ref()),
                (false, false) => connection.query(&sql, &cancel, fetch),
                (false, true) => connection.query_alongside(&sql, &cancel, fetch, read_only),
            };
            // A connection of the lane's own that failed is gone with it, and
            // says nothing about the profile's.
            let lost = !alongside && result.is_err() && connection.is_lost();
            (result, lost)
        };
        // Up to eight of a queue's statements each hold a thread for as long
        // as the server takes, which on Linux can fill GPUI's background
        // executor and stall the Cancel spawned behind them.
        let query_task = match alongside {
            true => cx.background_executor().spawn(on_own_thread(work)),
            false => cx.background_executor().spawn(async move { work() }),
        };

        cx.spawn(async move |workspace, cx| {
            let (result, lost) = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    // The plan is carried out of this block rather than stored
                    // inside it: `slot` holds the session borrowed, and the tab
                    // it belongs on has to be reached through the same session.
                    let (succeeded, produced_grid, plan, rest, notice) = {
                        let Some(profile) = workspace.issued_to(&id, generation) else {
                            workspace.drop_stale_run(&id, tab, lane.as_ref(), cx);
                            return;
                        };
                        // Read before `slot`, which borrows the session and not
                        // this field -- but the mode a result lands under has to
                        // be the mode at landing time, not a stale default.
                        let mode = profile.mode;
                        // Read before `slot` for the same reason. A view sorted
                        // in memory is sorted again as its rows land: a page
                        // turn or a re-run hands them back in the server's order.
                        let client_keys = profile
                            .session
                            .sorting(tab)
                            .and_then(Sorting::client_keys)
                            .map(<[SortKey]>::to_vec);
                        // Before `slot`: a tab closed mid-run still ran on the
                        // session that died.
                        if lost {
                            profile.state = ProfileState::Failed(format!(
                                "Connection to {} was lost.",
                                profile.config.endpoint()
                            ));
                            // Nothing left in the queue can run on a dead
                            // session, so it ends here rather than asking
                            // whether to Continue into "not open".
                            if let Tab::Query(query) = tab
                                && let Some(queue) = profile
                                    .session
                                    .query_tab_mut(query)
                                    .and_then(|query| query.queue.as_mut())
                            {
                                queue.remaining.clear();
                            }
                        }
                        let Some((state, results)) = profile.session.slot_at(tab, lane.as_ref())
                        else {
                            return;
                        };

                        let cancelled = matches!(
                            state,
                            QueryState::Running {
                                cancelling: Some(_),
                                ..
                            }
                        );
                        // Cancelled part way, a lone read keeps the rows it
                        // had: they are what the user stopped it to look at.
                        // Not a write's, which the cancel undid, and not when
                        // something other than the cancel ended it. Whatever
                        // the cancel cost besides is said either way.
                        let result = match result {
                            Err(error)
                                if cancelled
                                    && reads_only
                                    && db::is_cancel(engine, &error)
                                    && results.read(cx).delegate().streamed_set().is_some() =>
                            {
                                Ok(QueryResult {
                                    columns: results.read(cx).delegate().result().columns.clone(),
                                    elapsed: started.elapsed(),
                                    stopped: Some(Stopped::Cancelled),
                                    notice: db::cancel_cost(engine, &error, true),
                                    ..QueryResult::default()
                                })
                            }
                            Err(mut error) if cancelled && db::is_cancel(engine, &error) => {
                                if let Some(cost) = db::cancel_cost(engine, &error, reads_only)
                                    && cost != error.message
                                {
                                    error.message = format!("{}\n\n{cost}", error.message);
                                }
                                Err(error)
                            }
                            result => result,
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
                                (true, false, Some(plan), Vec::new(), result.notice)
                            }
                            Ok(mut result) => {
                                // Rows that streamed in were scrolled through as
                                // they came, so the grid stays where the user
                                // took it rather than where the last run left it.
                                let streamed = results.read(cx).delegate().streamed_set().is_some();
                                if let Some(feed) = &feed {
                                    let (names, widths) = (&shown.0, &shown.1);
                                    pour(feed, &results, mode, engine, (names, widths), cx);
                                    let (mut rows, streamed_columns) =
                                        results.update(cx, |table, _| {
                                            let grid = table.delegate_mut();
                                            (grid.take_rows(), grid.result().columns.clone())
                                        });
                                    // The describe after the statement can learn
                                    // nothing (a cancel, several statements) where
                                    // the one beside it already had.
                                    if result
                                        .columns
                                        .iter()
                                        .all(|column| column.data_type.is_none())
                                        && streamed_columns.len() == result.columns.len()
                                    {
                                        result.columns = streamed_columns;
                                    }
                                    rows.append(&mut result.rows);
                                    result.rows = rows;
                                    // A cancelled stream's size was never
                                    // reported; it is what arrived.
                                    if result.stopped == Some(Stopped::Cancelled) {
                                        result.bytes = result
                                            .rows
                                            .iter()
                                            .flatten()
                                            .flatten()
                                            .map(String::len)
                                            .sum();
                                    }
                                }
                                // Off the result before it reaches the grid:
                                // the sets behind the first become results of
                                // their own and have no business inside this
                                // one's snapshot.
                                let rest = std::mem::take(&mut result.rest);
                                let notice = result.notice.take();
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
                                    rows: result.total_rows(),
                                    bytes: result.bytes,
                                    elapsed: result.elapsed,
                                    rows_affected: result.rows_affected,
                                };
                                let produced_grid = !result.columns.is_empty();
                                // Sorted in memory as it lands, the rows move
                                // out from under any index into them.
                                let reordered =
                                    client_keys.as_deref().is_some_and(|keys| !keys.is_empty());
                                results.update(cx, |table, cx| {
                                    let selection = table.delegate().selection();
                                    let sort = sort_columns(engine, &keys, &result.columns);
                                    let (names, widths, vertical, horizontal) = &shown;
                                    *table.delegate_mut() = ResultGrid::new(result, mode)
                                        .with_engine(engine)
                                        .with_sort(sort, sortable)
                                        .with_client_sort(client_keys.as_deref())
                                        .with_layout(names, widths);
                                    // Streamed rows are the finished result's
                                    // first rows, in order, so what was picked
                                    // out of them as they came still names
                                    // them. Rows kept through a refresh kept
                                    // their selection too, but its index now
                                    // names whichever row the new result put
                                    // there.
                                    let kept = streamed
                                        && !reordered
                                        && table.delegate_mut().select(selection);
                                    if !kept {
                                        table.clear_selection(cx);
                                    }
                                    table.refresh(cx);
                                    if !streamed && table.delegate().layout().0 == *names {
                                        table
                                            .vertical_scroll_handle
                                            .0
                                            .borrow()
                                            .base_handle
                                            .set_offset(*vertical);
                                        table.horizontal_scroll_handle.set_offset(*horizontal);
                                    }
                                });
                                // Rows that arrived before their types, all of
                                // them if the describe beside the run learned
                                // nothing.
                                if feed.is_some() {
                                    render_aside(&results, engine, cx);
                                }
                                (true, produced_grid, None, rest, notice)
                            }
                            Err(mut error) => {
                                // Rows of a statement that failed part way are
                                // not its result, and nothing on screen would
                                // say they were cut short.
                                if feed.is_some() {
                                    results.update(cx, |table, cx| {
                                        if table.delegate().streamed_set().is_some() {
                                            *table.delegate_mut() = ResultGrid::empty();
                                            table.refresh(cx);
                                        }
                                    });
                                }
                                // Into the user's statement, not the prefix
                                // dbdelve put in front of it, which is on no
                                // screen to count from.
                                let prefix = explain
                                    .and_then(|mode| engine.explain_prefix(mode))
                                    .map_or(0, str::len);
                                error.position = error
                                    .position
                                    .and_then(|position| position.checked_sub(prefix));
                                *state = match restored {
                                    Some(previous) if cancelled => previous,
                                    _ => QueryState::Failed(error),
                                };
                                (false, false, None, Vec::new(), None)
                            }
                        }
                    };

                    if keep_rows
                        && let Some(profile) = workspace.issued_to(&id, generation)
                        && profile.session.notice.as_deref() == Some(Self::REFRESHING)
                    {
                        profile.session.notice = None;
                    }
                    if let Some(notice) = notice
                        && let Some(profile) = workspace.issued_to(&id, generation)
                    {
                        profile.session.notice = Some(notice);
                    }
                    if succeeded && let Some(profile) = workspace.issued_to(&id, generation) {
                        // A statement that returned no columns produced no grid,
                        // so it is not the statement to go back to — which is
                        // what keeps an applied UPDATE from becoming the query
                        // an apply re-runs.
                        // A lane's is set once the last of the queue lands,
                        // since the order they finish in is not the buffer's.
                        if produced_grid
                            && lane.is_none()
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

                    let had_refresh = refresh.is_some();
                    if succeeded && let Some(refresh) = refresh {
                        match refresh {
                            Refresh::Statement(sql) => workspace.execute_sql(sql, tab, cx),
                            Refresh::Relation(id) => workspace.refresh_relation(id, cx),
                        }
                    }
                    // A refresh belongs to a generated edit, so the run that
                    // carries one is never a queued statement and the two
                    // follow-ups cannot both fire for one result. The tab may
                    // still hold a finished queue -- its results stay
                    // switchable -- which is why this turns on the refresh and
                    // not on the queue's presence. `Queue::awaiting` is what
                    // keeps the edit's own result out of `done`.
                    match lane {
                        Some(lane) => workspace.land_lane(tab, lane, rest, cx),
                        None if !had_refresh => workspace.advance_queue(tab, rest, cx),
                        None => {}
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
    ///
    /// A lane's entry goes, with every statement of its queue not yet sent,
    /// and the tab is let go of once the last lane still out is dropped too.
    /// Only while the tab is still running the lane's own run (`run`): one
    /// let go of by an earlier lane may have started another since, whose
    /// statements can start where this one's did.
    pub(crate) fn drop_stale_run(
        &mut self,
        id: &str,
        tab: Tab,
        lane: Option<&Lane>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        if let Some(lane) = lane
            && !matches!(
                profile.session.slot(tab),
                Some((QueryState::Running { cancel, .. }, _)) if cancel.is(&lane.run)
            )
        {
            return;
        }
        if let Some(&Lane { start, .. }) = lane
            && let Tab::Query(query) = tab
            && let Some(queue) = profile
                .session
                .query_tab_mut(query)
                .and_then(|query| query.queue.as_mut())
        {
            queue.done.retain(|finished| {
                finished.start != start && !matches!(finished.state, QueryState::Idle)
            });
            queue.showing = queue.showing.min(queue.done.len().saturating_sub(1));
            cx.notify();
            if queue
                .done
                .iter()
                .any(|finished| matches!(finished.state, QueryState::Running { .. }))
            {
                return;
            }
        }
        let Some((state, _)) = profile.session.slot(tab) else {
            return;
        };
        if matches!(state, QueryState::Running { .. }) {
            *state = QueryState::Idle;
            cx.notify();
        }
        // A queued statement that will never land holds no chip, as
        // `release_queue` says; that one reaches only the active profile.
        if let Tab::Query(query) = tab
            && let Some(queue) = profile
                .session
                .query_tab_mut(query)
                .and_then(|query| query.queue.as_mut())
            && std::mem::take(&mut queue.awaiting)
        {
            queue.remaining.clear();
            queue.showing = queue.showing.min(queue.done.len().saturating_sub(1));
        }
        // Rows of a run nobody will land are not a result, and a grid still
        // marked as streaming would refuse every copy as if more were coming.
        if let Some((_, results)) = profile.session.slot(tab) {
            results.update(cx, |table, cx| {
                if table.delegate().streamed_set().is_some() {
                    *table.delegate_mut() = ResultGrid::empty();
                    table.refresh(cx);
                }
            });
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
        let formatted = match crate::sql::format(self.engine(), &text) {
            Ok(formatted) => formatted,
            Err(refusal) => {
                self.note(refusal.into(), cx);
                return;
            }
        };
        if formatted == text {
            return;
        }

        // ponytail: the cursor lands at the top of the statement it was in,
        // not on the token it was on -- a reflow moves every offset, and the
        // statement is the unit the user was working in. Map the token too if
        // the jump ever reads as losing your place.
        let buffer = Buffer::for_engine(self.engine(), &text);
        let was_in = buffer
            .statement_at(cursor)
            .and_then(|range| {
                buffer
                    .statements()
                    .iter()
                    .position(|r| r.start == range.start)
            })
            .and_then(|index| {
                Buffer::for_engine(self.engine(), &formatted)
                    .statements()
                    .get(index)
                    .map(|r| r.start)
            });
        let position = was_in.map_or(Position::new(0, 0), |offset| {
            position_at(&formatted, offset)
        });

        editor.update(cx, |editor, cx| {
            editor.set_value(formatted, window, cx);
            editor.set_cursor_position(position, window, cx);
        });
    }

    /// `cmd+/`: comment out every line the selection touches, or uncomment
    /// them when they all already are.
    pub(crate) fn toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .profile()
            .and_then(|profile| profile.session.editor(profile.session.active))
        else {
            return;
        };
        let marker = self.engine().syntax().line_comment();
        editor.update(cx, |editor, cx| {
            let Some((lines, toggled, after)) =
                toggled_comments(&editor.value(), editor.selected_range(), marker)
            else {
                return;
            };
            // ponytail: only the active cursor's lines are toggled, and any
            // other cursors are dropped -- gpui-base 0.6.4 exposes no way to
            // read them all. Undo reselects the whole lines, not the caret, for
            // the same lack of API.
            //
            // An IME composition in progress would otherwise be what `replace`
            // overwrites, ahead of the selection.
            gpui::EntityInputHandler::unmark_text(editor, window, cx);
            // Through `replace`, not `set_value`: that one wipes the undo stack.
            editor.set_selected_range(lines, cx);
            editor.replace(toggled, window, cx);
            editor.set_selected_range(after, cx);
        });
    }

    /// Put the cursor on what the error in front points at, with it selected.
    pub(crate) fn jump_to_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
        else {
            return;
        };
        let Some((_, span)) = error_in_buffer(tab, cx) else {
            return;
        };
        tab.editor.clone().update(cx, |editor, cx| {
            editor.set_selected_range(span, cx);
            editor.focus(window, cx);
        });
    }

    /// The statement to run and where it begins in the buffer.
    pub(crate) fn sql_to_run(
        &self,
        editor: &Entity<EditorState>,
        cx: &App,
    ) -> Option<Result<(usize, String), String>> {
        let engine = self.engine();
        let editor = editor.read(cx);
        let sql = editor.value();
        let selection = editor.selected_range();

        if !selection.is_empty() {
            let selected = &sql[selection.clone()];
            return match sql::one_batch(engine, selected) {
                Ok(batch) if batch.trim().is_empty() => None,
                // Past the empty case, `one_batch` hands back a slice of the
                // selection, so the two pointers say where in it the batch sits.
                Ok(batch) => Some(Ok((
                    selection.start + (batch.as_ptr() as usize - selected.as_ptr() as usize),
                    batch.to_string(),
                ))),
                Err(message) => Some(Err(message)),
            };
        }

        let range = Buffer::for_engine(engine, &sql).statement_at(editor.cursor())?;
        Some(
            sql::batch_repeats(engine, &sql, range.end)
                .map(|()| (range.start, sql[range].to_string())),
        )
    }
}

/// The whole lines `selection` touches, without the final newline. A
/// selection that ends at the very start of a line has not taken that line in.
fn line_span(text: &str, selection: Range<usize>) -> Range<usize> {
    let start = text[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    let last = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[last..].find('\n').map_or(text.len(), |i| last + i);
    start..end
}

/// Comment out the lines `selection` touches in `text`, or uncomment them when
/// every one already is: the span to replace, what replaces it, and the
/// selection to leave behind. `None` when there is nothing to toggle.
///
/// Blank lines in a block are left alone either way; a blank line on its own
/// takes the marker, to start a comment on. Commenting puts the marker at the
/// shallowest indent of the block, so the block keeps its shape.
fn toggled_comments(
    text: &str,
    selection: Range<usize>,
    marker: &str,
) -> Option<(Range<usize>, String, Range<usize>)> {
    // The editor keeps a pasted `\r\n`; the marker must not land between them.
    let content = |line: &'_ str| line.strip_suffix('\r').unwrap_or(line).len();
    let indent_of = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();

    let span = line_span(text, selection.clone());
    let block = &text[span.clone()];
    let filled: Vec<&str> = block
        .split('\n')
        .map(|line| &line[..content(line)])
        .filter(|line| !line.trim().is_empty())
        .collect();
    let lone_blank = filled.is_empty() && !block.contains('\n');
    if filled.is_empty() && !lone_blank {
        return None;
    }
    let uncomment = !lone_blank
        && filled
            .iter()
            .all(|line| line[indent_of(line)..].starts_with(marker));
    let indent = filled.iter().map(|line| indent_of(line)).min().unwrap_or(0);

    let mut toggled = String::with_capacity(block.len());
    let mut caret = selection.start;
    let mut line_start = span.start;
    for (i, line) in block.split('\n').enumerate() {
        if i > 0 {
            toggled.push('\n');
        }
        let body = &line[..content(line)];
        // Where in the line, how many bytes go, and what comes in.
        let edit = if lone_blank {
            Some((body.len(), 0, marker.len() + 1))
        } else if body.trim().is_empty() {
            None
        } else if uncomment {
            let at = indent_of(body);
            let space = body[at + marker.len()..].starts_with(' ');
            Some((at, marker.len() + usize::from(space), 0))
        } else {
            Some((indent, 0, marker.len() + 1))
        };
        match edit {
            None => toggled.push_str(line),
            Some((at, removed, inserted)) => {
                toggled.push_str(&line[..at]);
                if inserted > 0 {
                    toggled.push_str(marker);
                    toggled.push(' ');
                }
                toggled.push_str(&line[at + removed..]);
                // A caret is on one line only, so this runs for it at most once.
                let at = line_start + at;
                if lone_blank {
                    caret = at + inserted;
                } else if caret >= at + removed {
                    caret = caret - removed + inserted;
                } else if caret > at {
                    caret = at;
                }
            }
        }
        line_start += line.len() + 1;
    }

    let after = if selection.is_empty() {
        caret..caret
    } else {
        span.start..span.start + toggled.len()
    };
    Some((span, toggled, after))
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

/// Where a query tab's error points in its buffer: the position to name and
/// the span to select. `None` unless the buffer still holds the statement that
/// failed, where it stood when it ran.
///
/// ponytail: copies the buffer on every render an error is on screen. Compare
/// against the rope in place if a buffer ever gets big enough to feel it.
pub(crate) fn error_in_buffer(tab: &QueryTab, cx: &App) -> Option<(Position, Range<usize>)> {
    let QueryState::Failed(DbError {
        position: Some(position),
        ..
    }) = &tab.query
    else {
        return None;
    };
    let (start, sql) = tab.ran_from.as_ref()?;
    error_span(&tab.editor.read(cx).value(), *start, sql, *position)
}

/// `position` is a byte offset into `sql`, which ran from `start` in `buffer`.
/// The span is the word there, or the one character when it is not in a word,
/// and empty on whitespace or at the end of the statement.
fn error_span(
    buffer: &str,
    start: usize,
    sql: &str,
    position: usize,
) -> Option<(Position, Range<usize>)> {
    if !buffer.get(start..)?.starts_with(sql) {
        return None;
    }
    let rest = sql.get(position..)?;
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let token = match rest.chars().next() {
        Some(c) if is_word(c) => {
            sql[..position].trim_end_matches(is_word).len()
                ..position + rest.find(|c| !is_word(c)).unwrap_or(rest.len())
        }
        Some(c) if !c.is_whitespace() => position..position + c.len_utf8(),
        _ => position..position,
    };
    Some((
        position_at(buffer, start + position),
        start + token.start..start + token.end,
    ))
}

/// Render a grid's cells that arrived before their types, off the frame
/// thread, and put them back where they still are.
fn render_aside(results: &Entity<TableState<ResultGrid>>, engine: Engine, cx: &mut App) {
    let cells = db::unrendered(engine, results.read(cx).delegate().result());
    if cells.is_empty() {
        return;
    }
    let rendering = cx
        .background_executor()
        .spawn(async move { db::render(cells) });
    let results = results.clone();
    cx.spawn(async move |cx| {
        let rendered = rendering.await;
        results.update(cx, |table, cx| {
            table.delegate_mut().apply_rendered(rendered);
            cx.notify();
        });
    })
    .detach();
}

/// Move what has arrived into the grid. A new result set starts the grid over,
/// as it would have when the set's statement finished. `true` when anything
/// on screen changed.
fn pour(
    feed: &Feed,
    results: &Entity<TableState<ResultGrid>>,
    mode: Mode,
    engine: Engine,
    (names, widths): (&[String], &[Pixels]),
    cx: &mut App,
) -> bool {
    let fed = feed.take();
    // No result set has begun: nothing has a column to go under yet.
    if fed.set == 0 {
        return false;
    }
    let (typed, changed) = results.update(cx, |table, cx| {
        let changed = table.delegate().streamed_set() != Some(fed.set)
            || table.delegate().result().columns != fed.columns
            || table.delegate().filled() != fed.filled
            || !fed.rows.is_empty();
        let mut typed = false;
        if table.delegate().streamed_set() != Some(fed.set) {
            typed = fed.columns.iter().any(|column| column.data_type.is_some());
            let columns = QueryResult {
                columns: fed.columns,
                ..QueryResult::default()
            };
            *table.delegate_mut() = ResultGrid::new(columns, mode)
                .with_engine(engine)
                .with_layout(names, widths)
                .streaming(fed.set);
            table.refresh(cx);
        } else if table.delegate().result().columns != fed.columns {
            table.delegate_mut().set_column_types(fed.columns);
            typed = true;
            cx.notify();
        }
        table.delegate_mut().fill(fed.filled);
        if !fed.rows.is_empty() {
            let digits = |rows: usize| rows.max(1).ilog10();
            let before = table.delegate().result().rows.len();
            table.delegate_mut().append_rows(fed.rows);
            // The row-number gutter is sized to the last row's number, and
            // only a refresh lays its width out again.
            if digits(table.delegate().result().rows.len()) != digits(before) {
                table.refresh(cx);
            }
            cx.notify();
        }
        (typed, changed)
    });
    if typed {
        render_aside(results, engine, cx);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` with `|` marking the caret, or `[` `]` the selection, toggled.
    fn toggle(text: &str, marker: &str) -> Option<String> {
        let start = text.find(['|', '[']).unwrap();
        let end = text.find(']').map_or(start, |i| i - 1);
        let text = text.replace(['|', '[', ']'], "");
        let (span, toggled, after) = toggled_comments(&text, start..end, marker)?;
        let mut out = text;
        out.replace_range(span, &toggled);
        if after.is_empty() {
            out.insert(after.start, '|');
        } else {
            out.insert(after.end, ']');
            out.insert(after.start, '[');
        }
        Some(out)
    }

    #[test]
    fn a_comment_toggle_round_trips_a_block_at_its_shallowest_indent() {
        let commented = toggle("[select *\n  from t\n\n  where x]", "--").unwrap();
        assert_eq!(commented, "[-- select *\n--   from t\n\n--   where x]");
        assert_eq!(
            toggle(&commented, "--").unwrap(),
            "[select *\n  from t\n\n  where x]"
        );
        // One line still bare means the block gets commented, not uncommented.
        assert_eq!(toggle("[// a\nb]", "//").unwrap(), "[// // a\n// b]");
        assert_eq!(toggle("  --x|", "--").unwrap(), "  x|");
    }

    #[test]
    fn a_caret_keeps_its_place_in_the_text_it_was_on() {
        assert_eq!(toggle("|    select 1", "--").unwrap(), "|    -- select 1");
        assert_eq!(toggle("    sel|ect 1", "--").unwrap(), "    -- sel|ect 1");
        assert_eq!(toggle("  |  -- select 1", "--").unwrap(), "  |  select 1");
        // Inside the marker that goes, it lands where the marker was.
        assert_eq!(toggle("    -|- select 1", "--").unwrap(), "    |select 1");
    }

    #[test]
    fn a_blank_line_takes_a_marker_only_on_its_own() {
        assert_eq!(toggle("  |  ", "--").unwrap(), "    -- |");
        assert_eq!(toggle("|", "//").unwrap(), "// |");
        assert_eq!(toggle("a\n[  \n   ]\nb", "--"), None);
    }

    #[test]
    fn a_crlf_line_keeps_its_line_ending_whole() {
        assert_eq!(toggle("a\r\n|\r\nb", "--").unwrap(), "a\r\n-- |\r\nb");
        assert_eq!(toggle("[a\r\n\r\nb]", "--").unwrap(), "[-- a\r\n\r\n-- b]");
        assert_eq!(toggle("-- a|\r\nb", "--").unwrap(), "a|\r\nb");
    }

    #[test]
    fn a_line_span_covers_whole_lines_but_not_one_the_selection_only_reaches() {
        let text = "one\ntwo\nthree";
        assert_eq!(&text[line_span(text, 5..5)], "two");
        assert_eq!(&text[line_span(text, 1..9)], "one\ntwo\nthree");
        // Selecting "two\n" by whole line ends at the start of "three".
        assert_eq!(&text[line_span(text, 4..8)], "two");
    }

    #[test]
    fn a_position_lands_on_the_line_the_offset_is_on() {
        let sql = "select 1;\n  select 2;";
        let offset = Buffer::parse(sql).statements()[1].start;
        assert_eq!(position_at(sql, offset), Position::new(1, 2));
    }

    #[test]
    fn an_error_lands_on_its_word_in_the_buffer() {
        let buffer = "select 1;\nSELECT 'éé', nme FROM t;";
        let sql = "SELECT 'éé', nme FROM t";
        // Postgres says character 14; `postgres::query_error` hands it over as
        // byte 15, which is past the two two-byte characters.
        let (at, span) = error_span(buffer, 10, sql, 15).unwrap();
        assert_eq!(at, Position::new(1, 13));
        assert_eq!(&buffer[span], "nme");
        // Inside the word, it is still the whole word.
        let (_, span) = error_span(buffer, 10, sql, 16).unwrap();
        assert_eq!(&buffer[span], "nme");
    }

    #[test]
    fn an_error_off_a_word_selects_one_character_or_none() {
        let buffer = "SELECT (1 FROM";
        let (_, span) = error_span(buffer, 0, buffer, 7).unwrap();
        assert_eq!(&buffer[span], "(");
        let (_, span) = error_span(buffer, 0, buffer, 6).unwrap();
        assert!(span.is_empty());
        // "at end of input": the cursor goes after the statement, and nothing
        // past it is taken for part of it.
        let (at, span) = error_span("SELECT 1 FROM;", 0, "SELECT 1 FROM", 13).unwrap();
        assert_eq!((at, span), (Position::new(0, 13), 13..13));
    }

    #[test]
    fn an_error_is_not_placed_in_a_buffer_that_moved_on() {
        let sql = "SELECT nme FROM t";
        assert!(error_span(sql, 0, sql, sql.len() + 1).is_none());
        // Not a character boundary.
        assert!(error_span("SELECT 'é'", 0, "SELECT 'é'", 9).is_none());
        assert!(error_span("-- edited\nSELECT nme FROM t", 0, sql, 7).is_none());
        assert!(error_span("SELECT", 0, sql, 0).is_none());
        assert!(error_span(sql, 40, sql, 0).is_none());
    }
}

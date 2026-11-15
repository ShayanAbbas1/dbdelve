//! Running statements, and the saved queries and buffers they come from.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use std::ops::Range;

use gpui_component::table::TableState;

use crate::db::{
    ApplyOutcome, ApplyStage, Drift, Feed, Fetch, QueryResult, Row, StepKind, Stopped,
};
use crate::result_grid::{ColumnView, ColumnWidth, StepRow};

use super::*;
use crate::session::{
    APPLY_IN_TRANSACTION, AfterEdits, Binding, Bound, Finished, Lane, PendingDelete,
    PendingDiscard, PendingRun, QueryRename, Queue, Resume, Routed, Step, TabKey, Verification,
    next_step, query_name_refusal, reveals_results,
};

impl Workspace {
    /// Edits sitting in the visible grid, waiting to be written back. Read off
    /// the grid rather than held anywhere, so nothing can disagree with the
    /// cells about whether there is something to apply.
    pub(crate) fn has_pending_edits(&self, cx: &App) -> bool {
        self.pending_edit_count(cx) > 0
    }

    /// Whether the draft in front holds a row added in the grid, so its size
    /// is a count of changes and not of edited cells.
    pub(crate) fn draft_holds_rows(&self, cx: &App) -> bool {
        self.profile()
            .and_then(|profile| profile.session.active_results())
            .is_some_and(|results| results.read(cx).delegate().holds_rows())
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
            profile.config.target().edits_rows()
                && profile.session.active_results().is_some_and(|results| {
                    let grid = results.read(cx);
                    grid.delegate()
                        .active()
                        .is_some_and(|(row, _)| grid.delegate().row_key(row).is_some())
                })
        })
    }

    /// Whether the selection of a Draft view of a table holds a row that Delete
    /// row can mark: an added row, which it removes, or a row named by its
    /// primary key. Read off the grid for the reason
    /// [`Workspace::has_nameable_row`] is.
    pub(crate) fn has_markable_row(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            let tab = profile.session.active;
            profile.session.active_results().is_some_and(|results| {
                markable_row(
                    profile.config.target(),
                    tab,
                    profile.session.edit_mode(tab),
                    results.read(cx).delegate(),
                )
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
    /// put tokio on GPUI's executor"): the runtime belongs to the blocking driver and lives and
    /// dies on the thread the driver is running on. Nothing tokio-shaped is
    /// handed to GPUI's executor, which is the thing that panics.
    pub(crate) fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_front(cx);
    }

    /// What Cancel does to the run of the tab in front, for the action and for
    /// an apply review's Cancel while its batch runs.
    pub(crate) fn cancel_front(&mut self, cx: &mut Context<Self>) {
        // The session a bound tab runs on, which is where the statements that
        // are stopped by what the connection holds of them (MongoDB's, and
        // ClickHouse's fallback) are kept.
        let Some(connection) = self
            .profile()
            .and_then(|profile| profile.connection_to_stop(profile.session.active))
        else {
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
        let (number, engine) = self
            .profile()
            .map(|profile| {
                (
                    crate::logging::connection_number(&profile.id),
                    profile.config.target().label(),
                )
            })
            .unwrap_or_default();
        tracing::info!(
            connection = number,
            engine,
            statements = cancels.len(),
            "cancel requested"
        );
        let cancel_task = spawn_cancel(cx.background_executor(), move || {
            cancels
                .iter()
                .map(|cancel| connection.cancel(cancel))
                .fold(Ok(()), Result::and)
        });

        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                tracing::warn!(connection = number, engine, "cancel failed");
                tracing::debug!(
                    connection = number,
                    "cancel failed: {}",
                    crate::db::loggable_error(&error.message)
                );
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
                            if target == LimitTarget::Default && workspace.settings_open {
                                workspace.settings_focus.focus(window, cx);
                            } else {
                                workspace.refocus_front();
                            }
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
        // A tab asking for the keyboard back, as closing the palette does,
        // would take it after the field and blur it shut.
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = false;
        }
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
        if self.import_covers() {
            return;
        }
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        // An object tab has no buffer of its own: running it again is a refresh
        // of the rows dbdelve fetched, which is the only thing there is to run.
        if let Tab::Object(id) = tab {
            // A diagram has nothing to run, and its own `secondary-enter`
            // (Open Structure) sits behind this unscoped binding: handing the
            // keystroke on is what lets that one have it.
            if matches!(
                profile.session.active_object().map(|tab| &tab.body),
                Some(ObjectBody::Diagram { .. } | ObjectBody::Users { .. })
            ) {
                cx.propagate();
                return;
            }
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
        // Before the tab is touched: a Run over edits nobody applied replaces
        // them, and Cancel has to leave the tab as it was.
        if self.held_for_edits(tab, AfterEdits::Run, cx) {
            return;
        }
        // Here and not where each statement goes out, which a queue does once
        // per statement: someone reading its messages stays on them while the
        // rest arrive.
        if let Tab::Query(id) = tab
            && let Some(query) = self
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(id))
        {
            query.showing_messages = false;
        }

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
            // Never a bound tab's: its statements share the session of its
            // database, and one at once would run on a fresh session, which
            // is not on the binding.
            // ponytail: a bound alongside session that re-applies the
            // binding would let them go at once.
            let reads_at_once = reads.len() > 1
                && self.profile().is_some_and(|profile| {
                    !profile.session.is_bound(tab) && profile.in_turn().is_none() && {
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
        self.execute_own(sql, tab, None, cx);
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
                profile.session.reveal_results(tab);
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
        if !profile.session.is_bound(tab) && session::runs_at_once(engine, profile.mode, &texts) {
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
        self.execute_own(sql, tab, None, cx);
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

    /// Run one of a finished queue's statements again, as `sql`, into its own
    /// result. A lane, because the tab's slot is the last statement's result
    /// and the others are only ever written through `done`. `false` when
    /// nothing ran.
    pub(crate) fn rerun_queued(
        &mut self,
        tab: Tab,
        index: usize,
        sql: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Tab::Query(id) = tab else {
            return false;
        };
        let rerun = AfterEdits::Rerun {
            index,
            sql: sql.clone(),
        };
        if self.held_for_edits(tab, rerun, cx) {
            return false;
        }
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let issued = (profile.id.clone(), profile.generation);
        let Some(query) = profile.session.query_tab_mut(id) else {
            return false;
        };
        if matches!(query.query, QueryState::Running { .. }) {
            return false;
        }
        // The lane lands wherever `start` first leads, so it has to lead here.
        let Some(finished) = query
            .queue
            .as_mut()
            .filter(|queue| {
                queue
                    .done
                    .get(index)
                    .is_some_and(|finished| queue.lane(finished.start) == Some(index))
            })
            .and_then(|queue| queue.done.get_mut(index))
        else {
            return false;
        };
        finished.sql = sql;
        finished.state = QueryState::Idle;
        query.query = QueryState::Running {
            started: std::time::Instant::now(),
            cancelling: None,
            cancel: CancelToken::alongside(),
        };
        self.start_lane(tab, &issued.0, issued.1, cx);
        cx.notify();
        true
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
        let issued = self
            .issued_to(&lane.profile, lane.generation)
            .map(|profile| &*profile);
        let engine = issued
            .map(|profile| profile.config.engine())
            .unwrap_or_default();
        let (mode, client_keys) = Self::landing_view(issued, tab);
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
        self.show_sets(extra, dropped, engine, mode, client_keys.as_deref(), cx);
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
    ///
    /// On the profile the statement was issued to, `profile` on `generation`,
    /// which need not be the one in front by now.
    fn advance_queue(
        &mut self,
        profile: &str,
        generation: u64,
        tab: Tab,
        rest: Vec<db::QueryResult>,
        cx: &mut Context<Self>,
    ) {
        let Tab::Query(id) = tab else {
            return;
        };
        let issued = self.issued_to(profile, generation).map(|profile| &*profile);
        let engine = issued
            .map(|profile| profile.config.engine())
            .unwrap_or_default();
        let (mode, client_keys) = Self::landing_view(issued, tab);
        let (step, extra, dropped) = {
            let Some(profile) = self.issued_to(profile, generation) else {
                return;
            };
            // A bound tab's statements run on its database's session, whose
            // transaction and settings are its own; with none open there is
            // nothing certain to say.
            let uncertain = match profile.binding_of(tab) {
                None => profile.in_transaction || profile.session_changed,
                Some(binding) => profile
                    .route(Some(binding))
                    .bound()
                    .is_none_or(|session| session.changed()),
            };
            let Some(query) = profile.session.query_tab_mut(id) else {
                return;
            };
            if query.queue.is_none() {
                return;
            }
            let failed = matches!(query.query, QueryState::Failed(_));
            let mut state = query.query.clone();
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
            if let QueryState::Failed(error) = &mut state
                && let Some(note) = queue.committed_note(engine, uncertain)
            {
                error.message = format!("{}\n\n{note}", error.message.trim_end());
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

        self.show_sets(extra, dropped, engine, mode, client_keys.as_deref(), cx);
        // The next statement is sent and gated through the profile in front,
        // whose tab ids count from zero like every other's.
        let in_front = self
            .profile()
            .is_some_and(|front| front.id == profile && front.generation == generation);

        match step {
            Step::Finished => {
                // A batch that answered with one set after all leaves a queue
                // holding a single result, which is no queue at all: dropped,
                // so the tab is exactly what an ordinary run leaves behind and
                // nothing writes a second snapshot of the same grid.
                if let Some(query) = self
                    .issued_to(profile, generation)
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
                if let Some(profile) = self.issued_to(profile, generation) {
                    profile.session.queue_failure = Some(tab);
                }
                cx.notify();
            }
            Step::Next(_) if in_front => self.send_next_statement(tab, cx),
            // Sent on when the connection comes forward (`resume_parked`).
            Step::Next(_) => {
                if let Some(profile) = self.issued_to(profile, generation) {
                    profile.session.parked.push((tab, generation));
                }
                cx.notify();
            }
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
        engine: Engine,
        mode: Mode,
        client_keys: Option<&[SortKey]>,
        cx: &mut Context<Self>,
    ) {
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
    /// Send on the queues of the profile in front that were parked while
    /// another connection was. One parked on an earlier generation ran on a
    /// session that is gone, with whatever it had open, so its rest is
    /// dropped and said to be.
    pub(crate) fn resume_parked(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let generation = profile.generation;
        for (tab, ran_on) in std::mem::take(&mut profile.session.parked) {
            if ran_on == generation {
                self.send_next_statement(tab, cx);
                continue;
            }
            let Some(profile) = self.profile_mut() else {
                return;
            };
            if let Tab::Query(id) = tab
                && let Some(queue) = profile
                    .session
                    .query_tab_mut(id)
                    .and_then(|query| query.queue.as_mut())
                && !queue.remaining.is_empty()
            {
                queue.remaining.clear();
                profile.session.notice = Some(
                    "The rest of the selection was not run: the connection it ran on was opened \
                     again since."
                        .into(),
                );
            }
        }
        cx.notify();
    }

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
        self.execute_own(sql, tab, None, cx);
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
        let Some(query) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        let switched = match &mut query.queue {
            Some(queue) => {
                // The in-flight slot is only a chip while something is in it,
                // and a statement not yet sent has nothing to show.
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
                std::mem::replace(&mut queue.showing, index) != index
            }
            // With no queue the switcher is there for the Messages chip, and
            // the slot is the one result beside it.
            None => false,
        };
        query.showing_messages = false;
        query.results_hidden = false;
        // The form was built from the rows it was opened over, which can be
        // another table's, and a review's batch is refreshed by whichever
        // result is on screen when it runs.
        if switched && let Some(profile) = self.profile_mut() {
            profile.session.insert_form.take_if(|form| form.tab == tab);
            profile.session.drop_apply_review_of(tab);
        }
        cx.notify();
    }

    /// Show the run's server messages in place of the result the switcher
    /// had selected, which stays selected to come back to.
    pub(crate) fn show_messages(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Tab::Query(id) = tab else {
            return;
        };
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(query) = profile.session.query_tab_mut(id) else {
            return;
        };
        if query.messages().is_empty() || std::mem::replace(&mut query.showing_messages, true) {
            return;
        }
        query.results_hidden = false;
        // The grid and any form beside it unmount, and whichever of them held
        // focus would take the window's only dispatch path with it.
        profile.session.editor_needs_focus = true;
        cx.notify();
    }

    pub(crate) fn next_result(
        &mut self,
        _: &NextResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_result(1, window, cx);
    }

    pub(crate) fn previous_result(
        &mut self,
        _: &PreviousResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_result(-1, window, cx);
    }

    /// The switcher's chips from the keyboard, Messages among them, through
    /// the doors a click on each goes through.
    fn step_result(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
        else {
            return;
        };
        let id = Tab::Query(tab.id);
        let in_grid = tab.shown().1.focus_handle(cx).contains_focused(window, cx);
        match crate::session::stepped(&tab.switcher_chips(), tab.switcher_selected(), step) {
            Some(Some(index)) => self.show_queued_result(id, index, cx),
            Some(None) => self.show_messages(id, cx),
            None => return,
        }
        // A chord pressed in the grid it just swapped out would leave the
        // keyboard on a grid no longer on screen, with no dispatch path.
        if in_grid && let Some(crate::session::Focus::Grid(grid)) = self.front_grid(cx) {
            grid.update(cx, |table, cx| {
                crate::result_grid::take_keyboard(table, window, cx)
            });
            if let Some(profile) = self.profile_mut() {
                profile.session.editor_needs_focus = false;
            }
            cx.notify();
            return;
        }
        self.refocus_front();
    }

    /// The Messages chip, and back off it to the result it stood in for.
    pub(crate) fn toggle_messages(
        &mut self,
        _: &ShowMessages,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
        else {
            return;
        };
        let id = Tab::Query(tab.id);
        match tab.switcher_selected() {
            Some(_) => self.show_messages(id, cx),
            None => {
                let showing = tab.queue.as_ref().map_or(0, |queue| queue.showing);
                self.show_queued_result(id, showing, cx);
            }
        }
        self.refocus_front();
    }

    pub(crate) fn copy_plan_action(
        &mut self,
        _: &CopyPlan,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_plan(cx);
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
        if self.import_covers() {
            return;
        }
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

        let cannot = format!(
            "{} cannot {}.",
            engine.label(),
            action.mode.label().to_lowercase()
        );
        if engine.explain_prefix(action.mode).is_none() {
            // Reachable only if a menu offers a mode the engine does not have,
            // which is what `explain_prefix` returning `None` is there to stop.
            failure(self, &cannot, cx);
            return;
        }
        let (start, sql) = match self.sql_to_run(&editor, cx) {
            Some(Ok(statement)) => statement,
            Some(Err(message)) => return failure(self, &message, cx),
            None => return failure(self, "There is no statement to explain.", cx),
        };
        if let Err(message) = sql::explainable(engine, &sql) {
            return failure(self, &message, cx);
        }
        let Some(statement) = engine.explain(action.mode, &sql) else {
            return failure(self, &cannot, cx);
        };
        self.sent_from(tab, start, &sql);

        self.execute_own(statement, tab, Some(action.mode), cx);
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
        if !tab.flips_to_plan() {
            return;
        }
        tab.showing_plan = showing;
        tab.results_hidden = false;
        // Either half is asked for by name, so neither leaves Messages over it.
        let switched = match showing {
            true => tab.show_slot(),
            false => {
                tab.showing_messages = false;
                false
            }
        };
        if switched {
            profile
                .session
                .insert_form
                .take_if(|form| form.tab == Tab::Query(id));
            profile.session.drop_apply_review_of(Tab::Query(id));
        }
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
        if self.import_covers() {
            return;
        }
        // No buffer in front is nothing to save, and a name asked for anyway
        // would have nowhere to go when it is given.
        if self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .is_none()
        {
            return;
        }
        // Only a buffer with nowhere to go has to ask for a name.
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
        if let Some(message) =
            query_name_refusal(&name, previous.as_deref(), &profile.session.saved_queries)
        {
            self.note(message, cx);
            return;
        }

        let sql = editor.read(cx).value().to_string();
        if let Err(message) = store::save_query_as(&id, previous.as_deref(), &name, &sql) {
            self.note(message, cx);
            return;
        }
        let scratch_error = match tab {
            Tab::Query(tab) => store::delete_scratch(&id, tab).err(),
            Tab::Object(_) => None,
        };
        let chip = match (tab, &previous) {
            (Tab::Query(_), Some(previous)) => Some(TabKey::Saved(previous.clone())),
            (Tab::Query(id), None) => Some(TabKey::Unsaved(id)),
            (Tab::Object(_), _) => None,
        };

        if let Some(profile) = self.profile_mut() {
            // Before the tab takes its new name: the chip's old key is only in
            // the strip until then.
            if let Some(from) = chip {
                profile.session.rekey(&from, TabKey::Saved(name.clone()));
            }
            profile.session.naming = false;
            if let Some(previous) = &previous {
                profile.shortcuts.rename_query(previous, &name);
            }
        }
        if previous.is_some() {
            self.remember_profiles(cx);
        }
        self.reload_saved_queries(cx);
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
                // The library was rebuilt above while the tab was still a
                // scratch, with no row to select.
                self.select_front_row(cx);
            }
        }
        self.note(
            scratch_error.unwrap_or_else(|| format!("Saved {name}.")),
            cx,
        );
        self.remember_profiles(cx);
        cx.notify();
    }

    /// A new empty buffer, beside the ones already open.
    ///
    /// It used to clear the buffer in front, which is why a dirty scratch was
    /// persisted and then emptied: one editor meant a new query had nowhere to
    /// go but on top of the old one.
    pub(crate) fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.open_query_tab(None, window, cx);
    }

    /// [`Workspace::new_query`]'s tab, bound to a database where `binding`
    /// says so (New Query Here).
    pub(crate) fn open_query_tab(
        &mut self,
        binding: Option<Binding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                database: None,
                schema: None,
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
        let mut tab = tab;
        tab.binding = binding;
        profile.session.queries.push(tab);
        // Placed at the end of the strip rather than left for `strip_order`'s
        // default, which groups every unsaved buffer ahead of the saved
        // queries and objects regardless of when each was opened.
        profile.session.place_last(TabKey::Unsaved(id));
        // ponytail: a bound tab completes from the profile's own catalog, not
        // its database's; the upgrade is a provider per (profile, database)
        // catalog.
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), window, cx);
    }

    /// Close a query tab. An unsaved buffer's scratch file goes with it: an
    /// unnamed buffer is its text, and closing one is discarding both. A saved
    /// query's text is written to its file first and the file stays, so the
    /// query is still in the sidebar; a write that fails keeps the tab open.
    pub(crate) fn close_buffer(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.query_tab(id) else {
            return;
        };
        if let Some(name) = &tab.open_query {
            let sql = tab.editor.read(cx).value().to_string();
            if let Err(message) = store::write_query(&profile.id, name, &sql) {
                self.note(message, cx);
                return;
            }
        }
        self.remove_query_tab(id, window, cx);
    }

    /// Take a query tab out of the strip, its text written or not: closing
    /// writes it first, deleting its query does not.
    fn remove_query_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(position) = profile.session.queries.iter().position(|tab| tab.id == id) else {
            return;
        };
        let saved = profile.session.queries[position].open_query.is_some();
        self.stop_run(Tab::Query(id), cx);
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let in_front = (profile.session.active == Tab::Query(id))
            .then(|| profile.session.fallback(Tab::Query(id)));
        profile.session.queries.remove(position);
        profile.session.forget(Tab::Query(id));
        let profile_id = profile.id.clone();
        if let Some(fallback) = in_front {
            self.front_after_close(fallback, window, cx);
        }

        if !saved && let Err(message) = store::delete_scratch(&profile_id, id) {
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
                profile.session.notice = Some(format!("{name} no longer exists."));
                self.reload_saved_queries(cx);
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
                name: Some(name.clone()),
                active: true,
                queued_results: 0,
                database: None,
                schema: None,
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
        profile.session.place_last(TabKey::Saved(name));
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
        let appended = appended_statement(self.engine(), &text, &sql);
        let line = appended.lines().count().saturating_sub(sql.lines().count()) as u32;
        editor.update(cx, |editor, cx| {
            rewrite(editor, &appended, window, cx);
            editor.set_cursor_position(Position::new(line, 0), window, cx);
        });
        self.activate_tab(Tab::Query(id), window, cx);
    }

    /// A new query tab holding `statement`, run at once: a row opened as a
    /// result of its own, by a command that is then the user's to read, edit
    /// and run again, through the same gate as any they type. Bound where
    /// the row was read, since the statement does not name its database.
    pub(crate) fn run_in_new_query(
        &mut self,
        statement: String,
        binding: Option<Binding>,
        at_most: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.profile().map(|profile| profile.session.next_query_id);
        self.open_query_tab(binding, window, cx);
        // Not whatever tab is in front if the new one could not be made: that
        // buffer is the user's.
        let Some(editor) = self
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .filter(|tab| Some(tab.id) == id)
            .map(|tab| tab.editor.clone())
        else {
            return;
        };
        // An unlimited tab would let a Redis scan follow its cursor to the end.
        if let Some(tab) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id.unwrap_or_default()))
        {
            tab.row_limit.get_or_insert(at_most);
        }
        editor.update(cx, |editor, cx| {
            editor.set_value(statement, window, cx);
            editor.set_cursor_position(Position::new(0, 0), window, cx);
        });
        self.run_query(&RunQuery, window, cx);
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

    /// Turn a saved query's sidebar row into a field holding its name, all
    /// of it selected. `enter` renames, and `escape` or a click elsewhere
    /// leaves it as it was.
    pub(crate) fn start_query_rename(
        &mut self,
        name: String,
        row: gpui::SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_rename(name, false, row, window, cx);
    }

    /// The field over a saved query's row or a folder's.
    pub(crate) fn start_rename(
        &mut self,
        name: String,
        folder: bool,
        row: gpui::SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = if folder { "Folder name" } else { "Query name" };
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(placeholder);
            input.set_value(name.clone(), window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            |workspace, input, event: &InputEvent, _, cx| {
                let Some(rename) = workspace
                    .profile_mut()
                    .and_then(|profile| profile.session.query_rename.as_mut())
                    .filter(|rename| rename.input == *input)
                else {
                    return;
                };
                match event {
                    InputEvent::PressEnter { .. } => workspace.commit_query_rename(cx),
                    InputEvent::Change => {
                        if rename.error.take().is_some() {
                            cx.notify();
                        }
                    }
                    InputEvent::Blur => {
                        workspace.drop_query_rename();
                        cx.notify();
                    }
                    InputEvent::Focus => {}
                }
            },
        )
        .detach();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.query_rename = Some(QueryRename {
            name,
            folder,
            row,
            input,
            error: None,
        });
        profile.session.query_rename_needs_focus = true;
        cx.notify();
    }

    /// Rename the query the sidebar's field is over to what it holds, through
    /// the same checks and store call as the tab's own rename, so a tab
    /// holding it follows the new name.
    pub(crate) fn commit_query_rename(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(rename) = profile.session.query_rename.as_ref() else {
            return;
        };
        let (previous, typed) = (
            rename.name.clone(),
            rename.input.read(cx).value().trim().to_string(),
        );
        if typed == previous {
            self.end_query_rename(cx);
            return;
        }
        if rename.folder {
            self.commit_folder_rename(previous, typed, cx);
            return;
        }
        let id = profile.id.clone();
        let holder = profile.session.tab_holding(&previous);
        let refused = query_name_refusal(&typed, Some(&previous), &profile.session.saved_queries);
        // An open tab's text is the query's, written or not: renaming it is
        // what the tab's own rename would write.
        let sql = match refused {
            Some(message) => Err(message),
            None => match holder.and_then(|tab| profile.session.query_tab(tab)) {
                Some(tab) => Ok(tab.editor.read(cx).value().to_string()),
                None => store::read_query(&id, &previous)
                    .and_then(|sql| sql.ok_or_else(|| format!("{previous} no longer exists."))),
            },
        };
        let written = sql.and_then(|sql| store::save_query_as(&id, Some(&previous), &typed, &sql));
        match written {
            Ok(()) => {}
            Err(message) => {
                if let Some(rename) = self
                    .profile_mut()
                    .and_then(|profile| profile.session.query_rename.as_mut())
                {
                    rename.error = Some(message);
                }
                cx.notify();
                return;
            }
        }
        if let Some(profile) = self.profile_mut() {
            if let Some(holder) = holder {
                profile.session.rekey(
                    &TabKey::Saved(previous.clone()),
                    TabKey::Saved(typed.clone()),
                );
                if let Some(tab) = profile.session.query_tab_mut(holder) {
                    tab.open_query = Some(typed.clone());
                }
            }
            profile.session.notice = Some(format!("Renamed {previous} to {typed}."));
            profile.shortcuts.rename_query(&previous, &typed);
        }
        self.end_query_rename(cx);
        self.reload_saved_queries(cx);
        self.remember_profiles(cx);
    }

    /// `escape` over the sidebar's rename field: the name stays as it was.
    pub(crate) fn cancel_query_rename(&mut self, cx: &mut Context<Self>) -> bool {
        let renaming = self
            .profile()
            .is_some_and(|profile| profile.session.query_rename.is_some());
        if renaming {
            self.end_query_rename(cx);
        }
        renaming
    }

    /// Put the field away while it holds focus, and hand the keyboard back:
    /// an unmounted field keeps the window's focus, and every binding with it.
    pub(crate) fn end_query_rename(&mut self, cx: &mut Context<Self>) {
        self.drop_query_rename();
        self.refocus_front();
        cx.notify();
    }

    fn drop_query_rename(&mut self) {
        if let Some(profile) = self.profile_mut() {
            profile.session.query_rename = None;
            profile.session.query_rename_needs_focus = false;
        }
    }

    /// Raise the dialog that asks before a saved query is deleted. The one
    /// way in from the sidebar and the palette alike.
    pub(crate) fn ask_delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.pending_delete = Some(PendingDelete::Query(name));
        cx.notify();
    }

    /// Delete a saved query's file, and close the tab holding it: the query
    /// is gone, and its text with it. A tab with cell edits nobody applied
    /// asks first, as any close does, and the file stays until it is told.
    pub(crate) fn delete_saved_query(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.pending_delete = None;
        let holder = profile.session.tab_holding(&name).map(Tab::Query);
        if let Some(tab) = holder.filter(|tab| self.would_lose_on_close(*tab, cx)) {
            if let Some(profile) = self.profile_mut() {
                profile.session.pending_discard = Some(PendingDiscard {
                    tab,
                    then: AfterEdits::Close {
                        deleting: Some(name),
                    },
                    row: None,
                    also: Vec::new(),
                });
            }
            cx.notify();
            return;
        }
        self.delete_saved_query_now(name, window, cx);
        self.refocus_front();
    }

    /// The delete itself, once nothing is left to ask.
    pub(crate) fn delete_saved_query_now(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let id = profile.id.clone();
        let holder = profile.session.tab_holding(&name);
        if let Err(message) = store::delete_query(&id, &name) {
            self.note(message, cx);
            return;
        }
        if let Some(holder) = holder {
            self.remove_query_tab(holder, window, cx);
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = Some(format!("Deleted {name}."));
            profile.shortcuts.forget_query(&name);
        }
        self.reload_saved_queries(cx);
        self.remember_profiles(cx);
    }

    /// Read the profile in front's query files again, for a list that changed
    /// on disk, and redraw the sidebar that lists them.
    pub(crate) fn reload_saved_queries(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let id = profile.id.clone();
        profile.session.saved_queries = store::saved_queries(&id);
        self.refresh_query_library(&id, cx);
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

    /// A statement the user just asked to run: Run, the rest of the queue a
    /// Run started, and Explain. The only runs `Settings::run_unreadable` may
    /// let through unasked; everything that runs without being asked --
    /// previews, edits, refreshes -- goes through `execute_and_then` and keeps
    /// failing closed.
    pub(crate) fn execute_own(
        &mut self,
        sql: String,
        tab: Tab,
        explain: Option<ExplainMode>,
        cx: &mut Context<Self>,
    ) {
        self.execute_gated(sql, tab, None, false, explain, true, cx);
    }

    pub(crate) fn execute_and_then(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        explain: Option<ExplainMode>,
        cx: &mut Context<Self>,
    ) {
        self.execute_gated(sql, tab, refresh, keep_rows, explain, false, cx);
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_gated(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        explain: Option<ExplainMode>,
        own_run: bool,
        cx: &mut Context<Self>,
    ) {
        // ponytail: the entry points are the guard (`held_for_edits`), because
        // this runs after a sort has rewritten the buffer. This exists so a
        // path nobody guarded costs a notice rather than a draft; the upgrade
        // is to delete it once every caller is proven guarded.
        if refresh.is_none()
            && explain.is_none()
            && self
                .profile()
                .and_then(|profile| profile.session.results(tab))
                .is_some_and(|grid| grid.read(cx).delegate().has_pending())
        {
            if let Some(profile) = self.profile_mut() {
                profile.session.tab_notice = Some((
                    tab,
                    "The statement was not run: this tab has unapplied edits.".into(),
                ));
            }
            cx.notify();
            return;
        }
        // An idle-closed profile stays in front with its tabs (the idle
        // timer), so its next run is its reconnect: parked until the connect
        // lands, where `run_waiting_files` hands it back to `execute_sql` and
        // the mode gate judges it then. An explain, a refresh or an edit's
        // read-back is not the user pressing Run and goes on to fail as it
        // does with no connection.
        if let (Some(index), Tab::Query(query), None, None, false) =
            (self.active, tab, &refresh, &explain, keep_rows)
            && self
                .profiles
                .get(index)
                .is_some_and(|profile| matches!(profile.state, ProfileState::Idle))
        {
            let id = self.profiles[index].id.clone();
            self.runs_on_connect.push((id, query, sql));
            self.begin_connect(index, cx);
            return;
        }

        let verdict = sql::classify(self.engine(), &sql);
        let stopped = self
            .profile()
            .and_then(|profile| self.gate_run(profile, tab, &verdict, own_run));

        if stopped.is_some() {
            // What a Row send was to be followed by does not wait on the
            // prompt: a Run from it sends the row and the user repeats the
            // action, which has by then read the rows it names.
            let mut refresh = refresh;
            if let Some(Refresh::Row { then, .. }) = &mut refresh
                && let Some(then) = then.take()
            {
                self.put_back(tab, then);
            }
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
                    commit_cell: false,
                    own_run,
                });
            }
            cx.notify();
            return;
        }

        self.execute_unchecked(sql, tab, refresh, keep_rows, explain, None, cx);
    }

    /// A send that is dropped before it runs: the action it carried is put
    /// back, as it is on every other outcome that does not carry it out.
    fn drop_parked(&mut self, tab: Tab, refresh: Option<Refresh>) {
        if let Some(then) = refresh.and_then(Refresh::into_parked) {
            self.put_back(tab, then);
        }
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
    /// `explain` says this submission is the Explain command's, the statement
    /// under dbdelve's prefix, and diverts its result away from the grid and
    /// into the tab's plan, as a lone statement the user typed as an `EXPLAIN`
    /// is diverted without it (`sql::typed_explain`). It routes through here rather
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
            self.drop_parked(tab, refresh);
            return;
        };
        // Read before the task, which outlives the borrow of `self`.
        let target = profile.config.target();
        let engine = target.engine;
        let id = profile.id.clone();
        let generation = profile.generation;
        let issued_under = profile.mode;
        let read_only = issued_under == Mode::ReadOnly;
        let alongside = lane.is_some();
        // A tab bound to a database -- a query tab opened with New Query Here,
        // an object tab of another database -- runs on the session of that
        // database: the one open for it, else one this run opens first. A lane
        // belongs to the profile's own tabs, which are never bound.
        let binding = match &lane {
            None => profile.binding_of(tab),
            Some(_) => None,
        };
        let routed = profile.route(binding.clone());
        // An edit of dbdelve's, which a bound tab shares its session with
        // others for: not sent while a transaction is open on that session,
        // asked now and again once it is the edit's turn there (`Bound::edit`).
        let refusal_applies = binding.is_some()
            && matches!(
                refresh,
                Some(
                    Refresh::Statement { .. }
                        | Refresh::Earlier { .. }
                        | Refresh::Relation { .. }
                        | Refresh::Row { .. }
                )
            );
        let bound_refusal = binding
            .as_ref()
            .filter(|_| refusal_applies)
            .and_then(|binding| profile.bound_write_refusal(binding));
        // Asked again as it runs, not only when it was put up for review: a
        // transaction opened since would hold the change, and a rollback
        // would undo what the tab reported as applied. A run resumed from the
        // gate's prompt comes through here too.
        if matches!(refresh, Some(Refresh::Roles)) && profile.in_transaction {
            self.note(super::users::ROLE_CHANGE_IN_TRANSACTION.into(), cx);
            return;
        }
        // A write dbdelve generated for the grid: an Apply edits batch, an
        // Insert, a Delete row or a Row send.
        let edit = matches!(
            refresh,
            Some(
                Refresh::Statement { .. }
                    | Refresh::Earlier { .. }
                    | Refresh::Relation { .. }
                    | Refresh::Row { .. }
            )
        );
        // What the run is checked against, where its edit was reviewed for a
        // target that verifies an apply: the rows as they were fetched, read
        // again inside the apply's transaction. Cloned, as the refresh that
        // carries it goes on to the landing.
        let verification = match &refresh {
            Some(
                Refresh::Relation { verify, .. }
                | Refresh::Row { verify, .. }
                | Refresh::Statement { verify, .. }
                | Refresh::Earlier { verify, .. },
            ) => verify.clone(),
            _ => None,
        };
        // Every path that writes a grid's rows on a target that verifies
        // applies builds a `Verification`; this keeps a future one that does
        // not from sending the edit as the text alone, with no re-read and no
        // count (`AGENTS.md`, rule 2). A query tab's edit on a target that
        // does not review it (`Target::reviews_query_apply`) goes to its
        // buffer and is not one of these.
        let unverified = edit
            && verification.is_none()
            && target.verifies_applies()
            && match tab {
                Tab::Object(_) => true,
                Tab::Query(_) => target.reviews_query_apply(),
            };
        // A review that was open when a `COMMIT` went unanswered still holds
        // its draft, and running it again could write it twice. A run with no
        // verification is a read of the rows, which is what clears the mark.
        let outcome_unknown = verification.is_some()
            && profile
                .session
                .results(tab)
                .is_some_and(|results| results.read(cx).delegate().outcome_unknown());
        // A review written before the user opened a transaction is caught
        // here, at Run: the apply's own `BEGIN` and `COMMIT` would end theirs.
        // A bound tab's apply runs on its database's session, not the
        // profile's, and a transaction open there is `bound_refusal`'s.
        let refusal = outcome_unknown
            .then(|| super::editing::OUTCOME_UNKNOWN_REFUSAL.to_string())
            .or(bound_refusal)
            .or_else(|| {
                (verification.is_some() && binding.is_none() && profile.in_transaction)
                    .then(|| APPLY_IN_TRANSACTION.to_string())
            })
            .or_else(|| {
                unverified.then(|| "dbdelve refused to send an edit it could not verify.".into())
            })
            // An unverified edit on the connection's own session: not sent
            // when its own brackets would end the user's open transaction.
            .or_else(|| {
                (edit && binding.is_none())
                    .then(|| profile.own_write_refusal(&sql))
                    .flatten()
            });
        // An edit sent to a transactional SQL engine leaves the grid and the
        // draft where they are until the server answers, so a refusal or a
        // failure costs the user nothing. So does a verified one sent to an
        // ordered target, MongoDB, whose batch has no transaction: what failed
        // part way there has applied some of the draft, and the landing
        // removes exactly those rows from it by identity
        // (`ResultGrid::drop_applied`), so the kept draft is never sent twice
        // (`keeps_draft`).
        let kept = edit && keeps_draft(target, verification.is_some());
        // Whether a failed apply of this target has written the statements before
        // the one that failed (`Target::ordered_edits`).
        let ordered = target.ordered_edits();
        // Whether the batch is the grid's staged cells: Apply edits, from its
        // review or appended to a query tab's buffer. An Insert or a Delete
        // row sends none of them, and a relation tab's send without a review
        // has a landing of its own.
        let carries = kept
            && !matches!(refresh, Some(Refresh::Row { .. }))
            && match (profile.session.apply_review.as_ref(), tab) {
                (Some(review), _) if review.covers(tab) => review.edits,
                (_, Tab::Query(_)) => true,
                (_, Tab::Object(_)) => false,
            };
        // A statement the user typed as an `EXPLAIN` is theirs, run and kept in
        // the history as typed; only its answer goes where the Explain
        // command's does. Not in a queue, where every result holds a grid.
        let typed = match tab {
            Tab::Query(query) if explain.is_none() && refresh.is_none() && !alongside => profile
                .session
                .query_tab(query)
                .filter(|query| query.queue.is_none())
                .and_then(|_| sql::typed_explain(engine, &sql)),
            _ => None,
        };
        let explaining = explain.or(typed);
        let Some((state, _)) = profile.session.slot_at(tab, lane.as_ref()) else {
            return;
        };
        // Guarded here rather than in each caller: every path that runs SQL
        // routes through this one, and a caller that forgets would let two
        // results race into the grid with the older one landing last.
        if matches!(state, QueryState::Running { .. }) {
            self.drop_parked(tab, refresh);
            return;
        }

        if !routed.is_available() {
            if kept {
                profile
                    .session
                    .edit_failed(tab, "The connection is not open.".into());
            } else {
                *state = QueryState::Failed(DbError {
                    message: "The connection is not open.".into(),
                    position: None,
                });
                profile.session.reveal_results(tab);
            }
            cx.notify();
            self.drop_parked(tab, refresh);
            if let Some(lane) = lane {
                self.land_lane(tab, lane, Vec::new(), cx);
            }
            return;
        }
        // Lent only now the run is certain to start, so nothing refused above
        // leaves it lent.
        if let (Tab::Query(query), None, Some(Refresh::Earlier { index, .. })) =
            (tab, &lane, &refresh)
            && let Some(query) = profile.session.query_tab_mut(query)
        {
            query.lend(*index);
        }
        let Some((state, results)) = profile.session.slot_at(tab, lane.as_ref()) else {
            return;
        };
        // A lane's own, so a Cancel on its chip stops it and not the others.
        // A bound tab's too, because its session is shared with other tabs: a
        // Cancel stops what that session is running only while it is running
        // this tab's statement (`Connection::query_bound`).
        let cancel = match (&lane, &binding) {
            (None, None) => CancelToken::default(),
            _ => CancelToken::alongside(),
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
        // are still the ones that state describes. A kept edit send goes back
        // to it on either outcome, since `Failed` would draw the error over
        // the rows the draft is staged on.
        let restored = (keep_rows || kept).then_some(previous);
        // Here, where a Row send starts, rather than where it was asked for:
        // one the mode prompt held starts only once approved. The grid knows
        // the cells are out until the send lands, so that one put back to its
        // fetched value meanwhile stays staged, and Esc leaves the row alone.
        if let Some(Refresh::Row { row, sent, .. }) = &refresh {
            results.update(cx, |table, _| table.delegate_mut().begin_send(*row, sent));
        }
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
            // A typed `EXPLAIN`'s plan is a view of the rows about to be
            // replaced, with no Data of its own to flip back to it from.
            tab.plan.take_if(|plan| plan.typed);
        }

        // Rows from the previous statement must not sit under the one now on
        // screen -- a reader cannot tell stale rows from fresh ones. The
        // Explain command's `EXPLAIN` never reaches the grid at all, so the
        // rows already there are not the previous statement's: they are still
        // this tab's own result, and are what the user flips back to.
        let shown = {
            let table = results.read(cx);
            let (names, widths) = table.delegate().layout();
            let vertical = table.vertical_scroll_handle.0.borrow().base_handle.offset();
            (
                names,
                widths,
                vertical,
                table.horizontal_scroll_handle.offset(),
                table.delegate().column_view(),
            )
        };
        // What the batch carries, read now: on a query tab only these cells
        // are cleared on success, so an edit staged while it is out stays; a
        // relation tab's reviewed draft clears whole (see the landing). A lent
        // slot's grid is the one in front of the batch by here. Nothing can be
        // staged between the batch's writing and this read: a review holds the grid
        // against staging (`may_stage_into`), the buffer append sends in the
        // call that wrote it, and a batch the mode gate parks waits under
        // Read-only, where `set_pending` refuses. So every staged cell is in
        // the batch: a key is never NULL (`RowMatch::Key`) and only named
        // columns open, which leaves `pending_updates` no row to drop.
        let sent = carries.then(|| {
            let grid = results.read(cx).delegate();
            (grid.rows_id(), grid.pending_by_row())
        });
        // The fetched row each step of the draft updates, by step, for a kept
        // apply to hold what the server read back for it.
        let step_rows: Vec<Option<usize>> = match (&sent, &verification) {
            (Some(_), Some(_)) => results
                .read(cx)
                .delegate()
                .draft_order()
                .into_iter()
                .map(|row| match row {
                    crate::result_grid::DraftRow::Updated(row) => Some(row),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        // A Row send's one row, the cells it carries and where the ring went
        // as it was left, taken now: the landing clears those cells and no
        // edit staged since, and puts the ring back from `to` after a refusal.
        let row_sent = match &refresh {
            Some(Refresh::Row {
                row,
                rows_id,
                sent,
                to,
                ..
            }) => Some((*row, *rows_id, sent.clone(), *to)),
            _ => None,
        };
        // The action the send was for, which its landing releases.
        let row_then = match &refresh {
            Some(Refresh::Row { then, .. }) => then.clone(),
            _ => None,
        };
        // A kept send touches neither: its rows, ring, scroll and edits stay.
        if !kept && !keep_rows && explain.is_none() {
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
        } else if !kept && keep_rows {
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
            Tab::Object(object) => {
                let before_paging = (engine.spec().sql.paging
                    == crate::db::Paging::LimitOffsetByKey)
                    .then(|| self.statement_before_paging(object, &sql))
                    .flatten();
                sql::order_by(engine, &before_paging.unwrap_or_else(|| sql::unpaged(&sql)))
            }
            Tab::Query(_) => sql::order_by(engine, &sql),
        };
        let sortable = keys.is_some();
        let keys = keys.unwrap_or_default();
        // Kept only where it is read back: the query tab's grid has to be able
        // to say which statement produced it. An `EXPLAIN` produces no rows to
        // describe and belongs in nobody's history -- it is dbdelve's prefix over
        // the user's statement, and the statement itself is already there.
        // A Row send is recorded as a statement's run is, though it is a
        // relation tab's and dbdelve's: it is the user's edit, sent unreviewed,
        // and the history is where they can read what it was. Not one refused
        // before the server (`refusal`), which sent nothing.
        let statement = ((matches!(tab, Tab::Query(_)) && explain.is_none())
            || (row_sent.is_some() && refusal.is_none()))
        .then(|| sql.clone());
        // What the plan pane says it is a plan of: the user's statement, without
        // the prefix dbdelve put in front of it.
        let explained = explain
            .map(|mode| {
                let prefix = engine.explain_prefix(mode).unwrap_or_default();
                let suffix = engine.explain_suffix(mode);
                let bare = sql.strip_prefix(prefix).unwrap_or(&sql);
                bare.strip_suffix(suffix).unwrap_or(bare).to_string()
            })
            .or(typed.map(|_| sql.clone()));
        if let Tab::Query(query) = tab
            && lane.is_none()
            && let Some(tab) = self
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(query))
        {
            let ran = statement.as_ref().or(explained.as_ref());
            tab.ran_from = tab.sent_from.take().filter(|(_, sent)| Some(sent) == ran);
        }
        // Everything an object tab runs is dbdelve's, and a query tab runs
        // dbdelve's statement only when an edit is applied from its grid, the
        // one run that carries a refresh -- bar a `Return`, which the user's
        // own statement carries.
        let generated = matches!(tab, Tab::Object(_))
            || refresh
                .as_ref()
                .is_some_and(|refresh| !matches!(refresh, Refresh::Return));
        // Recorded on the way out rather than on the way back: the history is
        // what the user ran, and a statement that failed is exactly the one
        // worth getting back. Only the buffer's — a relation's preview is SQL
        // dbdelve wrote, and nobody asked to keep it. Nor an edit reviewed
        // rather than appended: recalled into a buffer, it would run without
        // the count that guards it. Its outcome is written as it lands, under
        // the same rules, since only a run recorded here has one to record.
        // Only a query tab's: a Row send is the user's edit sent unreviewed and
        // stays recorded.
        let recorded = match &statement {
            Some(statement)
                if !(generated && matches!(tab, Tab::Query(_)) && target.reviews_query_apply()) =>
            {
                self.issued_to(&id, generation).map(|profile| {
                    let entry = store::HistoryEntry::started(statement.as_str());
                    // A line that could not be written is not worth a notice
                    // on every run: the statement is still in the buffer, so
                    // nothing is lost.
                    let _ = store::append_history(&profile.id, &entry);
                    remember_statement(&mut profile.session.history, entry.clone());
                    entry
                })
            }
            _ => None,
        };
        // The user's own work, whichever profile the run belongs to: the one
        // thing besides a catalog read that keeps an idle timeout away.
        self.mark_activity(&id, cx);
        // Marked on the way out, like the history: a statement that failed
        // part way may still have opened a transaction.
        // What a lone COMMIT or ROLLBACK cleared, kept to put back when it
        // fails where the engine keeps the transaction (SQLite's
        // `SQLITE_BUSY`): it is read off what was sent, not off what the
        // server did.
        let mut ended = None;
        // Not a bound tab's: what it leaves is left on the session of its
        // database, which is not the profile's own.
        if !generated
            && binding.is_none()
            && let Some(profile) = self.issued_to(&id, generation)
        {
            let text = explained.as_deref().unwrap_or(&sql);
            let leaves = sql::leaves(engine, text);
            if leaves.transaction == Some(false)
                && profile.in_transaction
                && sql::after_last_end(engine, text).is_empty()
            {
                ended = Some(std::mem::take(&mut profile.transaction_statements));
            }
            profile.record_sent(&sql, leaves.transaction);
            if let Some(open) = leaves.transaction {
                profile.in_transaction = open;
            }
            profile.session_changed |= leaves.settings;
        }
        // The same, for the session of a bound tab's database, which a
        // transaction holds for every tab bound to it: marked on the session
        // as the statement goes (`Bound::leave`).
        let leaves_bound = (!generated && binding.is_some()).then(|| {
            let text = explained.as_deref().unwrap_or(&sql);
            let leaves = sql::leaves(engine, text);
            let open = leaves
                .transaction
                .or_else(|| sql::transaction_uncertain(engine, text).then_some(true));
            (open, leaves.settings)
        });
        // An edit dbdelve applies on the connection's own session runs inside
        // the user's open transaction and is committed or rolled back with it,
        // so the prompt that ends it lists it. Its reads stay out.
        if generated
            && edit
            && refusal.is_none()
            && let Some(profile) = self.issued_to(&id, generation)
        {
            profile.record_sent(&sql, None);
        }
        // A plan is never cut short: its rows are the plan, not data.
        let limit = match tab {
            Tab::Query(query) if explaining.is_none() => self
                .issued_to(&id, generation)
                .and_then(|profile| profile.session.query_tab(query))
                .and_then(|query| query.row_limit),
            _ => None,
        };
        // A preview whose statement has nowhere to write its limit.
        let preview_limit = match tab {
            Tab::Object(object) if !engine.pages() => self
                .issued_to(&id, generation)
                .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == object))
                .and_then(|tab| match &tab.body {
                    ObjectBody::Relation { limit, .. } => *limit,
                    ObjectBody::Routine(_)
                    | ObjectBody::Structure { .. }
                    | ObjectBody::Diagram { .. }
                    | ObjectBody::Users { .. } => None,
                }),
            Tab::Object(_) | Tab::Query(_) => None,
        };
        // Rows land in the grid as they arrive, but only in a grid cleared for
        // them: a refresh keeps the rows it is replacing until the new ones are
        // whole, and a plan's rows never reach the grid at all. A relation's
        // preview is dbdelve's statement and streams like the user's; a query
        // tab's generated run is an edit, with no rows to stream.
        let streams = matches!(tab, Tab::Object(_)) || !generated;
        // Never for a kept send, whose grid must be left alone and whose rows are
        // not a result to stream.
        let feed = (!keep_rows && !kept && streams && explaining.is_none()).then(Feed::default);
        if let Some(feed) = feed.clone() {
            let (id, results, lane) = (id.clone(), results.clone(), lane.clone());
            let (names, widths, column_view) = (shown.0.clone(), shown.1.clone(), shown.4.clone());
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
                        if running && pour(&feed, &results, mode, engine, (&names, &widths, &column_view), cx) {
                            // The first arrival reveals the stream; a later
                            // explicit hide lasts until the run completes.
                            if began.is_none()
                                && let Some(profile) = workspace.issued_to(&id, generation)
                            {
                                profile.session.reveal_results(tab);
                            }
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
        // A statement that ends a transaction goes as it is, with the binding
        // not applied first: a session inside one cannot be moved (Postgres
        // refuses inside any, DuckDB inside an aborted one, and Redis would
        // queue the move into the MULTI), so applying it first would stop the
        // COMMIT or ROLLBACK that frees the session.
        // ponytail: a bound session is auto-commit until the user's own
        // statement opens a transaction on it, which every tab bound to its
        // database shares. An edit of dbdelve's is refused while one is open
        // (`Profile::bound_write_refusal`), and the user's statements go on
        // running as typed; the upgrade is a session of the tab's own.
        let ends_transaction =
            binding.is_some() && sql::leaves(engine, &sql).transaction == Some(false);
        let bound_db = binding.as_ref().map(|binding| binding.database.clone());
        // A `USE` or `SET` in a bound tab is undone, not rewritten: the
        // binding stays what the tab was opened for, and the next run puts the
        // session back on it first. Said once the run has landed.
        let reset = binding
            .as_ref()
            .filter(|_| !generated && sql::leaves(engine, &sql).settings)
            .map(|binding| {
                format!(
                    "The next run in this tab starts again on {}.",
                    binding.label()
                )
            });
        // The verdict the mode is asked again of once a session this run opens
        // has landed, which can take as long as a connect.
        let reopened = matches!(routed, Routed::Open { .. }).then(|| sql::classify(engine, &sql));
        // What the status line says of it, once `work` has taken the statement.
        let row_statement = row_sent.as_ref().map(|_| sql.clone());
        let work = move |connection: Connection, bound: Option<Arc<Bound>>| {
            let fetch = Fetch {
                limit,
                feed: fed.as_ref(),
                reads_only,
                held: None,
                let_go: None,
            };
            // A verified Row send's row, read inside the transaction that
            // wrote it.
            let mut verified_read = None;
            // A verified apply whose `COMMIT` has no answer: the grid is
            // marked when this lands.
            let mut unknown = false;
            // A draft's rows read back after their writes, by step.
            let mut written_rows = Vec::new();
            // The rows of an ordered apply that was written before it stopped,
            // which the landing takes out of the draft.
            let mut applied = Vec::new();
            // How many statements an ordered apply wrote before it stopped,
            // ones no row names among them: a lone step's New Row has no
            // identity to take out of the draft, and its review would insert
            // it again.
            let mut written = 0;
            let mut run = || match (&verification, generated, alongside, binding.as_ref()) {
                // A reviewed draft, or a Row send, of a target that verifies an
                // apply: its rows read again and compared inside one
                // transaction, each write counted. Never `generated`, which
                // sends the text as it is. A bound tab's belongs to its
                // database whichever way the session has been moved.
                (Some(verify), _, _, binding) => {
                    let mut outcome = match binding {
                        Some(binding) => connection.apply_verified_bound(
                            binding.schema.as_deref(),
                            &verify.steps,
                            &cancel,
                        ),
                        None => connection.apply_verified(&verify.steps, &cancel),
                    };
                    if let ApplyOutcome::Applied { read_backs, .. } = &mut outcome {
                        let mut read = std::mem::take(read_backs);
                        match verify.identities.is_empty() {
                            // A Row send's one step: its row, for `land_row`.
                            true => verified_read = read.pop().map(|(_, rows)| Ok(rows)),
                            // A draft's: each row it updated as the server
                            // now holds it, for a kept apply to hold.
                            false => written_rows = read,
                        }
                    }
                    unknown = unknown_outcome(&outcome);
                    if ordered {
                        (applied, written) = written_rows_of(&outcome, &verify.identities);
                        // The write landed and only its read failed: it lands
                        // as written, with the read's own failure told by
                        // `land_row`, and not as a refusal that keeps the edits.
                        let (written, read_failed) = read_back_after_write(outcome);
                        outcome = written;
                        if let Some(error) = read_failed {
                            verified_read = Some(Err(error));
                        }
                    }
                    verified_result(outcome, verify, ordered)
                }
                // An edit applied from a bound tab's grid belongs to its
                // database whichever way the session has been moved.
                (None, true, _, Some(binding)) => connection.generated_bound(
                    binding.schema.as_deref(),
                    &sql,
                    &cancel,
                    fed.as_ref(),
                    preview_limit,
                ),
                (None, true, ..) => {
                    connection.generated(&sql, &cancel, fed.as_ref(), preview_limit)
                }
                (None, false, false, Some(binding)) if !ends_transaction => {
                    connection.query_bound(binding.schema.as_deref(), &sql, &cancel, fetch)
                }
                (None, false, false, Some(_)) => connection.query_unplaced(&sql, &cancel, fetch),
                (None, false, false, None) => connection.query(&sql, &cancel, fetch),
                (None, false, true, _) => {
                    connection.query_alongside(&sql, &cancel, fetch, read_only)
                }
            };
            let result = match (&bound, &binding, leaves_bound) {
                // Asked again under the session's turn, which a transaction
                // opened since the edit was issued has taken before it.
                (Some(bound), Some(binding), _) if refusal_applies => {
                    bound.edit(&binding.database, run)
                }
                (Some(bound), _, Some((open, settings))) => bound.leave(open, settings, run),
                _ => run(),
            };
            // A Row send's row, read inside the transaction that wrote it: the
            // one row is the whole answer.
            let read_back = verified_read;
            // A connection of the lane's own that failed is gone with it, and
            // says nothing about the profile's.
            let failed = result.is_err() || matches!(read_back, Some(Err(_)));
            let lost = !alongside && failed && connection.is_lost();
            (
                result,
                lost,
                read_back,
                unknown,
                (applied, written),
                written_rows,
            )
        };
        let number = crate::logging::connection_number(&id);
        // The database to open a session for, when none is open for it.
        let opens = match &routed {
            Routed::Open { database, .. } => Some(database.clone()),
            _ => None,
        };

        cx.spawn(async move |workspace, cx| {
            // A bound tab with no session open for its database opens one
            // first, and keeps it where the tree and a Cancel find it before
            // anything is sent on it (`Workspace::keep_session`).
            let ready = if let Some(message) = refusal {
                Err(DbError {
                    message,
                    position: None,
                })
            } else {
                match opens {
                    None => {
                        let bound = routed.bound();
                        routed.resolve().map(|(connection, _)| (connection, bound))
                    }
                    Some(database) => {
                        let opening =
                            spawn_blocking(cx.background_executor(), move || routed.resolve())
                                .await;
                        match opening {
                            Ok((session, _)) => match workspace.update(cx, |workspace, cx| {
                                let Some(session) = workspace
                                    .keep_session(&id, generation, database, read_only, session, cx)
                                else {
                                    return Err(DbError {
                                        message: Self::SESSION_OUTLIVED.into(),
                                        position: None,
                                    });
                                };
                                // Gated under the mode it was issued in, and the
                                // session it was waiting for was opened under
                                // that one: a mode picked since has not been put
                                // on it, and the statement is still unsent. A
                                // move to or from Read-only refuses it whatever
                                // it reads as: the server's hold is what keeps a
                                // read such as `SELECT write_fn()` to reading,
                                // and the session holds the old mode's.
                                let moved = workspace
                                    .issued_to(&id, generation)
                                    .map(|profile| profile.mode)
                                    .filter(|mode| {
                                        *mode != issued_under
                                            && ((*mode == Mode::ReadOnly) != read_only
                                                || reopened
                                                    .as_ref()
                                                    .is_some_and(|verdict| verdict.mode > *mode))
                                    });
                                match moved {
                                    Some(mode) => Err(DbError {
                                        message: format!(
                                            "Not sent: the connection moved to {} while the session opened.",
                                            mode.label()
                                        ),
                                        position: None,
                                    }),
                                    None => Ok((session.connection.clone(), Some(session))),
                                }
                            }) {
                                Ok(session) => session,
                                Err(_) => return,
                            },
                            Err(error) => Err(error),
                        }
                    }
                }
            };
            // Blocks for as long as the server takes, and statements queued
            // behind it on the same session block on its lock, so none of them
            // may hold one of the executor's threads (`spawn_blocking`).
            let (result, lost, read_back, unknown, (applied, written), written_rows, bound) = match ready {
                Ok((connection, bound)) => {
                    let (result, lost, read_back, unknown, applied, written_rows) =
                        spawn_blocking(cx.background_executor(), {
                            let bound = bound.clone();
                            move || work(connection, bound)
                        })
                        .await;
                    (result, lost, read_back, unknown, applied, written_rows, bound)
                }
                Err(error) => (
                    Err(error),
                    false,
                    None,
                    false,
                    (Vec::new(), 0),
                    Vec::new(),
                    None,
                ),
            };
            // Outcome and duration only. Not even at debug the server's text:
            // some engines quote the statement back in a syntax error.
            let (origin, elapsed) = (
                if generated { "dbdelve" } else { "user" },
                started.elapsed(),
            );
            let engine_label = target.label();
            match &result {
                Ok(_) => tracing::info!(
                    connection = number,
                    engine = engine_label,
                    origin,
                    ?elapsed,
                    "statement ran"
                ),
                Err(error) if db::is_cancel(target, error) => {
                    tracing::info!(
                        connection = number,
                        engine = engine_label,
                        origin,
                        ?elapsed,
                        "statement cancelled"
                    )
                }
                Err(_) => tracing::info!(
                    connection = number,
                    engine = engine_label,
                    origin,
                    ?elapsed,
                    lost,
                    "statement failed"
                ),
            }
            if lost {
                tracing::warn!(
                    connection = number,
                    engine = engine_label,
                    "connection lost"
                );
            }
            // Told before anything below asks whether the session or the tab
            // the run was for is still there: a run closed or reconnected under
            // still ran, and what it came to is history. The tab's own outcome,
            // which counts the rows it streamed, replaces this one when the
            // tab is there to count them.
            let mut unrecorded = recorded
                .as_ref()
                .map(|entry| answered(entry, &result, feed.is_some(), started.elapsed(), target));
            workspace
                .update(cx, |workspace, cx| {
                    // A landing for a profile since reconnected is not use of
                    // the new connection.
                    if workspace.issued_to(&id, generation).is_some() {
                        workspace.mark_activity(&id, cx);
                    }
                    // The plan is carried out of this block rather than stored
                    // inside it: `slot` holds the session borrowed, and the tab
                    // it belongs on has to be reached through the same session.
                    // What a Row send's landing says, and whether the relation
                    // is read again.
                    let mut row_said: Option<(String, bool)> = None;
                    // Whether an ordered apply stopped after writing some of the
                    // draft, which is then no longer the draft the review shows.
                    let mut part_way = false;
                    // A review closed while this run was out held its grid
                    // against staging until now (`Session::draft_out`).
                    if let Some(profile) =
                        workspace.profiles.iter_mut().find(|profile| profile.id == id)
                    {
                        profile.session.draft_out.retain(|out| *out != tab);
                    }
                    let (succeeded, produced_grid, plan, rest, notice, landed) = {
                        let Some(profile) = workspace.issued_to(&id, generation) else {
                            // An edit a reconnect dropped the landing of may
                            // have written: it succeeded, its `COMMIT` went
                            // unanswered, or an ordered apply wrote part of it.
                            // Its review would send the same batch again, so it
                            // goes, and the grid is marked until its rows are
                            // read. One that wrote nothing is left as it was.
                            let wrote = kept && (result.is_ok() || unknown || written > 0);
                            if let Some(profile) =
                                workspace.profiles.iter_mut().find(|profile| profile.id == id)
                            {
                                if wrote {
                                    profile.session.drop_apply_review_of(tab);
                                    if row_sent.is_none() {
                                        profile.session.tab_notice =
                                            Some((tab, Self::LANDING_DROPPED.into()));
                                    }
                                }
                                // A Row send a reconnect dropped still ends:
                                // its row is no longer out, and the action it
                                // carried goes back as a refused send's does.
                                if let Some(results) = profile.session.results(tab)
                                    && (wrote || row_sent.is_some())
                                {
                                    results.update(cx, |table, cx| {
                                        let grid = table.delegate_mut();
                                        if row_sent.is_some() {
                                            grid.end_send(wrote);
                                        }
                                        if wrote {
                                            grid.drop_applied(&applied);
                                            grid.mark_outcome_unknown();
                                            if let Some((row, rows_id, _, _)) = &row_sent
                                                && grid.rows_id() == *rows_id
                                            {
                                                grid.set_send_failure(
                                                    *row,
                                                    Self::LANDING_DROPPED.into(),
                                                );
                                            }
                                        }
                                        table.refresh(cx);
                                    });
                                }
                                if row_sent.is_some()
                                    && let Some(then) = row_then
                                {
                                    profile.session.put_back(tab, then);
                                }
                            }
                            workspace.drop_stale_run(&id, tab, lane.as_ref(), cx);
                            return;
                        };
                        // Read before `slot`, which borrows the session and not
                        // this field -- but the mode a result lands under has to
                        // be the mode at landing time, not a stale default.
                        let mode = profile.mode;
                        // A Classic view paints no row washes; a query tab
                        // answers Draft and keeps them.
                        let row_washes =
                            profile.session.edit_mode(tab) != crate::session::EditMode::Classic;
                        // Read before `slot` for the same reason. A view sorted
                        // in memory is sorted again as its rows land: a page
                        // turn or a re-run hands them back in the server's order.
                        // Never a plan's: its lines are in the only order
                        // that reads as one.
                        let client_keys = profile
                            .session
                            .sorting(tab)
                            .and_then(Sorting::client_keys)
                            .filter(|_| typed.is_none())
                            .map(<[SortKey]>::to_vec);
                        // Before `slot`: a tab closed mid-run still ran on the
                        // session that died.
                        if lost {
                            // A bound tab's session is not the profile's own:
                            // losing it takes that session out, so the next
                            // run opens another, and says nothing of the
                            // connection the profile is on.
                            match (&bound_db, &bound) {
                                (Some(database), Some(bound)) => {
                                    profile.lose_session(database, bound)
                                }
                                (Some(_), None) => {}
                                // The sessions opened for other databases end
                                // with it, as a disconnect ends them: nothing
                                // reaches them once it is gone, and one may
                                // hold a transaction's locks.
                                (None, _) => {
                                    profile.drop_sessions();
                                    profile.state = ProfileState::Failed(format!(
                                        "Connection to {} was lost.",
                                        profile.config.endpoint()
                                    ))
                                }
                            }
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
                        // Settled here, before a tab closed under the run can
                        // end the block: whether the end really ended anything
                        // is only known now.
                        let ended_ok = result.is_ok();
                        // Put back only where the engine says the failed end
                        // left the transaction open; a guess would keep a
                        // flag no COMMIT could clear, or offer a Roll Back
                        // for work the server had already rolled back.
                        if let Some(statements) = ended
                            && !lost
                            && result.as_ref().err().is_some_and(|error| {
                                engine.failed_end_keeps_transaction(&error.message)
                            })
                        {
                            profile.in_transaction = true;
                            profile.transaction_statements = statements;
                        }
                        if profile.session.auto_after_end == Some(tab) {
                            profile.session.auto_after_end = None;
                            if ended_ok {
                                profile.auto_commit = true;
                            }
                        }
                        let Some((state, results)) = profile.session.slot_at(tab, lane.as_ref())
                        else {
                            // A tab closed mid-run has no slot, but the
                            // connection's row still has to say it failed.
                            if lost && bound_db.is_none() {
                                workspace.rebuild_connections(cx);
                            }
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
                        let ran = statement.as_deref().unwrap_or_default();
                        // Read before the cost is appended to the message,
                        // which is what the engine's cancel is told by.
                        let cancel_landed = cancelled
                            && matches!(&result, Err(error) if db::is_cancel(target, error));
                        let result = match result {
                            Err(error)
                                if cancelled
                                    && reads_only
                                    && db::is_cancel(target, &error)
                                    && results.read(cx).delegate().streamed_set().is_some() =>
                            {
                                Ok(QueryResult {
                                    columns: results.read(cx).delegate().result().columns.clone(),
                                    elapsed: started.elapsed(),
                                    stopped: Some(Stopped::Cancelled),
                                    notice: db::cancel_cost(target, &error, true, ran),
                                    ..QueryResult::default()
                                })
                            }
                            Err(mut error) if cancelled && db::is_cancel(target, &error) => {
                                if let Some(cost) = db::cancel_cost(target, &error, reads_only, ran)
                                    && cost != error.message
                                {
                                    error.message = format!("{}\n\n{cost}", error.message);
                                }
                                Err(error)
                            }
                            result => result,
                        };
                        // Marked whichever way the landing goes: the draft may
                        // have landed, and the grid says so until its rows are
                        // read again.
                        if unknown {
                            results
                                .update(cx, |table, _| table.delegate_mut().mark_outcome_unknown());
                        }
                        match result {
                            // An `EXPLAIN` describes a statement rather than
                            // returning its rows, so nothing here reaches the
                            // grid: it keeps whatever the last real run put in
                            // it, which is what the Data tab flips back to.
                            Ok(mut result) if explain.is_some() => {
                                let mode = explain.unwrap_or_default();
                                *state = QueryState::Explained {
                                    elapsed: result.elapsed,
                                    mode,
                                    messages: std::mem::take(&mut result.messages),
                                };
                                let columns: Vec<String> = result
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect();
                                let plan = explain::parse(&columns, &result.rows);
                                let answer = (plan, columns, result.rows);
                                // The Explain command's run is dbdelve's
                                // prefix, never recorded.
                                (true, false, Some(answer), Vec::new(), result.notice, None)
                            }
                            // The write's own answer has no rows to show, and
                            // the grid is not replaced by it: it clears what
                            // it carried and the tab goes back to what it was.
                            Ok(mut result) if kept => {
                                let notice = result.notice.take();
                                let landed =
                                    recorded.as_ref().map(|entry| entry.succeeded(&result));
                                if let Some(previous) = restored {
                                    *state = previous;
                                }
                                if let Some((row, rows_id, cells, _)) = &row_sent {
                                    let sent = row_statement.as_deref().unwrap_or_default();
                                    let status = match result.rows_affected {
                                        Some(1) => format!("1 row updated: {sent}"),
                                        Some(count) => format!("{count} rows updated: {sent}"),
                                        None => format!("Sent: {sent}"),
                                    };
                                    row_said = Some(results.update(cx, |table, cx| {
                                        let said = land_row(
                                            table.delegate_mut(),
                                            (*row, *rows_id, cells),
                                            read_back,
                                            status,
                                        );
                                        table.refresh(cx);
                                        said
                                    }));
                                }
                                if let Some((rows_id, rows)) = &sent {
                                    results.update(cx, |table, cx| {
                                        let grid = table.delegate_mut();
                                        match tab {
                                            // A reviewed relation tab's apply
                                            // sent the whole draft, cells and
                                            // added rows: the review held the
                                            // grid against staging while it was
                                            // out (`may_stage_into`), and a
                                            // sort in memory only permuted what
                                            // it held. So the whole draft goes,
                                            // whatever `rows_id` now is, and an
                                            // added row is not left staged to
                                            // be sent twice.
                                            Tab::Object(_) => grid.discard_pending(),
                                            // The sent cells are matched by row
                                            // index and value under an
                                            // unchanged `rows_id`, which holds:
                                            // a sort in memory waits for the
                                            // run (`set_view_sorting`).
                                            // Each holds what the server read
                                            // back for it inside the apply, so
                                            // a value it stored otherwise, or a
                                            // DEFAULT, is what the next apply's
                                            // re-read is compared with. One
                                            // with no read-back holds what was
                                            // sent.
                                            Tab::Query(_) => {
                                                if grid.rows_id() == *rows_id {
                                                    for (row, cells) in rows {
                                                        grid.clear_sent(*row, cells);
                                                        let placed = written_rows
                                                            .iter()
                                                            .find(|(step, _)| {
                                                                step_rows.get(*step)
                                                                    == Some(&Some(*row))
                                                            })
                                                            .is_some_and(|(_, read)| {
                                                                grid.place_read_back(
                                                                    *row, read, cells,
                                                                )
                                                            });
                                                        if !placed {
                                                            grid.write_sent(*row, cells);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        table.refresh(cx);
                                    });
                                }
                                (true, false, None, Vec::new(), notice, landed)
                            }
                            Ok(mut result) => {
                                // Rows that streamed in were scrolled through as
                                // they came, so the grid stays where the user
                                // took it rather than where the last run left it.
                                let streamed = results.read(cx).delegate().streamed_set().is_some();
                                if let Some(feed) = &feed {
                                    let (names, widths, column_view) =
                                        (&shown.0, &shown.1, &shown.4);
                                    pour(
                                        feed,
                                        &results,
                                        mode,
                                        engine,
                                        (names, widths, column_view),
                                        cx,
                                    );
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
                                        result.bytes = result.rows.iter().map(Row::text_len).sum();
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
                                if typed.is_some()
                                    || statement.as_deref().is_some_and(|statement| {
                                        !sql::rerunnable(engine, statement)
                                    })
                                {
                                    result.edit = None;
                                }
                                // A typed `EXPLAIN`'s rows land as any result's
                                // do, for copying and export, and are drawn as
                                // the plan they spell.
                                let plan = typed.map(|_| {
                                    let columns: Vec<String> = result
                                        .columns
                                        .iter()
                                        .map(|column| column.name.clone())
                                        .collect();
                                    (
                                        explain::parse(&columns, &result.rows),
                                        Vec::new(),
                                        Vec::new(),
                                    )
                                });
                                let landed =
                                    recorded.as_ref().map(|entry| entry.succeeded(&result));
                                *state = QueryState::Complete {
                                    rows: result.total_rows(),
                                    bytes: result.bytes,
                                    elapsed: result.elapsed,
                                    rows_affected: result.rows_affected,
                                    messages: std::mem::take(&mut result.messages),
                                };
                                let produced_grid = !result.columns.is_empty();
                                // Sorted in memory as it lands, the rows move
                                // out from under any index into them.
                                let reordered =
                                    client_keys.as_deref().is_some_and(|keys| !keys.is_empty());
                                results.update(cx, |table, cx| {
                                    let selection = table.delegate().selection();
                                    let sort = sort_columns(engine, &keys, &result.columns);
                                    let (names, widths, vertical, horizontal, column_view) = &shown;
                                    let column_view = carried_view(table.delegate(), column_view);
                                    let (carried_names, carried_widths) =
                                        carried_layout(table.delegate(), names, widths);
                                    *table.delegate_mut() = ResultGrid::new(result, mode)
                                        .with_target(target)
                                        .with_sort(sort, sortable)
                                        .with_client_sort(client_keys.as_deref())
                                        .with_layout(&carried_names, &carried_widths)
                                        .with_column_view(&column_view);
                                    table.delegate_mut().set_row_washes(row_washes);
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
                                (true, produced_grid, plan, rest, notice, landed)
                            }
                            // Rows and edits stay and the tab is as it was; the
                            // message is said where the statement is.
                            Err(error) if kept => {
                                let landed = recorded.as_ref().map(|entry| {
                                    entry.failed(&error, started.elapsed(), cancel_landed)
                                });
                                if let Some(previous) = restored {
                                    *state = previous;
                                }
                                // A refused row is left dirty and marked, and the
                                // tab's controls ask before they leave it, so what
                                // is said says what gets it back.
                                let said = match row_sent.is_some() {
                                    true => format!(
                                        "{}{} {}",
                                        error.message.trim_end(),
                                        match error.message.trim_end().ends_with(['.', '!', '?']) {
                                            true => "",
                                            false => ".",
                                        },
                                        Self::ROW_KEPT
                                    ),
                                    false => error.message,
                                };
                                // What an ordered apply wrote before it stopped
                                // is on the server and not to be sent again, so
                                // it leaves the draft, by what names it: the rows
                                // may have been sorted since the apply began.
                                if written > 0 {
                                    part_way = true;
                                    results.update(cx, |table, cx| {
                                        let grid = table.delegate_mut();
                                        grid.drop_applied(&applied);
                                        // A write no row names stays staged
                                        // as if it had not been sent.
                                        if applied.len() < written {
                                            grid.mark_outcome_unknown();
                                        }
                                        table.refresh(cx);
                                    });
                                }
                                // The row says what the server said to it, and
                                // keeps its edits.
                                if let Some((row, rows_id, _, _)) = &row_sent {
                                    results.update(cx, |table, cx| {
                                        let grid = table.delegate_mut();
                                        grid.end_send(false);
                                        if grid.rows_id() == *rows_id {
                                            grid.set_send_failure(*row, said.clone());
                                        }
                                        cx.notify();
                                    });
                                }
                                (false, false, None, Vec::new(), Some(said), landed)
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
                                let landed = recorded.as_ref().map(|entry| {
                                    entry.failed(&error, started.elapsed(), cancel_landed)
                                });
                                // Into the user's statement, not the prefix
                                // dbdelve put in front of it, which is on no
                                // screen to count from.
                                let prefix = explain
                                    .and_then(|mode| engine.explain_prefix(mode))
                                    .map_or(0, str::len);
                                error.position = error
                                    .position
                                    .and_then(|position| position.checked_sub(prefix));
                                // The Users tab draws no result pane, so a
                                // role change the server refused says why in
                                // the status bar. A cancel was the user's own.
                                let refused = (!cancelled
                                    && matches!(refresh, Some(Refresh::Roles)))
                                .then(|| error.message.clone());
                                *state = match restored {
                                    Some(previous) if cancelled => previous,
                                    _ => QueryState::Failed(error),
                                };
                                (false, false, None, Vec::new(), refused, landed)
                            }
                        }
                    };

                    if let Some(landed) = landed {
                        unrecorded = None;
                        let _ = store::append_history(&id, &landed);
                        workspace.remember_run(&id, landed);
                    }
                    // Hidden for statements that answer with nothing to read,
                    // the pane comes back for one that does, queued lanes
                    // included: what lands must never be on screen unseen.
                    if let Some(profile) = workspace.issued_to(&id, generation)
                        && profile
                            .session
                            .slot_at(tab, lane.as_ref())
                            .is_some_and(|(state, _)| reveals_results(state, produced_grid))
                    {
                        profile.session.reveal_results(tab);
                    }
                    if keep_rows
                        && let Some(profile) = workspace.issued_to(&id, generation)
                        && profile.session.notice.as_deref() == Some(Self::REFRESHING)
                    {
                        profile.session.notice = None;
                    }
                    if let Some(notice) = notice
                        && let Some(profile) = workspace.issued_to(&id, generation)
                    {
                        // A kept send that failed says so where its statement
                        // is, which is the review when one covers the tab.
                        if kept && !succeeded && part_way {
                            // The review's statements include the ones that
                            // applied, and Run would send them again: the draft
                            // that is left is written afresh by Apply.
                            profile.session.drop_apply_review_of(tab);
                            profile.session.tab_notice = Some((tab, notice));
                        } else if kept && !succeeded {
                            profile.session.edit_failed(tab, notice);
                        } else {
                            profile.session.tab_notice = Some((tab, notice));
                        }
                    }
                    let in_front_now = workspace.profile().is_some_and(|profile| {
                        profile.id == id && profile.generation == generation
                    });
                    // A refused or cancelled row send leaves the row dirty, so the
                    // ring the user moved off it goes back, unless they have
                    // moved it on since (`snap_back_to`).
                    if !succeeded
                        && in_front_now
                        && let Some((row, _, _, to)) = &row_sent
                    {
                        workspace.snap_back_to(tab, *row, *to, cx);
                    }
                    // Released only by a write that landed; every other
                    // outcome cancels it, with the tab as it was. An action
                    // already waiting is the user's own, and this one only a
                    // re-read's.
                    // Whichever profile is in front: a send that lands behind
                    // another connection's tab still owes its own bars their
                    // way back, and the action does not follow the user.
                    if let Some(then) = row_then
                        && let Some(profile) = workspace.issued_to(&id, generation)
                    {
                        match in_front_now && row_said.is_some() {
                            true => profile.session.park_resume(tab, then),
                            false => profile.session.put_back(tab, then),
                        }
                    }
                    if let Some((mut said, reread)) = row_said {
                        // The relation is read again first: that clears the
                        // notices, and the last send is what the status line
                        // is to show. Not over another row's edits: the read
                        // would send them, and nothing is sent without the
                        // user's act. Those rows are kept and the tab is owed
                        // the read, which its next refresh or visit does.
                        if reread && let Tab::Object(object) = tab {
                            if in_front_now && !workspace.has_unapplied_edits(tab, cx) {
                                workspace.refresh_relation(object, cx);
                            } else {
                                if in_front_now {
                                    said = format!("{said} {}", Self::NOT_READ_AGAIN);
                                }
                                if let Some(profile) = workspace.issued_to(&id, generation) {
                                    profile.session.mark_stale(object);
                                }
                            }
                        }
                        if let Some(profile) = workspace.issued_to(&id, generation) {
                            profile.session.notice = None;
                            profile.session.tab_notice = Some((tab, said));
                        }
                    }
                    if let Some(reset) = reset
                        && let Some(profile) = workspace.issued_to(&id, generation)
                    {
                        profile.session.tab_notice = Some((tab, reset));
                    }
                    if succeeded && let Some(profile) = workspace.issued_to(&id, generation) {
                        // A statement that returned no columns produced no grid,
                        // so it is not the statement to go back to — which is
                        // what keeps an applied UPDATE from becoming the query
                        // an apply re-runs.
                        // A lane's is set once the last of the queue lands,
                        // since the order they finish in is not the buffer's.
                        // Nor is a run that carries a refresh: a generated
                        // write, whose reply can have columns (a MongoDB
                        // `insertOne`'s does) and would be run again by the
                        // next apply, or a re-read of an earlier result, which
                        // is that result's statement and not the slot's.
                        if produced_grid
                            && lane.is_none()
                            && refresh.is_none()
                            && let Some(statement) = statement
                            && let Tab::Query(query) = tab
                            && let Some(tab) = profile.session.query_tab_mut(query)
                        {
                            // Never a typed `EXPLAIN`: a sort or refresh
                            // re-runs `last_query`, and `ANALYZE` runs the
                            // statement it explains.
                            tab.last_query = typed.is_none().then_some(statement);
                        }
                        // Shown as soon as it lands: asking for a plan is asking
                        // to read one, so the pane turns to it rather than
                        // leaving the answer behind a tab the user has to find.
                        if let Some((plan, columns, rows)) = plan
                            && let Some(mode) = explaining
                            && let Tab::Query(query) = tab
                            && let Some(tab) = profile.session.query_tab_mut(query)
                        {
                            tab.plan = Some(Explained {
                                plan,
                                mode,
                                sql: explained.unwrap_or_default(),
                                typed: typed.is_some(),
                                columns,
                                rows,
                            });
                            tab.showing_plan = true;
                            // The form and review were for another result.
                            if tab.show_slot() {
                                profile
                                    .session
                                    .insert_form
                                    .take_if(|form| form.tab == Tab::Query(query));
                                profile.session.drop_apply_review_of(Tab::Query(query));
                            }
                        }
                        // Nothing left to read once the batch it was showing has
                        // run. Only this tab's: another tab's is still unread.
                        if refresh.is_some() {
                            profile.session.drop_apply_review_of(tab);
                        }
                    }
                    // The other side of the pair in `load_structure`: a run
                    // replaces the whole delegate, so a fresh grid has to be
                    // marked again from the structure the tab already holds.
                    if succeeded && let Tab::Object(object) = tab {
                        workspace.mark_columns(object, cx);
                    }
                    if lost && bound_db.is_none() {
                        // The connection's row says it failed.
                        workspace.rebuild_connections(cx);
                    }
                    cx.notify();

                    let had_refresh = refresh.is_some();
                    // The lent result's chain ends with its re-read, or with
                    // the batch when that failed, whose error is the result's.
                    let ends_loan = match &refresh {
                        Some(Refresh::Return) => true,
                        Some(Refresh::Earlier { .. }) => !succeeded,
                        _ => false,
                    };
                    if ends_loan {
                        workspace.give_back(&id, generation, tab);
                    }
                    // A follow-up runs on the profile in front, whose tab ids
                    // count from zero like every other's: after a switch it
                    // would land in another connection's tab of the same id.
                    let in_front = workspace.profile().is_some_and(|profile| {
                        profile.id == id && profile.generation == generation
                    });
                    // Edits still staged once a kept send has cleared what it
                    // carried: an Insert or Delete row carries none, and one
                    // staged while the batch was out is not in it. Reading the
                    // rows again would drop them unseen.
                    let edits_left = kept
                        && succeeded
                        && row_sent.is_none()
                        && in_front
                        && workspace
                            .issued_to(&id, generation)
                            .and_then(|profile| profile.session.slot(tab))
                            .is_some_and(|(_, results)| results.read(cx).delegate().has_pending());
                    if edits_left {
                        if matches!(refresh, Some(Refresh::Earlier { .. })) {
                            workspace.give_back(&id, generation, tab);
                        }
                        if let Some(profile) = workspace.issued_to(&id, generation) {
                            profile.session.tab_notice = Some((tab, Self::EDITS_KEPT.into()));
                        }
                    } else if succeeded
                        && !in_front
                        && let Some(
                            Refresh::Statement { .. }
                            | Refresh::Earlier { .. }
                            | Refresh::Relation { .. },
                        ) = &refresh
                    {
                        workspace.give_back(&id, generation, tab);
                        if let Some(profile) = workspace.issued_to(&id, generation) {
                            profile.session.notice = Some(
                                "The change was applied. Run the statement again to see it.".into(),
                            );
                        }
                    } else if succeeded && let Some(refresh) = refresh {
                        match refresh {
                            Refresh::Statement { sql, .. } => workspace.execute_sql(sql, tab, cx),
                            Refresh::Earlier { sql, .. } => {
                                workspace.execute_and_then(
                                    sql,
                                    tab,
                                    Some(Refresh::Return),
                                    false,
                                    None,
                                    cx,
                                );
                                // Parked by the gate or refused, the re-read
                                // is not coming back to end the loan.
                                if !workspace
                                    .issued_to(&id, generation)
                                    .is_some_and(|profile| profile.session.running(tab))
                                {
                                    workspace.give_back(&id, generation, tab);
                                }
                            }
                            Refresh::Return => {}
                            // Landed where it stands, above.
                            Refresh::Row { .. } => {}
                            Refresh::Relation { id, .. } => workspace.refresh_relation(id, cx),
                            Refresh::Roles => workspace.load_roles(&id, cx),
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
                        None if !had_refresh => {
                            workspace.advance_queue(&id, generation, tab, rest, cx)
                        }
                        None => {}
                    }
                })
                .ok();
            if let Some(entry) = unrecorded {
                let _ = store::append_history(&id, &entry);
                workspace
                    .update(cx, |workspace, _| workspace.remember_run(&id, entry))
                    .ok();
            }
        })
        .detach();
    }

    /// Said after the server refused a Row send, after what it said.
    const ROW_KEPT: &str = "The row keeps its edits; Esc puts it back as it was read.";

    /// Said where an edit's landing a reconnect dropped would have been: what
    /// it wrote is not on screen, so nothing more is sent from the grid until
    /// its rows are read again.
    const LANDING_DROPPED: &str = "The connection was refreshed before this edit's answer came \
                                   back. Read the rows again to see what the server holds.";

    /// Said after a Row send that left the relation owed a read, over a tab
    /// that still holds another row's edits.
    const NOT_READ_AGAIN: &str = "The rows were not read again: this tab has unapplied edits.";

    /// Said after an edit send succeeded over a grid that still holds edits,
    /// which the re-read would have dropped unseen.
    pub(super) const EDITS_KEPT: &str =
        "The change was applied. The rows were not read again: this tab has unapplied edits.";

    /// A run's outcome into the history of the profile that sent it, by id
    /// alone: a reconnect retires the session's generation, not its history.
    pub(crate) fn remember_run(&mut self, profile_id: &str, entry: store::HistoryEntry) {
        if let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        {
            remember_statement(&mut profile.session.history, entry);
        }
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
    ///
    /// A queue's earlier result lent the slot for the run gets it back, its
    /// own state left as it was: an abandoned run says nothing about it.
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
        if let Tab::Query(query) = tab
            && let Some(query) = profile.session.query_tab_mut(query)
        {
            query.give_back();
        }
    }

    /// End a queue's earlier result's loan of the slot on the profile the run
    /// was issued to.
    fn give_back(&mut self, id: &str, generation: u64, tab: Tab) {
        if let Tab::Query(query) = tab
            && let Some(query) = self
                .issued_to(id, generation)
                .and_then(|profile| profile.session.query_tab_mut(query))
        {
            query.give_back();
        }
    }

    /// Rewrite the selection as formatted SQL, or the whole buffer when
    /// nothing is selected. Invoked by hand only -- running, saving and
    /// leaving the buffer all leave what was typed alone.
    pub(crate) fn format_query(
        &mut self,
        _: &FormatQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_covers() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let editor = tab.editor.clone();
        let (text, selection, cursor) = {
            let editor = editor.read(cx);
            (
                editor.value().to_string(),
                editor.selected_range(),
                editor.cursor(),
            )
        };
        let whole = selection.is_empty();
        let span = match whole {
            true => 0..text.len(),
            false => selection,
        };
        let source = &text[span.clone()];
        if source.trim().is_empty() {
            return;
        }

        // Said out loud rather than left as a no-op: a Format that appears to
        // do nothing reads as a broken Format, not as a deliberate refusal.
        let formatted = match whole {
            true => crate::sql::format(self.engine(), source),
            false => crate::sql::format_selection(self.engine(), &text, span.clone()),
        };
        let formatted = match formatted {
            Ok(formatted) => formatted,
            Err(refusal) => {
                self.note(refusal.into(), cx);
                return;
            }
        };
        if formatted == source {
            return;
        }

        if !whole {
            let after = span.start..span.start + formatted.len();
            editor.update(cx, |editor, cx| {
                replace_range(editor, span, &formatted, window, cx);
                editor.set_selected_range(after, cx);
            });
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
            rewrite(editor, &formatted, window, cx);
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
            // other cursors are dropped -- gpui-base 0.8 exposes no way to
            // read them all. Undo reselects the whole lines, not the caret, for
            // the same lack of API.
            //
            replace_range(editor, lines, &toggled, window, cx);
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

/// Replace `range` of the buffer with `text` as one step Undo takes back,
/// leaving the cursor after it. Through `replace`, not `set_value`: that one
/// wipes the undo stack.
pub(crate) fn replace_range(
    editor: &mut EditorState,
    range: Range<usize>,
    text: &str,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    // An IME composition in progress would otherwise be what `replace`
    // overwrites, ahead of the selection.
    gpui::EntityInputHandler::unmark_text(editor, window, cx);
    editor.set_selected_range(range, cx);
    editor.replace(text.to_string(), window, cx);
}

/// Make the buffer read `text` as one step Undo takes back, replacing only the
/// span between what the two share at either end, so Undo reselects what
/// changed rather than the whole buffer.
pub(crate) fn rewrite(
    editor: &mut EditorState,
    text: &str,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let old = editor.value().to_string();
    let (changed, with) = changed_span(&old, text);
    if changed.is_empty() && with.is_empty() {
        return;
    }
    replace_range(editor, changed, &text[with], window, cx);
}

/// The span of `old` that differs from `new`, and the span of `new` that
/// replaces it, both on char boundaries.
fn changed_span(old: &str, new: &str) -> (Range<usize>, Range<usize>) {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    (prefix..old.len() - suffix, prefix..new.len() - suffix)
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

/// What the next grid keeps of the hidden and pinned columns: the grid's
/// own, which a column hidden while rows were still arriving has changed
/// since the run began, unless the run already cleared it to nothing.
fn carried_view(grid: &ResultGrid, at_start: &ColumnView) -> ColumnView {
    match grid.columns().is_empty() {
        true => at_start.clone(),
        false => grid.column_view(),
    }
}

/// The widths the finished grid starts from: the streamed grid's, which hold
/// any column dragged while the rows were arriving, over the ones taken when
/// the run began.
fn carried_layout(
    grid: &ResultGrid,
    names: &[String],
    widths: &[ColumnWidth],
) -> (Vec<String>, Vec<ColumnWidth>) {
    match grid.columns().is_empty() {
        true => (names.to_vec(), widths.to_vec()),
        false => grid.layout(),
    }
}

/// Move what has arrived into the grid. A new result set starts the grid over,
/// as it would have when the set's statement finished. `true` when anything
/// on screen changed.
fn pour(
    feed: &Feed,
    results: &Entity<TableState<ResultGrid>>,
    mode: Mode,
    engine: Engine,
    (names, widths, column_view): (&[String], &[ColumnWidth], &ColumnView),
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
            let column_view = carried_view(table.delegate(), column_view);
            *table.delegate_mut() = ResultGrid::new(columns, mode)
                .with_engine(engine)
                .with_layout(names, widths)
                .with_column_view(&column_view)
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

/// [`Workspace::has_markable_row`] over what it reads: a Draft view of a
/// relation's rows on a target whose edits stay in the grid until the server
/// answers, whose selection holds an added row or a row named by its key.
fn markable_row(target: Target, tab: Tab, mode: EditMode, grid: &ResultGrid) -> bool {
    matches!(tab, Tab::Object(_))
        && mode == EditMode::Draft
        && target.keeps_edits()
        && grid.insert_target().is_some()
        && grid
            .selected_draft_rows()
            .into_iter()
            .any(|row| row >= grid.fetched_rows() || grid.row_key(row).is_some())
}

/// What a verified apply's outcome is to the run that sent it: the write's own
/// answer, or the error the landing shows where the statement is.
fn verified_result(
    outcome: ApplyOutcome,
    verify: &Verification,
    ordered: bool,
) -> Result<QueryResult, DbError> {
    match outcome {
        ApplyOutcome::Applied { affected, .. } => Ok(QueryResult {
            rows_affected: Some(affected),
            ..QueryResult::default()
        }),
        // An ordered target's failure after its first write is told with what
        // applied (`verified_message`); every other is the driver's own words.
        ApplyOutcome::Failed { stage, error }
            if !(ordered
                && matches!(
                    stage,
                    ApplyStage::Write(_) | ApplyStage::Unanswered(_) | ApplyStage::Concern(_)
                )) =>
        {
            Err(error)
        }
        refused => Err(DbError {
            message: verified_message(&refused, verify, ordered).unwrap_or_default(),
            position: None,
        }),
    }
}

/// Whether an edit sent to `target` leaves the grid and its draft in place
/// until the server answers. A transactional engine's failure rolls the whole
/// batch back (`Target::row_edits`). An ordered target's (MongoDB's) failure
/// leaves the statements before it applied, and only a `verified` apply says
/// which, for the landing to take them out of the draft: a query tab's
/// unverified batch there is not kept, or its next Apply would send again
/// what already landed.
fn keeps_draft(target: Target, verified: bool) -> bool {
    target.row_edits() || (target.ordered_edits() && verified)
}

/// Whether nobody can know what `outcome` left on the server: a `COMMIT` with
/// no answer, or on an ordered target, which has no transaction to undo it, a
/// write nothing answered, which may have reached the server before its
/// answer was lost (`ApplyStage::Unanswered`). Kept in the draft, that write
/// would otherwise go again on the next Apply, and an insert with no `_id` of
/// its own would land twice. A write the server refused applied nothing, and
/// its draft may be applied again once corrected.
fn unknown_outcome(outcome: &ApplyOutcome) -> bool {
    matches!(
        outcome,
        ApplyOutcome::Unknown(_)
            | ApplyOutcome::Failed {
                stage: ApplyStage::Unanswered(_),
                ..
            }
    )
}

/// How many statements of an ordered apply were written when it stopped, the
/// ones before the step it names: a statement the server refused, or counted
/// other than one row. Nothing was sent after it. A drift and a failure
/// before the first write wrote nothing.
///
/// A read-back that failed follows its step's write, so that step counts too:
/// only a Row send's step carries one, and the draft's own steps never do. So
/// does a write whose write concern failed, which applied. One nothing
/// answered is not counted: whether it applied is not known.
fn written_before_stop(outcome: &ApplyOutcome) -> usize {
    match outcome {
        ApplyOutcome::Miscounted { step, .. }
        | ApplyOutcome::Failed {
            stage: ApplyStage::Write(step) | ApplyStage::Unanswered(step),
            ..
        } => *step,
        ApplyOutcome::Failed {
            stage: ApplyStage::ReadBack(step) | ApplyStage::Concern(step),
            ..
        } => step + 1,
        _ => 0,
    }
}

/// The rows an ordered apply wrote before it stopped, by what names them, and
/// how many statements it wrote: more than the rows when a step names none, as
/// a Row send's or a lone New Row's does, which is then still staged.
fn written_rows_of(outcome: &ApplyOutcome, identities: &[StepRow]) -> (Vec<StepRow>, usize) {
    let written = written_before_stop(outcome);
    let rows = identities
        .get(..written)
        .map(<[StepRow]>::to_vec)
        .unwrap_or_default();
    (rows, written)
}

/// An ordered target's failed read-back as what it is: every statement up to
/// and including its step was written, there being no transaction to take them
/// back, and the read is what failed. The outcome is then that of the writes,
/// applied, and the error is the read's, for the landing to tell
/// ([`land_row`]); any other outcome is returned as it is.
fn read_back_after_write(outcome: ApplyOutcome) -> (ApplyOutcome, Option<DbError>) {
    match outcome {
        ApplyOutcome::Failed {
            stage: ApplyStage::ReadBack(step),
            error,
        } => (
            ApplyOutcome::Applied {
                affected: step as u64 + 1,
                read_backs: Vec::new(),
            },
            Some(error),
        ),
        other => (other, None),
    }
}

/// What an ordered apply that stopped at step `stopped` says: what applied and
/// has left the draft, the sentence for the statement it stopped at, and the
/// statements after it that were never sent.
fn stopped_part_way(verify: &Verification, stopped: usize, sentence: String) -> String {
    told_part_way(verify, (stopped, Some(stopped)), sentence, None)
}

/// [`stopped_part_way`]'s sentences for `(applied, stopped)`: the first
/// `applied` statements applied and have left the draft, the one at index
/// `stopped` is what `sentence` says, and the rest were not sent. A
/// `stopped` of `None` says nothing of how many applied, for a first write
/// whose own outcome is not known. `after` closes it.
fn told_part_way(
    verify: &Verification,
    (applied, stopped): (usize, Option<usize>),
    sentence: String,
    after: Option<&str>,
) -> String {
    let total = verify.steps.len();
    let applied = match (applied, stopped) {
        (0, None) => None,
        (0, _) => Some("Nothing was applied.".to_string()),
        (1, _) => Some("Statement 1 was applied and has left the draft.".to_string()),
        (written, _) => Some(format!(
            "Statements 1 to {written} were applied and have left the draft."
        )),
    };
    let last = stopped.unwrap_or(0);
    let unsent = match total.saturating_sub(last + 1) {
        0 => None,
        1 => Some(format!("Statement {total} was not sent.")),
        _ => Some(format!("Statements {} to {total} were not sent.", last + 2)),
    };
    [applied, Some(sentence), unsent, after.map(str::to_string)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `message` without trailing space, and the full stop it lacks, if any.
fn closed(message: &str) -> (&str, &'static str) {
    let said = message.trim_end();
    match said.ends_with(['.', '!', '?']) {
        true => (said, ""),
        false => (said, "."),
    }
}

/// What the review says of a verified apply that wrote nothing, naming the
/// rows: `Nothing was applied.` and then one sentence for each row that is not
/// the row that was read. A `COMMIT` whose answer was lost is said as that
/// alone, since nothing is known of what it did. `None` for an outcome that
/// carries no such sentence, the write that landed and the errors a driver
/// words itself.
///
/// For an `ordered` target, which has no transaction to roll back, a statement
/// that failed or counted wrongly after others had been written says which
/// were applied, which one stopped it and which were not sent
/// ([`stopped_part_way`]); a drift wrote nothing and keeps the sentences above.
fn verified_message(
    outcome: &ApplyOutcome,
    verify: &Verification,
    ordered: bool,
) -> Option<String> {
    let row = |step: usize| verify.rows.get(step).map_or("?", String::as_str);
    // An added row is named as it is, an existing one by its key.
    let named = |step: usize| match verify.steps.get(step).map(|step| step.kind) {
        Some(StepKind::Insert) => row(step).to_string(),
        Some(StepKind::Update | StepKind::Delete) | None => format!("row {}", row(step)),
    };
    let sentences: Vec<String> = match outcome {
        ApplyOutcome::Drifted(drifted) => drifted
            .iter()
            .map(|(step, drift)| match drift {
                Drift::Changed => {
                    format!("Row {} does not match the row as it was read.", row(*step))
                }
                Drift::Gone => format!("Row {} is no longer on the server.", row(*step)),
                Drift::Ambiguous(rows) => {
                    // The re-read stops at two, which is all it takes.
                    let _ = rows;
                    format!(
                        "Row {} matches more than one row on the server.",
                        row(*step)
                    )
                }
            })
            .collect(),
        ApplyOutcome::Miscounted { step, count } => {
            let what = match count {
                Some(count) => format!("affected {count} rows"),
                None => "did not say how many rows it affected".to_string(),
            };
            let sentence = format!(
                "{} {} ({}) {what}.",
                verify
                    .steps
                    .get(*step)
                    .map_or("Statement", |step| step.kind.keyword()),
                step + 1,
                named(*step)
            );
            if ordered {
                return Some(stopped_part_way(verify, *step, sentence));
            }
            vec![sentence]
        }
        ApplyOutcome::Failed {
            stage: ApplyStage::Write(step),
            error,
        } if ordered => {
            let (said, stop) = closed(&error.message);
            return Some(stopped_part_way(
                verify,
                *step,
                format!(
                    "Statement {} ({}) failed: {said}{stop}",
                    step + 1,
                    named(*step)
                ),
            ));
        }
        // Nothing answered for this write, so it is neither among the
        // statements that applied nor among those never sent.
        ApplyOutcome::Failed {
            stage: ApplyStage::Unanswered(step),
            error,
        } if ordered => {
            let (said, stop) = closed(&error.message);
            return Some(told_part_way(
                verify,
                (*step, (*step > 0).then_some(*step)),
                format!(
                    "Statement {} ({}) got no answer, so it may have been applied: {said}{stop}",
                    step + 1,
                    named(*step)
                ),
                Some("Read the rows again to see what the server holds."),
            ));
        }
        // It applied, and has left the draft with the ones before it.
        ApplyOutcome::Failed {
            stage: ApplyStage::Concern(step),
            error,
        } if ordered => {
            let (said, stop) = closed(&error.message);
            return Some(told_part_way(
                verify,
                (step + 1, Some(*step)),
                format!("Statement {} ({}): {said}{stop}", step + 1, named(*step)),
                None,
            ));
        }
        ApplyOutcome::Unknown(_) => {
            return Some(
                "The connection was lost during COMMIT; whether the changes were applied is not \
                 known."
                    .into(),
            );
        }
        ApplyOutcome::Applied { .. } | ApplyOutcome::Failed { .. } => return None,
    };
    Some(format!("Nothing was applied. {}", sentences.join(" ")))
}

/// What a Row send's landing does to the grid, and what it says. The row the
/// server now holds is put where the row stood and the cells that were sent
/// are cleared, so an edit staged while it was out stays. Anything else clears
/// the cells that were sent all the same, keeps the edits staged since, and
/// leaves the grid's row to a read of the whole relation, saying what
/// happened: the second of the pair is whether that read is owed. The caller
/// holds the read back while edits remain, since it would drop them.
///
/// An index into rows that were reordered while the send was out names
/// another row, so nothing is placed or cleared by it then. A row named by
/// every value is placed as well, when its read holds exactly the values it
/// was sent: the row's next send names it by them. Reading the whole relation
/// instead would drop an input open on another row, and with other rows'
/// edits staged it is held back, leaving the row named by values it no longer
/// has. A read holding others may be another row that already held the sent
/// values, so the relation is read instead ([`ResultGrid::place_read_back`]).
fn land_row(
    grid: &mut ResultGrid,
    (row, rows_id, sent): (usize, u64, &[(usize, NewValue)]),
    read: Option<Result<QueryResult, DbError>>,
    status: String,
) -> (String, bool) {
    grid.end_send(true);
    if grid.rows_id() != rows_id {
        return (
            "The row was updated. The rows were reordered while it was saved, so it was not \
             read back."
                .into(),
            false,
        );
    }
    match read {
        Some(Ok(read)) if read.rows.len() == 1 => {
            let placed = grid.place_read_back(row, &read, sent);
            grid.clear_sent(row, sent);
            (status, !placed)
        }
        Some(Ok(read)) if read.rows.is_empty() => {
            grid.clear_sent(row, sent);
            ("The row was not found after the update.".into(), true)
        }
        Some(Ok(_)) | None => {
            grid.clear_sent(row, sent);
            (status, true)
        }
        Some(Err(error)) => {
            grid.clear_sent(row, sent);
            (
                format!(
                    "The row was updated. Reading it back failed: {}",
                    error.message
                ),
                true,
            )
        }
    }
}

/// What a run came to, told from its answer alone, for a run whose tab or
/// session went while it ran. Rows that streamed went to a grid nobody reads
/// any more, so a streamed result's count is left unknown rather than
/// undercounted, unless the row limit already said how many there were. With
/// the tab gone there is no state to say a Cancel was asked for, so the
/// engine's own cancel wording is what tells one.
pub(super) fn answered(
    entry: &store::HistoryEntry,
    result: &Result<QueryResult, DbError>,
    streamed: bool,
    elapsed: std::time::Duration,
    target: Target,
) -> store::HistoryEntry {
    match result {
        Ok(result) => {
            let mut landed = entry.succeeded(result);
            if streamed && result.capped_from.is_none() {
                landed.rows = None;
            }
            landed
        }
        Err(error) => entry.failed(error, elapsed, db::is_cancel(target, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verification() -> Verification {
        let step = |kind| db::VerifiedStep {
            sql: String::new(),
            kind,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        };
        Verification {
            steps: vec![
                step(db::StepKind::Delete),
                step(db::StepKind::Update),
                step(db::StepKind::Insert),
            ],
            rows: vec!["id = 1".into(), "id = 2".into(), "added row 1".into()],
            identities: Vec::new(),
        }
    }

    /// A collection's three documents keyed by `_id`, as a `find` on it reads
    /// them, each cell carrying its own `$type` tag.
    fn mongo_grid() -> ResultGrid {
        let id = |n: u8| Some(format!("ObjectId('65a4f1c0ffffffffffffff{n:02x}')"));
        let column = |name: &str, data_type: &str| db::Column {
            name: name.into(),
            data_type: Some(data_type.into()),
        };
        ResultGrid::new(
            QueryResult {
                columns: vec![column("_id", "objectId"), column("name", "string")],
                rows: db::rows(vec![
                    vec![id(1), Some("Ann".into())],
                    vec![id(2), Some("Bo".into())],
                    vec![id(3), Some("Cy".into())],
                ]),
                cell_types: vec![vec!["objectId", "string"]; 3],
                edit: Some(db::EditTarget {
                    schema: "dbdelve_dev".into(),
                    table: "accounts".into(),
                    columns: vec![Some("_id".into()), Some("name".into())],
                    keys: vec![0],
                    ..Default::default()
                }),
                ..QueryResult::default()
            },
            crate::sql::Mode::ReadWrite,
        )
        .with_engine(Engine::MongoDb)
    }

    #[test]
    fn a_mongo_collection_tab_offers_draft_rows_and_marks() {
        let mut grid = mongo_grid();
        let (mongo, object) = (Target::built_in(Engine::MongoDb), Tab::Object(0));
        assert!(grid.toggle_marks(&[0]), "a document is marked by its _id");
        assert!(grid.set_pending(1, 1, NewValue::Value("Boris".into())));
        let added = grid
            .add_row()
            .expect("a collection takes an added document");
        assert!(grid.set_pending(added, 1, NewValue::Value("Di".into())));

        // The row the ring is on is the added one, which Delete row removes.
        assert!(markable_row(mongo, object, EditMode::Draft, &grid));
        assert!(!markable_row(mongo, object, EditMode::Row, &grid));
        assert!(!markable_row(mongo, object, EditMode::Classic, &grid));
        assert!(!markable_row(mongo, Tab::Query(0), EditMode::Draft, &grid));
        assert!(!markable_row(
            Target::built_in(Engine::Redis),
            object,
            EditMode::Draft,
            &grid
        ));

        // The manifest of its draft, as Apply draft builds it.
        let batch = sql::draft_batch(
            Engine::MongoDb,
            &grid.draft_deletes(),
            &grid.pending_updates(),
            &grid
                .draft_inserts()
                .expect("the added document holds a value"),
        )
        .expect("the draft is written");
        let verify = super::super::editing::verification(Engine::MongoDb, &grid, &batch, false)
            .expect("a collection's draft can be verified");
        let kinds: Vec<_> = verify.steps.iter().map(|step| step.kind).collect();
        assert_eq!(
            kinds,
            [StepKind::Delete, StepKind::Update, StepKind::Insert]
        );
        assert_eq!(
            verify.rows,
            [
                "_id = ObjectId('65a4f1c0ffffffffffffff01')",
                "_id = ObjectId('65a4f1c0ffffffffffffff02')",
                "added row 1"
            ]
        );
        for step in &verify.steps[..2] {
            assert!(step.reread.is_some(), "{}", step.sql);
            let tags: Vec<_> = step
                .before
                .iter()
                .map(|cell| (cell.column.as_str(), cell.cell_type.as_deref()))
                .collect();
            assert_eq!(
                tags,
                [("_id", Some("objectId")), ("name", Some("string"))],
                "each fetched cell carries its $type tag"
            );
        }
        assert!(verify.steps[2].reread.is_none() && verify.steps[2].before.is_empty());
    }

    #[test]
    fn a_verified_outcome_names_each_row_in_what_happened() {
        let verify = verification();
        let said = |outcome: ApplyOutcome| verified_message(&outcome, &verify, false);

        assert_eq!(
            said(ApplyOutcome::Drifted(vec![
                (0, Drift::Changed),
                (1, Drift::Gone)
            ]))
            .as_deref(),
            Some(
                "Nothing was applied. Row id = 1 does not match the row as it was read. \
                 Row id = 2 is no longer on the server."
            ),
            "every drifted row, in step order"
        );
        assert_eq!(
            said(ApplyOutcome::Drifted(vec![(1, Drift::Ambiguous(2))])).as_deref(),
            Some("Nothing was applied. Row id = 2 matches more than one row on the server.")
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 1,
                count: Some(0)
            })
            .as_deref(),
            Some("Nothing was applied. UPDATE 2 (row id = 2) affected 0 rows.")
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 0,
                count: Some(2)
            })
            .as_deref(),
            Some("Nothing was applied. DELETE 1 (row id = 1) affected 2 rows.")
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 1,
                count: None
            })
            .as_deref(),
            Some(
                "Nothing was applied. UPDATE 2 (row id = 2) did not say how many rows it affected."
            )
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 2,
                count: Some(0)
            })
            .as_deref(),
            Some("Nothing was applied. INSERT 3 (added row 1) affected 0 rows.")
        );
        assert_eq!(
            said(ApplyOutcome::Applied {
                affected: 3,
                read_backs: Vec::new()
            }),
            None
        );
    }

    /// A lone New Row whose write concern failed was inserted, though no row
    /// names it: it counts as written, so its review is dropped rather than
    /// left to insert it again.
    #[test]
    fn a_write_concern_failure_counts_a_step_no_row_names_as_written() {
        let concern = ApplyOutcome::Failed {
            stage: db::ApplyStage::Concern(0),
            error: DbError {
                message: "waiting for replication timed out".into(),
                position: None,
            },
        };
        assert_eq!(written_rows_of(&concern, &[]), (Vec::new(), 1));
        let refused = ApplyOutcome::Failed {
            stage: db::ApplyStage::Write(0),
            error: DbError {
                message: "duplicate key".into(),
                position: None,
            },
        };
        assert_eq!(written_rows_of(&refused, &[]), (Vec::new(), 0));
    }

    #[test]
    fn a_mongo_apply_that_stops_part_way_says_which_statements_were_applied() {
        let step = |kind| db::VerifiedStep {
            sql: String::new(),
            kind,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        };
        // A delete, an update and three added documents, as a draft orders them.
        let verify = Verification {
            steps: vec![
                step(db::StepKind::Delete),
                step(db::StepKind::Update),
                step(db::StepKind::Insert),
                step(db::StepKind::Insert),
                step(db::StepKind::Insert),
            ],
            rows: vec![
                "_id = 1".into(),
                "_id = 2".into(),
                "added row 1".into(),
                "added row 2".into(),
                "added row 3".into(),
            ],
            identities: Vec::new(),
        };
        let failed = |stage| ApplyOutcome::Failed {
            stage,
            error: DbError {
                message: "duplicate key".into(),
                position: None,
            },
        };
        let said = |outcome: ApplyOutcome| verified_message(&outcome, &verify, true);

        assert_eq!(
            said(failed(db::ApplyStage::Write(2))).as_deref(),
            Some(
                "Statements 1 to 2 were applied and have left the draft. Statement 3 \
                 (added row 1) failed: duplicate key. Statements 4 to 5 were not sent."
            )
        );
        assert_eq!(
            said(failed(db::ApplyStage::Write(3))).as_deref(),
            Some(
                "Statements 1 to 3 were applied and have left the draft. Statement 4 \
                 (added row 2) failed: duplicate key. Statement 5 was not sent."
            ),
            "one statement left reads in the singular"
        );
        assert_eq!(
            said(failed(db::ApplyStage::Write(4))).as_deref(),
            Some(
                "Statements 1 to 4 were applied and have left the draft. Statement 5 \
                 (added row 3) failed: duplicate key."
            ),
            "nothing after the failure, so nothing is said was not sent"
        );
        assert_eq!(
            said(failed(db::ApplyStage::Write(1))).as_deref(),
            Some(
                "Statement 1 was applied and has left the draft. Statement 2 (row _id = 2) \
                 failed: duplicate key. Statements 3 to 5 were not sent."
            )
        );
        assert_eq!(
            said(failed(db::ApplyStage::Write(0))).as_deref(),
            Some(
                "Nothing was applied. Statement 1 (row _id = 1) failed: duplicate key. \
                 Statements 2 to 5 were not sent."
            )
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 0,
                count: Some(0)
            })
            .as_deref(),
            Some(
                "Nothing was applied. DELETE 1 (row _id = 1) affected 0 rows. \
                 Statements 2 to 5 were not sent."
            )
        );
        assert_eq!(
            said(ApplyOutcome::Miscounted {
                step: 1,
                count: Some(2)
            })
            .as_deref(),
            Some(
                "Statement 1 was applied and has left the draft. UPDATE 2 (row _id = 2) \
                 affected 2 rows. Statements 3 to 5 were not sent."
            )
        );
        // A write nothing answered may have applied: it is said apart from
        // the writes before it, which applied, and those after, never sent.
        let unanswered = |step| ApplyOutcome::Failed {
            stage: db::ApplyStage::Unanswered(step),
            error: DbError {
                message: "connection reset by peer".into(),
                position: None,
            },
        };
        assert_eq!(
            said(unanswered(2)).as_deref(),
            Some(
                "Statements 1 to 2 were applied and have left the draft. Statement 3 \
                 (added row 1) got no answer, so it may have been applied: connection reset by \
                 peer. Statements 4 to 5 were not sent. Read the rows again to see what the \
                 server holds."
            )
        );
        let first = said(unanswered(0)).expect("a first write nothing answered is said");
        assert_eq!(
            first,
            "Statement 1 (row _id = 1) got no answer, so it may have been applied: connection \
             reset by peer. Statements 2 to 5 were not sent. Read the rows again to see what \
             the server holds."
        );
        assert!(!first.contains("Nothing was applied"), "{first}");
        // One whose write concern failed applied, and has left the draft.
        assert_eq!(
            said(ApplyOutcome::Failed {
                stage: db::ApplyStage::Concern(1),
                error: DbError {
                    message: "The writes applied, but their write concern failed: timed out".into(),
                    position: None,
                },
            })
            .as_deref(),
            Some(
                "Statements 1 to 2 were applied and have left the draft. Statement 2 \
                 (row _id = 2): The writes applied, but their write concern failed: timed out. \
                 Statements 3 to 5 were not sent."
            )
        );
        let landed = verified_result(unanswered(2), &verify, true)
            .expect_err("a write nothing answered is an error");
        assert!(
            landed.message.contains("may have been applied"),
            "{}",
            landed.message
        );
        // A failure before any write, and a drift, wrote nothing and keep the
        // sentences of a transactional engine.
        assert_eq!(said(failed(db::ApplyStage::Reread(1))), None);
        assert_eq!(
            said(ApplyOutcome::Drifted(vec![(1, Drift::Gone)])).as_deref(),
            Some("Nothing was applied. Row _id = 2 is no longer on the server.")
        );
        // A transactional target rolled it all back, whichever step it was.
        let rolled_back = |outcome: ApplyOutcome| verified_message(&outcome, &verify, false);
        assert_eq!(rolled_back(failed(db::ApplyStage::Write(2))), None);
        assert_eq!(
            rolled_back(ApplyOutcome::Miscounted {
                step: 1,
                count: Some(0)
            })
            .as_deref(),
            Some("Nothing was applied. UPDATE 2 (row _id = 2) affected 0 rows.")
        );
        // What lands in the draft's notice is the same sentence.
        let landed = verified_result(failed(db::ApplyStage::Write(2)), &verify, true)
            .expect_err("a part-way failure is an error");
        assert!(landed.message.starts_with("Statements 1 to 2 were applied"));
    }

    #[test]
    fn an_ordered_apply_has_written_the_steps_before_the_one_it_stopped_at() {
        let failed = |stage| ApplyOutcome::Failed {
            stage,
            error: DbError {
                message: "refused".into(),
                position: None,
            },
        };
        assert_eq!(written_before_stop(&failed(ApplyStage::Write(3))), 3);
        assert_eq!(
            written_before_stop(&ApplyOutcome::Miscounted {
                step: 2,
                count: Some(0)
            }),
            2
        );
        // A read-back follows its step's write, so that step is written.
        assert_eq!(written_before_stop(&failed(ApplyStage::ReadBack(0))), 1);
        // Before any write, or with none sent: nothing is written.
        for outcome in [
            failed(ApplyStage::Begin),
            failed(ApplyStage::Reread(2)),
            ApplyOutcome::Drifted(vec![(1, Drift::Gone)]),
            ApplyOutcome::Applied {
                affected: 3,
                read_backs: Vec::new(),
            },
        ] {
            assert_eq!(written_before_stop(&outcome), 0, "{outcome:?}");
        }
    }

    #[test]
    fn a_failed_read_back_after_a_written_update_lands_as_written_with_the_read_failing() {
        let error = DbError {
            message: "connection reset".into(),
            position: None,
        };
        let (outcome, read_failed) = read_back_after_write(ApplyOutcome::Failed {
            stage: ApplyStage::ReadBack(0),
            error: error.clone(),
        });
        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                affected: 1,
                read_backs: Vec::new()
            }
        );
        assert_eq!(read_failed, Some(error.clone()));
        // The row lands as written, said with the read's own words, and the
        // tab reads the relation again.
        let mut grid = ResultGrid::empty();
        let rows_id = grid.rows_id();
        let (said, reread) = land_row(
            &mut grid,
            (0, rows_id, &[]),
            read_failed.map(Err),
            "ok".into(),
        );
        assert_eq!(
            said,
            "The row was updated. Reading it back failed: connection reset"
        );
        assert!(reread);
        // A write that failed is not written, and is left as it is.
        let refused = |error| ApplyOutcome::Failed {
            stage: ApplyStage::Write(0),
            error,
        };
        assert_eq!(
            read_back_after_write(refused(error.clone())),
            (refused(error), None)
        );
    }

    #[test]
    fn only_an_ordered_write_nothing_answered_leaves_the_outcome_unknown() {
        let error = DbError {
            message: "connection reset by peer".into(),
            position: None,
        };
        let failed_at = |stage| ApplyOutcome::Failed {
            stage,
            error: error.clone(),
        };
        // A write the server answered with a refusal applied nothing, and one
        // whose write concern failed applied: both are known.
        assert!(!unknown_outcome(&failed_at(ApplyStage::Write(1))));
        assert!(!unknown_outcome(&failed_at(ApplyStage::Concern(1))));
        assert!(!unknown_outcome(&failed_at(ApplyStage::Reread(0))));
        // One nothing answered may have applied.
        assert!(unknown_outcome(&failed_at(ApplyStage::Unanswered(1))));
        assert_eq!(
            written_before_stop(&failed_at(ApplyStage::Unanswered(1))),
            1
        );
        assert_eq!(written_before_stop(&failed_at(ApplyStage::Concern(1))), 2);
        assert!(unknown_outcome(&ApplyOutcome::Unknown(error)));
    }

    /// A MongoDB batch has no transaction: one a query tab appended to its
    /// buffer is not verified, so nothing says which of its statements applied
    /// before one failed, and its draft is not kept to be sent again.
    #[test]
    fn only_a_verified_mongo_apply_keeps_its_draft() {
        let mongo = Target::built_in(Engine::MongoDb);
        assert!(!keeps_draft(mongo, false), "a query tab's buffer apply");
        assert!(keeps_draft(mongo, true), "a reviewed draft or a Row send");
        let sqlite = Target::built_in(Engine::Sqlite);
        assert!(keeps_draft(sqlite, false) && keeps_draft(sqlite, true));
        assert!(!keeps_draft(Target::built_in(Engine::Redis), true));
    }

    #[test]
    fn a_lost_commit_is_said_as_an_unknown_outcome() {
        let said = verified_message(
            &ApplyOutcome::Unknown(DbError {
                message: "connection reset by peer".into(),
                position: None,
            }),
            &verification(),
            false,
        )
        .expect("a lost COMMIT is said");

        assert_eq!(
            said,
            "The connection was lost during COMMIT; whether the changes were applied is not known."
        );
        assert!(!said.contains("Nothing was applied."), "{said}");
    }

    #[test]
    fn a_run_answered_without_its_tab_keeps_only_what_the_answer_says() {
        let entry = store::HistoryEntry::started("select 1");
        let streamed = QueryResult {
            columns: vec![db::Column::default()],
            elapsed: std::time::Duration::from_millis(9),
            ..QueryResult::default()
        };
        let landed = answered(
            &entry,
            &Ok(streamed.clone()),
            true,
            Default::default(),
            Target::built_in(Engine::Sqlite),
        );
        assert_eq!((landed.rows, landed.ms), (None, Some(9)));
        let capped = QueryResult {
            capped_from: Some(7),
            ..streamed
        };
        let landed = answered(
            &entry,
            &Ok(capped),
            true,
            Default::default(),
            Target::built_in(Engine::Sqlite),
        );
        assert_eq!(landed.rows, Some(7));

        let interrupted = DbError {
            message: "interrupted".into(),
            position: None,
        };
        let landed = answered(
            &entry,
            &Err(interrupted),
            false,
            Default::default(),
            Target::built_in(Engine::Sqlite),
        );
        assert!(landed.cancelled && landed.error.is_none());
    }

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
    fn a_rewrite_replaces_only_what_changed() {
        assert_eq!(
            changed_span("select 1;", "select 1;\nselect 2;"),
            (9..9, 9..19)
        );
        assert_eq!(changed_span("a b c", "a x c"), (2..3, 2..3));
        assert_eq!(changed_span("same", "same"), (4..4, 4..4));
        // A repeated letter at the seam is shared once, not by both ends.
        assert_eq!(changed_span("aa", "aaa"), (2..2, 2..3));
        assert_eq!(changed_span("é1", "é2é"), (2..3, 2..5));
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

    #[test]
    fn a_row_send_whose_read_back_fails_clears_only_the_cells_it_sent() {
        let column = |name: &str| db::Column {
            name: name.into(),
            data_type: None,
        };
        let edited_as = |row_match: db::RowMatch| {
            let mut grid = ResultGrid::new(
                QueryResult {
                    columns: vec![column("id"), column("name"), column("n")],
                    rows: crate::db::rows(vec![vec![
                        Some("1".into()),
                        Some("wren".into()),
                        Some("1".into()),
                    ]]),
                    edit: Some(db::EditTarget {
                        schema: "main".into(),
                        table: "birds".into(),
                        columns: vec![Some("id".into()), Some("name".into()), Some("n".into())],
                        keys: vec![0],
                        row_match,
                        ..Default::default()
                    }),
                    ..QueryResult::default()
                },
                Mode::ReadWrite,
            );
            assert!(grid.set_pending(0, 1, NewValue::Value("robin".into())));
            let sent = grid.pending_cells(0);
            // Staged while the UPDATE was out.
            assert!(grid.set_pending(0, 2, NewValue::Value("2".into())));
            (grid, sent)
        };
        let edited = || edited_as(db::RowMatch::Key);
        let failed = DbError {
            message: "connection reset".into(),
            position: None,
        };
        let empty = QueryResult::default();
        let two = QueryResult {
            rows: crate::db::rows(vec![vec![Some("1".into())], vec![Some("1".into())]]),
            ..QueryResult::default()
        };
        let narrow = QueryResult {
            rows: crate::db::rows(vec![vec![Some("1".into())]]),
            ..QueryResult::default()
        };
        for read in [
            Some(Err(failed)),
            Some(Ok(empty)),
            Some(Ok(two)),
            Some(Ok(narrow)),
            None,
        ] {
            let (mut grid, sent) = edited();
            let rows_id = grid.rows_id();
            let (_, reread) = land_row(&mut grid, (0, rows_id, &sent), read, "ok".into());
            assert!(reread);
            assert_eq!(
                grid.pending_cells(0),
                vec![(2, NewValue::Value("2".into()))],
                "the edit staged since stays, and the read waits on it"
            );
        }

        // A row named by every value is found again by the values it was
        // sent: a one-row read holding exactly them is placed where it stood,
        // and the relation is not read again.
        let stored = |n: &str| QueryResult {
            columns: vec![column("n"), column("id"), column("name")],
            rows: crate::db::rows(vec![vec![
                Some(n.into()),
                Some("1".into()),
                Some("robin".into()),
            ]]),
            ..QueryResult::default()
        };
        let (mut grid, sent) = edited_as(db::RowMatch::EveryColumn { fixed: Vec::new() });
        let rows_id = grid.rows_id();
        let (_, reread) = land_row(
            &mut grid,
            (0, rows_id, &sent),
            Some(Ok(stored("1"))),
            "ok".into(),
        );
        assert!(!reread, "the relation is not read again");
        assert_eq!(grid.cell(0, 1), Some("robin"));
        assert_eq!(grid.cell(0, 2), Some("1"));

        // One holding another value may be another row that already held the
        // sent ones: it is not placed, and the relation is read instead.
        let (mut grid, sent) = edited_as(db::RowMatch::EveryColumn { fixed: Vec::new() });
        let rows_id = grid.rows_id();
        let (_, reread) = land_row(
            &mut grid,
            (0, rows_id, &sent),
            Some(Ok(stored("7"))),
            "ok".into(),
        );
        assert!(reread, "the relation is read again");
        assert_eq!(grid.cell(0, 1), Some("wren"), "nothing is placed");
    }
}

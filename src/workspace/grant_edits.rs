//! The Show Grants dialog's Edit mode: cells that stage a grant or a revoke
//! for the role and object of their row. Nothing staged is sent from here:
//! the changes go to the apply review whole, and run only from its Run.
//!
//! A run is a batch per database (`db::roles::grant_statements`), each
//! gated by `sql::is_generated_grant_change` as the review is written and by
//! the connection's mode at Run. Each goes to the session of its database
//! (`route_in`, `run_routed`) and runs on a session of its own beside it
//! (`Connection::generated_apart_settled`), as a role change does: a transaction the
//! user has open never holds it, nor would its rollback undo it.

use super::*;

use crate::db::ApartOutcome;
use crate::db::roles::{self, GrantBatch};
use crate::session::GrantRun;
use crate::users_view::Loaded;

/// Said when Run finds the connection opened again or moved to another
/// database since the review was written.
const MOVED_SINCE_REVIEW: &str = "Not sent: the connection was opened again or moved to another \
     database since these changes were reviewed. Close this and review them again.";

/// Why a grant run stopped at a batch.
struct RunFailure {
    message: String,
    /// The session was lost with the batch out: it may have committed.
    unknown: bool,
}

impl Workspace {
    /// Whether the dialog in front may be put in Edit mode: its grants are
    /// read and its connection's engine writes grant changes.
    pub(crate) fn grants_editable(&self) -> bool {
        self.grants_view_in_front()
            .is_some_and(|view| matches!(view.data, crate::users_view::Loaded::Ready(_)))
            && self.target().manages_roles()
    }

    /// Turn Edit mode on or off. Leaving it drops what was staged: a change
    /// out of sight is not one the user can be said to have meant.
    pub(crate) fn set_grants_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
        if editing && !self.grants_editable() {
            return;
        }
        let Some(view) = self.grants_view.as_mut() else {
            return;
        };
        view.editing = editing;
        view.refusal = None;
        if !editing {
            view.staged.clear();
            view.added.clear();
        }
        view.rebuild(true, cx);
        cx.notify();
    }

    /// A click on the cell of row `key`'s `column` in Edit mode: stage the
    /// change it names, or take it back when it is staged. A cell that cannot
    /// change stages nothing and says why.
    pub(crate) fn toggle_grant_cell(&mut self, key: &str, column: usize, cx: &mut Context<Self>) {
        let Some(view) = self.grants_view.as_mut().filter(|view| view.editing) else {
            return;
        };
        let Some(row) = view.matrix.row(key) else {
            return;
        };
        view.refusal = view.staged.toggle(row, &view.matrix.columns, column).err();
        cx.notify();
    }

    /// Grant to…: list `role` in an object view, holding nothing yet, so a
    /// grant can be staged on its row.
    pub(crate) fn add_grant_role(&mut self, role: &str, cx: &mut Context<Self>) {
        let Some(view) = self.grants_view.as_mut().filter(|view| view.editing) else {
            return;
        };
        let Loaded::Ready(GrantsOf::Object(grants)) = &view.data else {
            return;
        };
        if !crate::grants::grant_candidates(grants, &view.added).contains(&role) {
            return;
        }
        view.added.push(role.to_string());
        view.refusal = None;
        view.rebuild(true, cx);
        cx.notify();
    }

    /// Drop every staged change; Edit mode stays on.
    pub(crate) fn discard_grant_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.grants_view.as_mut() {
            view.staged.clear();
            view.refusal = None;
            cx.notify();
        }
    }

    /// Review changes…: the staged changes written as statements and put up
    /// in the apply review, every one shown; or why they were not.
    pub(crate) fn review_grant_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.write_grant_review(window, cx)
            && let Some(view) = self.grants_view.as_mut()
        {
            view.refusal = Some(message);
        }
        cx.notify();
    }

    fn write_grant_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(view) = self.grants_view_in_front().filter(|view| view.editing) else {
            return Err("Turn on Edit to change grants.".into());
        };
        if view.staged.is_empty() {
            return Err("Nothing is staged.".into());
        }
        let (request, changes) = (view.request, view.staged.changes());
        if !self.grants_editable() {
            return Err("This connection does not write grant changes.".into());
        }
        let Some(profile) = self.profile() else {
            return Err("The connection is not open.".into());
        };
        let own = profile.active_database();
        let generation = profile.generation;
        let tab = profile.session.active;
        let batches =
            roles::grant_statements(&crate::grants::on_own_session(changes, own.as_deref()));
        // The gate every statement this dialog writes passes (`AGENTS.md`
        // rule 2), each batch as it will run: failing it is a bug in
        // dbdelve, not something the user did.
        let engine = self.engine();
        if batches.is_empty()
            || (batches.iter()).any(|batch| !sql::is_generated_grant_change(engine, &batch.sql))
        {
            return Err(
                "dbdelve refused to run a statement it wrote itself: it is not a grant change."
                    .into(),
            );
        }
        // Over whatever tab is in front, which a Show Grants of a relation
        // opened from the explorer has no Users and roles tab to be.
        if !self.review_slot_free(tab, false, window, cx) {
            return Err("Run or close the statement up for review first.".into());
        }
        let mut notes = vec![
            "A grant change needs the Full connection mode. It runs on a session of its own, \
             never inside a transaction open on the connection."
                .to_string(),
        ];
        notes.push(match batches.len() {
            1 => "Its statements apply together or not at all.".into(),
            _ => "Each database's statements run on that database's session and apply together \
                  or not at all. The databases run in the order shown and stop at the first \
                  that fails; those before it stay applied."
                .into(),
        });
        let sql = crate::grants::review_text(&batches, own.as_deref());
        let written_on = (generation, own);
        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = Some(ApplyReview {
                tab,
                title: "Change grants",
                notes,
                sql,
                error: None,
                edits: false,
                verify: None,
                ran_on: None,
                joining: false,
                grants: Some(GrantRun {
                    batches,
                    request,
                    written_on,
                    running: None,
                    cancelling: false,
                }),
            });
        }
        Ok(())
    }

    /// Run a grant review: the mode first, then each database's batch in
    /// turn, stopping at the first that fails. One Run at a time.
    pub(crate) fn run_grant_review(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(run) =
            (profile.session.apply_review.as_ref()).and_then(|review| review.grants.as_ref())
        else {
            return;
        };
        if run.running.is_some() {
            return;
        }
        // A batch of the own database names none: since a reconnect or a
        // switch it would run where the review does not say.
        if run.written_on != (profile.generation, profile.active_database()) {
            if let Some(review) = (self.profile_mut()).and_then(|p| p.session.apply_review.as_mut())
            {
                review.error = Some(MOVED_SINCE_REVIEW.into());
            }
            cx.notify();
            return;
        }
        let (batches, request) = (run.batches.clone(), run.request);
        let text: Vec<&str> = batches.iter().map(|batch| batch.sql.as_str()).collect();
        let verdict = sql::classify(self.engine(), &text.join("\n"));
        match sql::gate(&verdict, profile.mode, &profile.confirmed) {
            None => {}
            // The prompt offers the mode alone; Run is pressed again after.
            Some(sql::Stop::Upgrade(mode)) => {
                self.require(mode, cx);
                return;
            }
            Some(_) => {
                if let Some(review) = self
                    .profile_mut()
                    .and_then(|p| p.session.apply_review.as_mut())
                {
                    review.error = Some(
                        "dbdelve could not read the statements it wrote, so none was sent.".into(),
                    );
                }
                cx.notify();
                return;
            }
        }
        let (id, generation) = (profile.id.clone(), profile.generation);
        let cancel = crate::db::CancelToken::alongside();
        if let Some(review) = self
            .profile_mut()
            .and_then(|p| p.session.apply_review.as_mut())
        {
            review.error = None;
            review.ran_on = Some(generation);
            if let Some(run) = review.grants.as_mut() {
                run.running = Some(cancel.clone());
                run.cancelling = false;
            }
        }
        cx.notify();
        cx.spawn(async move |workspace, cx| {
            let mut applied = 0;
            let mut failure: Option<RunFailure> = None;
            for batch in &batches {
                let (sql, token) = (batch.sql.clone(), cancel.clone());
                let task = workspace
                    .update(cx, |workspace, cx| {
                        let (profile_id, opened, routed) = workspace
                            .route_in(&id, batch.database.clone())
                            .filter(|(_, opened, _)| *opened == generation)?;
                        Some(workspace.run_routed(
                            &profile_id,
                            opened,
                            routed,
                            move |connection| Ok(connection.generated_apart_settled(&sql, &token)),
                            cx,
                        ))
                    })
                    .ok()
                    .flatten();
                let outcome = match task {
                    Some(task) => task.await.unwrap_or_else(ApartOutcome::Failed),
                    None => ApartOutcome::Failed(DbError {
                        message: "The connection was closed or opened again since Run.".into(),
                        position: None,
                    }),
                };
                match outcome {
                    ApartOutcome::Applied => applied += 1,
                    ApartOutcome::Failed(error) => {
                        failure = Some(RunFailure {
                            message: error.message,
                            unknown: false,
                        });
                        break;
                    }
                    ApartOutcome::Unknown(error) => {
                        failure = Some(RunFailure {
                            message: error.message,
                            unknown: true,
                        });
                        break;
                    }
                }
            }
            _ = workspace.update(cx, |workspace, cx| {
                workspace.land_grant_run(&id, request, &cancel, &batches, applied, failure, cx)
            });
        })
        .detach();
    }

    /// A grant run ended, `applied` batches in, and `failure` the next one's
    /// error if it stopped there. Success closes the review. A failure is
    /// said in it, the staged changes kept, and the batches that committed
    /// leave it, so Run again never sends them twice. A batch whose outcome
    /// is unknown closes it instead: what it holds may have applied, and
    /// only a review written from the grants as read again can say what is
    /// left. Once anything may have applied, the dialog reads the grants
    /// again and the changes that landed leave what is staged.
    #[allow(clippy::too_many_arguments)]
    fn land_grant_run(
        &mut self,
        id: &str,
        request: u64,
        cancel: &crate::db::CancelToken,
        batches: &[GrantBatch],
        applied: usize,
        failure: Option<RunFailure>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let own = profile.active_database();
        let names = |batches: &[GrantBatch]| -> Vec<String> {
            (batches.iter())
                .map(|batch| {
                    crate::grants::database_name(batch.database.as_deref(), own.as_deref())
                })
                .collect()
        };
        let unknown = failure.as_ref().is_some_and(|failure| failure.unknown);
        let said = failure.map(|failure| {
            crate::grants::run_failure(
                &names(&batches[..applied]),
                &names(&batches[applied..=applied])[0],
                &failure.message,
                failure.unknown,
                &names(&batches[applied + 1..]),
            )
        });
        let ours = (profile.session.apply_review.as_ref()).is_some_and(|review| {
            (review.grants.as_ref())
                .and_then(|run| run.running.as_ref())
                .is_some_and(|running| running.is(cancel))
        });
        match (ours, said) {
            (true, None) => profile.session.apply_review = None,
            (true, Some(said)) if unknown => {
                profile.session.apply_review = None;
                self.note(said, cx);
            }
            (true, Some(said)) => {
                if let Some(review) = profile.session.apply_review.as_mut() {
                    review.error = Some(said);
                    if let Some(run) = review.grants.as_mut() {
                        run.running = None;
                        run.cancelling = false;
                        run.batches.drain(..applied.min(run.batches.len()));
                        review.sql =
                            crate::grants::review_text(&run.batches, run.written_on.1.as_deref());
                    }
                }
            }
            // Closed while it ran, after a Cancel: said where the user is.
            (false, Some(said)) => self.note(said, cx),
            (false, None) => {}
        }
        if applied > 0 || unknown {
            self.reread_grants(id, request, cx);
        }
        cx.notify();
    }

    /// Read the dialog's grants again, as a new request, so a read still out
    /// from before the run cannot land over it.
    fn reread_grants(&mut self, id: &str, request: u64, cx: &mut Context<Self>) {
        let next = self.last_grants_request + 1;
        let Some(view) = (self.grants_view.as_mut())
            .filter(|view| view.request == request && view.profile == id)
        else {
            return;
        };
        let object = match (&view.data, &view.object) {
            (Loaded::Ready(GrantsOf::Object(_)), Some(object)) => Ok(object.clone()),
            (Loaded::Ready(GrantsOf::Role(roles)), _) => Err(roles.role.clone()),
            _ => return,
        };
        view.request = next;
        self.last_grants_request = next;
        match object {
            Ok(object) => self.read_object_grants(id, object, next, cx),
            Err(role) => self.read_role_access(id, role, next, cx),
        }
    }

    /// Cancel or Esc on a grant review: while its run is out, stop it first
    /// (what was not yet sent is not sent), and close only once that is asked.
    /// Whether this was a grant review's.
    pub(crate) fn close_grant_review(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let Some(review) = profile.session.apply_review.as_mut() else {
            return false;
        };
        let Some(run) = review.grants.as_mut() else {
            return false;
        };
        let cancel = match (&run.running, run.cancelling) {
            (Some(cancel), false) => cancel.clone(),
            _ => {
                profile.session.apply_review = None;
                cx.notify();
                return true;
            }
        };
        run.cancelling = true;
        if let Some(connection) = profile.connection() {
            let task = spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel));
            cx.background_executor()
                .spawn(async move {
                    if task.await.is_err() {
                        tracing::warn!("a grant run's cancel could not be sent");
                    }
                })
                .detach();
        }
        cx.notify();
        true
    }
}

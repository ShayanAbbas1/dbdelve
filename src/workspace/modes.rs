//! What the mode prompt does when it is answered, and the one place a
//! connection's mode changes.

use super::*;
use crate::session::{PendingRun, Resume};
use crate::sql::Stop;

impl Workspace {
    /// The only place a connection's mode changes. Single, because a later
    /// caller (the grid's own cache, the titlebar picker) is one more thing
    /// that would have to be told separately if this were not the one door.
    pub(crate) fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.mode = mode;
        // Every open grid caches the mode `editable` reads (`result_grid.rs`),
        // because that predicate is asked from the mouse and the palette with
        // no route back to the profile. Without this push a grid opened before
        // the change keeps answering from the mode it was built under until
        // its tab happens to re-run.
        let grids = profile.session.grids().cloned().collect::<Vec<_>>();
        for grid in grids {
            grid.update(cx, |table, _| table.delegate_mut().set_mode(mode));
        }
        self.remember_profiles(cx);
        cx.notify();

        // The server-side backstop behind the gate this door already guards --
        // see `Connection::set_read_only`. `query` blocks on the connection
        // mutex, so it goes to the background executor rather than running
        // here, same as `cancel_query` (queries.rs).
        //
        // ponytail: nothing orders these tasks against each other, so flipping
        // modes faster than a round trip can land them out of order and leave
        // the session held the opposite way to the mode on screen. Survivable
        // because `sql::gate` is the boundary and is unaffected -- what goes
        // stale is the backstop, not the protection. Stamp each task with a
        // per-profile counter and drop the stale ones if it ever matters.
        let Some(connection) = self.profile().and_then(Profile::connection) else {
            return;
        };
        let entering_read_only = mode == Mode::ReadOnly;
        let read_only_task = cx
            .background_executor()
            .spawn(async move { connection.set_read_only(entering_read_only) });
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = read_only_task.await {
                // Only entering Read-only is worth a notice: a failure here
                // leaves the user believing they have protection they don't.
                // Leaving Read-only on a failed statement leaves the server
                // still held to reads -- more than asked for, not less -- so
                // that direction fails silently on purpose.
                if entering_read_only {
                    _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
                }
            }
        })
        .detach();
    }

    /// Whether this connection may do `mode`-level work, raising the prompt
    /// when it may not. The prompt has nothing to resume: it offers the mode
    /// change alone, and the user repeats the keystroke.
    ///
    /// ponytail: threading a resumable gpui action through the prompt to save
    /// one keystroke is more machinery than the keystroke is worth. Store the
    /// action and re-dispatch it if the retry ever becomes annoying.
    pub(crate) fn require(&mut self, mode: Mode, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        if profile.mode >= mode {
            return true;
        }
        profile.session.pending_run = Some(PendingRun {
            resume: None,
            verdict: sql::Verdict {
                mode,
                destructive: Vec::new(),
            },
            dont_ask: false,
        });
        cx.notify();
        false
    }

    /// The titlebar menu's own entries route here rather than setting the
    /// field themselves, so raising or lowering the mode from the picker gets
    /// the same grid push and save as every other path into `set_mode`.
    pub(crate) fn choose_mode(&mut self, action: &SetMode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(action.mode, cx);
    }

    /// The picker's way back out of a silenced confirmation -- see `set_mode`'s
    /// caller in the titlebar menu. Without it, ticking "don't ask again" is a
    /// one-way door.
    pub(crate) fn reset_confirmations(
        &mut self,
        _: &ResetConfirmations,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(profile) = self.profile_mut() {
            profile.confirmed.clear();
            profile.confirmed_stale = false;
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    pub(crate) fn cancel_pending_run(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_run = None;
        }
        cx.notify();
    }

    pub(crate) fn toggle_dont_ask(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut()
            && let Some(pending) = &mut profile.session.pending_run
        {
            pending.dont_ask = !pending.dont_ask;
        }
        cx.notify();
    }

    pub(crate) fn approve_pending_run(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(PendingRun {
            resume,
            verdict,
            dont_ask,
        }) = profile.session.pending_run.take()
        else {
            return;
        };
        let Some(stop) = sql::gate(&verdict, profile.mode, &profile.confirmed) else {
            return;
        };

        match stop {
            Stop::Upgrade(mode) => {
                self.set_mode(mode, cx);
                // Re-checked rather than run outright: raising Read-only to
                // Full for a DROP answers "may this connection do this at
                // all" and leaves "did you mean this table", which is a
                // second dialog on purpose. `execute_and_then` is the only
                // arm here allowed to re-enter the gate -- it is the one
                // that just changed the mode the gate reads.
                let Some(resume) = resume else {
                    return;
                };
                self.execute_and_then(
                    resume.sql,
                    resume.tab,
                    resume.refresh,
                    resume.keep_rows,
                    resume.explain,
                    cx,
                );
            }
            // Confirm and RunOnce run *unchecked*. Sending either back
            // through `execute_and_then` would re-raise the prompt just
            // answered, forever.
            Stop::Confirm(kind) => {
                if dont_ask
                    && kind.suppressible()
                    && let Some(profile) = self.profile_mut()
                    && !profile.confirmed.contains(&kind)
                {
                    profile.confirmed.push(kind);
                    self.remember_profiles(cx);
                }
                self.run_resume(resume, cx);
            }
            Stop::RunOnce => self.run_resume(resume, cx),
        }
    }

    fn run_resume(&mut self, resume: Option<Resume>, cx: &mut Context<Self>) {
        let Some(resume) = resume else {
            return;
        };
        self.execute_unchecked(
            resume.sql,
            resume.tab,
            resume.refresh,
            resume.keep_rows,
            resume.explain,
            cx,
        );
    }
}

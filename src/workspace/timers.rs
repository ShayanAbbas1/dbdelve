//! Per-profile timers: the work a connection does on its own, between the
//! user's gestures.
//!
//! Each is a one-shot `Task` on the background executor that re-arms itself
//! when its round lands, and is held on the `Profile` so that replacing it
//! re-arms and dropping it cancels. A repeating timer would keep every test's
//! scheduler busy for ever (`docs/agents/session.md`), so every timer here is
//! off unless the profile asks, and nothing arms one for a profile that has
//! not connected.
//!
//! The blocking round trips run through [`spawn_blocking`], never on GPUI's
//! executor, which has no reactor to drive a socket.

use std::time::Duration;

use gpui::{App, Context};

use super::{Workspace, profiles::unapplied_edits_of, spawn_blocking};
use crate::db::DbError;
use crate::session::{Profile, ProfileState, Tab};

impl Workspace {
    /// Set the profile's keep-alive timer from its interval, replacing the one
    /// there: a new interval counts from now and a cleared one drops it. Reads
    /// the setting and whether the profile holds a connection, never the
    /// engine: restore and the form already offer it only where one applies.
    pub(crate) fn arm_keep_alive(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let generation = profile.generation;
        let interval = profile
            .behaviour
            .keep_alive
            .filter(|_| matches!(profile.state, ProfileState::Connected(_)));
        profile.keep_alive_task = interval.map(|seconds| {
            let id = id.to_string();
            cx.spawn(async move |workspace, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(u64::from(seconds)))
                    .await;
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.keep_alive_due(&id, generation, cx)
                });
            })
        });
    }

    /// The interval has passed: ping every session the profile holds open.
    ///
    /// A round is skipped, and the next one set, while the user has a
    /// transaction open on any session, so a ping never lands inside it. A
    /// session a statement is running on skips itself ([`crate::db::Connection::ping`]).
    /// This writes no profile field but the timer: a ping is not the user's
    /// activity and changes nothing they see.
    fn keep_alive_due(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        if !matches!(profile.state, ProfileState::Connected(_)) {
            // A connection lost since the last round is found on the next use,
            // and there is nothing left to keep.
            profile.keep_alive_task = None;
            return;
        }
        if profile.any_transaction_open() {
            self.arm_keep_alive(id, cx);
            return;
        }
        let sessions = profile.sessions_to_ping();
        #[cfg(test)]
        {
            self.pings += 1;
        }
        let round = spawn_blocking(cx.background_executor(), move || {
            sessions
                .iter()
                .try_for_each(|(connection, idle)| connection.ping(&**idle))
        });
        let task_id = id.to_string();
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        // The round takes the fired timer's place, so a disconnect meanwhile
        // drops it with the rest.
        profile.keep_alive_task = Some(cx.spawn(async move |workspace, cx| {
            let result = round.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.keep_alive_landed(&task_id, generation, result, cx)
            });
        }));
    }

    /// A round has finished. Answered, the next is set. A session that did not
    /// answer ends this profile's keep-alive until its next connect and says
    /// nothing: the lost session is found on its next use, as it was before
    /// there was a keep-alive, and a ping that retried would be a loop nobody
    /// asked for.
    pub(crate) fn keep_alive_landed(
        &mut self,
        id: &str,
        generation: u64,
        result: Result<(), DbError>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        match result {
            Ok(()) => self.arm_keep_alive(id, cx),
            Err(_) => {
                tracing::info!(
                    connection = crate::logging::connection_number(id),
                    "keep-alive stopped"
                );
                profile.keep_alive_task = None;
            }
        }
    }

    /// The user did something on this profile: the idle clock counts from now.
    /// Nothing else is done, and the timer is not re-armed per event: when it
    /// wakes it finds the moved deadline itself ([`Self::idle_due`]). Called for
    /// the user's own work only, never for a keep-alive ping, which would hold
    /// an unattended connection open for ever.
    pub(crate) fn mark_activity(&mut self, id: &str, cx: &App) {
        let now = cx.background_executor().now();
        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) {
            profile.last_activity = now;
        }
    }

    /// Set the profile's idle timer from its timeout, replacing the one there:
    /// it sleeps until the timeout after the last use, and a cleared timeout or
    /// a profile with no connection drops it.
    pub(crate) fn arm_idle(&mut self, id: &str, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let wait = idle_timeout(profile)
            .map(|timeout| (profile.last_activity + timeout).saturating_duration_since(now));
        self.arm_idle_after(id, wait, cx);
    }

    /// Sleep for `wait`, then check whether the profile is due to close; no
    /// wait is no timer. Kept apart from [`Self::arm_idle`] because a wait
    /// counted from the last use is already over once that is past, and a
    /// re-arm that counted it again would fire at once, for ever.
    fn arm_idle_after(&mut self, id: &str, wait: Option<Duration>, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let generation = profile.generation;
        let wait = wait.filter(|_| matches!(profile.state, ProfileState::Connected(_)));
        profile.idle_task = wait.map(|wait| {
            let id = id.to_string();
            cx.spawn(async move |workspace, cx| {
                cx.background_executor().timer(wait).await;
                _ = workspace.update(cx, |workspace, cx| workspace.idle_due(&id, generation, cx));
            })
        });
    }

    /// The wait is over. A profile used since the timer was set is waited on
    /// for the remainder, one with work unfinished for a whole timeout more,
    /// and any other is closed.
    pub(crate) fn idle_due(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let Some(index) = self
            .profiles
            .iter()
            .position(|profile| profile.id == id && profile.generation == generation)
        else {
            return;
        };
        let profile = &self.profiles[index];
        if !matches!(profile.state, ProfileState::Connected(_)) {
            // A session lost since the timer was set is found and reported on
            // the next use. Closing it here would replace that report with a
            // sentence about time, which is not what happened, and would turn a
            // failed profile into one that reconnects on Run without saying so.
            self.profiles[index].idle_task = None;
            return;
        }
        let Some(timeout) = idle_timeout(profile) else {
            self.profiles[index].idle_task = None;
            return;
        };
        let deadline = profile.last_activity + timeout;
        if now < deadline {
            self.arm_idle_after(id, Some(deadline - now), cx);
        } else if self.idle_blocked(index, cx) {
            self.arm_idle_after(id, Some(timeout), cx);
        } else {
            self.close_idle(index, cx);
        }
    }

    /// Whether the profile has something unfinished that closing would lose or
    /// roll back, front or not. Asked at the deadline, so none of it is watched
    /// for in between.
    fn idle_blocked(&self, index: usize, cx: &App) -> bool {
        let Some(profile) = self.profiles.get(index) else {
            return false;
        };
        let session = &profile.session;
        // The user's own work: a transaction would be rolled back, a statement
        // cancelled, an import or export left half done, and edits lost.
        profile.any_transaction_open()
            || session.queries.iter().any(|tab| {
                session.running(Tab::Query(tab.id))
                    || tab
                        .queue
                        .as_ref()
                        .is_some_and(|queue| queue.awaiting || !queue.remaining.is_empty())
            })
            || session
                .objects
                .iter()
                .any(|tab| session.running(Tab::Object(tab.id)))
            || self.running_import_into(&profile.id).is_some()
            || session.exporting.is_some()
            || unapplied_edits_of(profile, cx).is_some()
    }

    /// Close the profile's sessions. Its tabs and buffers stay, and so does
    /// its place in front: the profile is the same one with its connection
    /// gone, which the user reopens by using it, so it says why instead of
    /// going quiet.
    fn close_idle(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(minutes) = self
            .profiles
            .get(index)
            .and_then(|profile| profile.behaviour.idle_timeout)
        else {
            return;
        };
        // As a disconnect does: a schema graph read still running holds a
        // session the close would otherwise leave open.
        self.cancel_diagram_reads(index, cx);
        self.close_sessions(index, cx);
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        profile.session.notice = Some(format!(
            "Disconnected after {minutes} {} without use.",
            if minutes == 1 { "minute" } else { "minutes" }
        ));
        self.rebuild_connections(cx);
        cx.notify();
    }
}

/// How long the profile may sit unused before it closes; `None` is never,
/// as it is for a database that would go with its connection.
fn idle_timeout(profile: &Profile) -> Option<Duration> {
    profile
        .behaviour
        .idle_timeout
        .filter(|_| !profile.config.lives_with_its_connection())
        .map(|minutes| Duration::from_secs(u64::from(minutes) * 60))
}

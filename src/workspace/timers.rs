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

use gpui::Context;

use super::{Workspace, spawn_blocking};
use crate::db::{Connection, DbError};
use crate::session::ProfileState;

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
    /// session a statement is running on skips itself ([`Connection::ping`]).
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
        if profile.in_transaction || !profile.bound_transactions.is_empty() {
            self.arm_keep_alive(id, cx);
            return;
        }
        let sessions = profile.sessions_to_hold();
        #[cfg(test)]
        {
            self.pings += 1;
        }
        let round = spawn_blocking(cx.background_executor(), move || {
            sessions.iter().try_for_each(Connection::ping)
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
}

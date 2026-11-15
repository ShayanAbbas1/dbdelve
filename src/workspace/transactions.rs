//! Transaction control the user drives: the auto-commit switch, Commit and
//! Rollback.
//!
//! Every statement these send is written into the buffer first and runs from
//! there, the way a grid edit's batch does (`sql::appended_statement`). That
//! is the whole design: `AGENTS.md` hard rule 1 leaves no room for a `BEGIN`
//! dbdelve puts on the wire unseen, so the `BEGIN` goes where it can be read,
//! edited and undone, and what runs is what is on screen.

use super::*;
use crate::session::AutoCommitPrompt;

impl Workspace {
    /// Turn each statement committing on its own off or back on.
    ///
    /// Turning it off opens the transaction at once rather than waiting for
    /// the next statement: a switch that reads "off" while the session is
    /// still autocommitting is a lie for exactly as long as it takes to lose
    /// a row.
    ///
    /// Turning it back on while a transaction is open asks first, and sends
    /// nothing until the question is answered.
    pub(crate) fn toggle_auto_commit(
        &mut self,
        _: &ToggleAutoCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A prompt whose transaction ended under it draws nothing, and must
        // not come back for the next one.
        if let Some(profile) = self.profile_mut() {
            profile.session.auto_commit_prompt = None;
        }
        let Some((auto_commit, open, engine)) = self.profile().map(|profile| {
            (
                profile.auto_commit,
                profile.in_transaction,
                profile.config.engine(),
            )
        }) else {
            return;
        };
        let Some(begin) = engine.begins_transaction() else {
            self.note(
                format!("{} has no transaction to hold open.", engine.label()),
                cx,
            );
            return;
        };

        if !auto_commit {
            // Turning it back on has to end an open transaction to mean
            // anything, and dbdelve does not get to pick between committing
            // and discarding the user's work, so it asks. Refusing instead
            // left no way back: in Manual every Commit and Roll back opens
            // the next transaction at once.
            if open {
                if let Some(profile) = self.profile_mut() {
                    profile.session.auto_commit_prompt = Some(AutoCommitPrompt {
                        statements_shown: false,
                    });
                }
                cx.notify();
                return;
            }
            if let Some(profile) = self.profile_mut() {
                profile.auto_commit = true;
            }
            cx.notify();
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.auto_commit = false;
        }
        cx.notify();
        if !open {
            self.run_in_buffer(begin.to_string(), window, cx);
        }
    }

    pub(crate) fn commit_transaction(
        &mut self,
        _: &CommitTransaction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_transaction("COMMIT", true, window, cx);
    }

    pub(crate) fn rollback_transaction(
        &mut self,
        _: &RollbackTransaction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_transaction("ROLLBACK", true, window, cx);
    }

    /// Close the open transaction, and open the next one where the user has
    /// asked not to autocommit.
    ///
    /// Both go out as one submission so the session is never briefly
    /// autocommitting between them: a statement run in that gap would commit
    /// itself under a switch that reads "off". `sql::leaves` reads the pair
    /// and lands on the `BEGIN`, so `in_transaction` stays true across it. The
    /// Auto dialog's end is the only one that opens no next transaction, which
    /// is what `reopen` being false is for: it is the one end that leaves
    /// Manual.
    ///
    /// Whether the statement was handed to the run, `false` where a refusal
    /// already said why nothing was.
    fn end_transaction(
        &mut self,
        verb: &str,
        reopen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((open, auto_commit, engine)) = self.profile().map(|profile| {
            (
                profile.in_transaction,
                profile.auto_commit,
                profile.config.engine(),
            )
        }) else {
            return false;
        };
        if !open {
            self.note("No transaction is open on this connection.".into(), cx);
            return false;
        }
        let begin = (reopen && !auto_commit)
            .then(|| engine.begins_transaction())
            .flatten();
        let statement = match begin {
            Some(begin) => format!("{verb};\n\n{begin};"),
            None => verb.to_string(),
        };
        self.run_in_buffer(statement, window, cx);
        true
    }

    /// The Auto dialog's answer: end the open transaction with a plain
    /// `verb` (`COMMIT` or `ROLLBACK`) that opens no next one, then read Auto.
    ///
    /// The switch flips last, and only once the statement has succeeded: the
    /// run's completion does it (`Session::auto_after_end`). A switch reading
    /// Auto over a transaction that is still open is wrong for as long as it
    /// takes to lose a row, the mirror of the reason Manual opens its `BEGIN`
    /// at once, and a `COMMIT` can fail and leave the transaction open.
    /// `in_transaction` is marked as the statement is sent, so it is already
    /// false here when the run started; still true means a statement is
    /// running in that tab, or the connection is gone.
    pub(crate) fn end_transaction_for_auto(
        &mut self,
        verb: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let asked = self
            .profile_mut()
            .and_then(|profile| profile.session.auto_commit_prompt.take())
            .is_some();
        if !asked {
            return;
        }
        // Not `in_transaction` alone: it is also false when the transaction
        // ended between the dialog being drawn and the click, and arming
        // `auto_after_end` then would turn Auto on at the tab's next success.
        // A refusal there has said why already.
        if !self.end_transaction(verb, false, window, cx) {
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.in_transaction {
            self.note(
                format!("The {verb} was not sent, so auto-commit is still off."),
                cx,
            );
            return;
        }
        profile.session.auto_after_end = Some(profile.session.active);
        cx.notify();
    }

    /// Close the Auto dialog and change nothing.
    pub(crate) fn cancel_auto_commit_prompt(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.auto_commit_prompt.take())
            .is_some();
        if cancelled {
            self.refocus_front();
            cx.notify();
        }
        cancelled
    }

    pub(crate) fn toggle_auto_commit_statements(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = self
            .profile_mut()
            .and_then(|profile| profile.session.auto_commit_prompt.as_mut())
        {
            prompt.statements_shown = !prompt.statements_shown;
        }
        cx.notify();
    }

    /// Append `statement` to the buffer in front and run it from there.
    ///
    /// `recall_statement`'s shape, for its reasons: appended rather than
    /// swapped in, the cursor left on it because that is what `cmd+enter`
    /// reads, and a new buffer rather than whichever one happens to be behind
    /// an object tab. `keep_rows` because transaction control returns no rows
    /// and clearing the grid under it would look like the data went away.
    fn run_in_buffer(&mut self, statement: String, window: &mut Window, cx: &mut Context<Self>) {
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
        let appended = appended_statement(self.engine(), &text, &statement);
        let line = appended
            .lines()
            .count()
            .saturating_sub(statement.lines().count()) as u32;
        editor.update(cx, |editor, cx| {
            queries::rewrite(editor, &appended, window, cx);
            editor.set_cursor_position(Position::new(line, 0), window, cx);
        });
        self.activate_tab(Tab::Query(id), window, cx);
        self.execute_and_then(statement, Tab::Query(id), None, true, None, cx);
    }
}

//! Vim's modes in the query buffer: `escape` leaves Insert for Normal, `i`
//! goes back.

mod grammar;
mod run;

use std::time::Duration;

use gpui::{Context, KeyDownEvent, Task, Window};
use gpui_component::input;

use self::{
    grammar::{Parse, parse},
    run::run,
};
use crate::{session::QueryTab, workspace::Workspace};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    /// Where a buffer starts, so one is typed into as it always was until
    /// `escape` is pressed in it.
    #[default]
    Insert,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
        }
    }
}

/// A command still being typed. Dropping it drops `_timeout`, which cancels
/// the timer, so a new key replacing it starts the countdown again.
pub(crate) struct Pending {
    keys: String,
    _timeout: Task<()>,
}

/// How long a half-typed command waits for its next key.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Clears the pending keys of the query tab `tab_id` once `TIMEOUT` passes
/// without them being replaced.
fn start_timeout(tab_id: u64, cx: &mut Context<Workspace>) -> Task<()> {
    cx.spawn(async move |workspace, cx| {
        cx.background_executor().timer(TIMEOUT).await;
        let _ = workspace.update(cx, |workspace, cx| {
            if let Some(tab) = workspace
                .profile_mut()
                .and_then(|profile| profile.session.query_tab_mut(tab_id))
            {
                tab.vim_pending = None;
                cx.notify();
            }
        });
    })
}

/// None with Vim mode off in Settings, so the buffer types as it always did.
fn tab(workspace: &mut Workspace) -> Option<&mut QueryTab> {
    if !workspace.settings.vim_mode {
        return None;
    }
    workspace.profile_mut()?.session.active_query_tab_mut()
}

/// Bound on the buffer's bubble phase, so it is reached only once the editor
/// has nothing of its own to dismiss, such as a completion menu or extra
/// cursors.
pub(crate) fn escape(
    workspace: &mut Workspace,
    _: &input::Escape,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(tab) = tab(workspace) {
        tab.vim = Mode::Normal;
        tab.vim_pending = None;
        cx.notify();
    }
}

/// Bound on the buffer's capture phase, ahead of the editor: a keystroke
/// stopped here never reaches the platform's text input. Chords bound to
/// actions have already run by now, so what is left with text would type it.
///
/// ponytail: Vim's redo, `ctrl-r`, is missing: on Linux and Windows that is
/// `secondary-r`, Refresh Rows, whose binding takes it before it gets here.
pub(crate) fn key_down(
    workspace: &mut Workspace,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tab = match tab(workspace) {
        None => return,
        Some(tab) => tab,
    };
    match tab.vim {
        Mode::Insert => return,
        Mode::Normal => {}
    }
    let key = match event.keystroke.key_char.as_deref() {
        None => return,
        Some(key) => key,
    };
    // Normal types nothing, whether or not the key means anything to it.
    cx.stop_propagation();
    // Taking the pending keys drops their timer.
    let keys = match tab.vim_pending.take() {
        None => key.to_string(),
        Some(pending) => pending.keys + key,
    };
    match parse(&keys) {
        Parse::Complete(command) => run(command, tab, window, cx),
        Parse::More => {
            tab.vim_pending = Some(Pending {
                keys,
                _timeout: start_timeout(tab.id, cx),
            })
        }
        Parse::Invalid => {}
    }
}

//! Vim's modes in the query buffer: `escape` leaves Insert for Normal, `i`
//! goes back. Normal has the motions and edits the editor already has.

use gpui::{Action, Context, KeyDownEvent, Window};
use gpui_component::input;

use crate::{session::QueryTab, workspace::Workspace};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `pending` is the `g`, `d` etc
    Normal { pending: Option<char> },
    /// Where a buffer starts, so one is typed into as it always was until
    /// `escape` is pressed in it.
    #[default]
    Insert,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Normal { .. } => "NORMAL",
            Self::Insert => "INSERT",
        }
    }
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
        tab.vim = Mode::Normal { pending: None };
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
    let Some(tab) = tab(workspace) else {
        return;
    };
    let pending = match tab.vim {
        Mode::Insert => return,
        Mode::Normal { pending } => pending,
    };
    let Some(text) = event.keystroke.key_char.as_deref() else {
        return;
    };
    // Normal types nothing, whether or not the key means anything to it.
    cx.stop_propagation();
    tab.vim = Mode::Normal { pending: None };
    let action: Box<dyn Action> = match (pending, text) {
        (Some('g'), "g") => Box::new(input::MoveToStart),
        (_, "g") => {
            tab.vim = Mode::Normal {
                pending: text.chars().next(),
            };
            return;
        }
        (_, "i") => {
            tab.vim = Mode::Insert;
            cx.notify();
            return;
        }
        (_, "h") => Box::new(input::MoveLeft),
        (_, "j") => Box::new(input::MoveDown),
        (_, "k") => Box::new(input::MoveUp),
        (_, "l") => Box::new(input::MoveRight),
        (_, "w") => Box::new(input::MoveToNextWord),
        (_, "b") => Box::new(input::MoveToPreviousWord),
        (_, "0") => Box::new(input::MoveToStartOfLine),
        (_, "$") => Box::new(input::MoveToEndOfLine),
        (_, "G") => Box::new(input::MoveToEnd),
        (_, "x") => Box::new(input::Delete),
        (_, "D") => Box::new(input::DeleteToEndOfLine),
        (_, "p") => Box::new(input::Paste),
        (_, "u") => Box::new(input::Undo),
        _ => return,
    };
    window.dispatch_action(action, cx);
}

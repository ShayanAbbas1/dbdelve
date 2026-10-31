//! Vim's modes in the query buffer: `escape` leaves Insert for Normal, `i`
//! goes back. Nothing else of Vim's yet; Normal only keeps typing out of the
//! buffer.

use gpui::{Context, KeyDownEvent, Window};
use gpui_component::input;

use crate::workspace::Workspace;

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

fn mode(workspace: &mut Workspace) -> Option<&mut Mode> {
    let tab = workspace.profile_mut()?.session.active_query_tab_mut()?;
    Some(&mut tab.vim)
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
    if let Some(mode) = mode(workspace) {
        *mode = Mode::Normal;
        cx.notify();
    }
}

/// Bound on the buffer's capture phase, ahead of the editor: a keystroke
/// stopped here never reaches the platform's text input. Chords bound to
/// actions have already run by now, so what is left with text would type it.
pub(crate) fn key_down(
    workspace: &mut Workspace,
    event: &KeyDownEvent,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(mode) = mode(workspace) else {
        return;
    };
    match *mode {
        // Continue Typing as usual
        Mode::Insert => {}
        // Normal types nothing: every keystroke with text stops here. Except 'i'
        Mode::Normal => match event.keystroke.key_char.as_deref() {
            None => {}
            Some("i") => {
                *mode = Mode::Insert;
                cx.notify();
                cx.stop_propagation();
            }
            Some(_) => cx.stop_propagation(),
        },
    }
}

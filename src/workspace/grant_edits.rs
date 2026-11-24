//! The Show Grants dialog's Edit mode: cells that stage a grant or a revoke
//! for the role and object of their row. Nothing staged is sent from here:
//! the changes go to the apply review whole, and run only from its Run.

use super::*;

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

    /// Drop every staged change; Edit mode stays on.
    pub(crate) fn discard_grant_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.grants_view.as_mut() {
            view.staged.clear();
            view.refusal = None;
            cx.notify();
        }
    }
}

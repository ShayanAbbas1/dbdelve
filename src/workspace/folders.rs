//! The explorer's folders and pins: shortcuts to its rows, kept per
//! connection on its profile.

use super::*;
use crate::explorer::{Container, Landing, Member, ObjectRef, Pin, ShortcutDatabase, folder_row};
use crate::session::{PendingDelete, routine_name};

/// What a sidebar row carries while it is dragged.
pub(crate) struct DragShortcut {
    /// A drag outlives a connection switch, so the drop checks it lands on
    /// the profile it was picked up from.
    pub(crate) profile: String,
    pub(crate) pin: Pin,
    /// The folder or the Pinned group it is dragged out of, if either.
    pub(crate) from: Option<Container>,
    pub(crate) label: gpui::SharedString,
}

/// The row's name, following the pointer. Static: anything that moves on its
/// own is the redraw hazard in `AGENTS.md`.
pub(crate) struct DragLabel {
    pub(crate) label: gpui::SharedString,
    /// Where the pointer took hold of the row, from its corner.
    pub(crate) grab: gpui::Point<gpui::Pixels>,
}

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        // gpui draws a preview where the row's corner would be, so one taken
        // from the far side of a wide row starts off the window's left edge.
        // Padded by the grab, it sits just past the pointer instead.
        let label = div()
            .px(px(layout::SPACE_SM))
            .py(px(layout::SPACE_XS))
            .rounded(px(layout::RADIUS_CONTROL))
            .border_1()
            .border_color(t.border)
            .bg(t.overlay)
            .text_color(t.text)
            .font_family(fonts(cx).chrome.clone())
            .text_size(px(layout::chrome(layout::TEXT_MD)))
            .whitespace_nowrap()
            .child(self.label.clone());
        div()
            .pl(self.grab.x + px(layout::SPACE_MD))
            .pt(self.grab.y + px(layout::SPACE_MD))
            .child(label)
    }
}

impl Workspace {
    /// What a folder or a pin names the object a sidebar row opens by. None
    /// while the database it is in is not known.
    pub(crate) fn member_at(&self, target: ExplorerTarget) -> Option<Member> {
        let database = match self.profile()?.shortcut_database() {
            ShortcutDatabase::Unnamed => None,
            ShortcutDatabase::On(database) => Some(database),
            ShortcutDatabase::Unknown => return None,
        };
        let catalog = self.catalog()?;
        let (schema, name, kind) = match target {
            ExplorerTarget::Relation {
                schema_index,
                relation_index,
            } => {
                let schema = catalog.schemas.get(schema_index)?;
                let relation = schema.relations.get(relation_index)?;
                (
                    schema,
                    relation.name.clone(),
                    ObjectKind::Relation(relation.kind),
                )
            }
            ExplorerTarget::Routine {
                schema_index,
                routine_index,
            } => {
                let schema = catalog.schemas.get(schema_index)?;
                let routine = schema.routines.get(routine_index)?;
                (
                    schema,
                    routine_name(routine),
                    ObjectKind::Routine(routine.kind),
                )
            }
        };
        Some(Member::Object(ObjectRef {
            database,
            schema: schema.name.clone(),
            name,
            kind,
        }))
    }

    pub(crate) fn toggle_in_folder(
        &mut self,
        folder: String,
        member: Member,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.shortcuts.toggle(&folder, member);
        self.shortcuts_changed(cx);
    }

    pub(crate) fn toggle_pin(&mut self, pin: Pin, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.shortcuts.toggle_pin(pin);
        self.shortcuts_changed(cx);
    }

    /// A folder under a name no other has, holding `member` when one is
    /// given, with its row turned into the field that names it. Made first and
    /// named after, so the field has a row to sit on.
    pub(crate) fn new_folder(
        &mut self,
        member: Option<Member>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let name = profile.shortcuts.unused_name();
        if profile.shortcuts.add_folder(&name).is_err() {
            return;
        }
        if let Some(member) = member {
            profile.shortcuts.toggle(&name, member);
        }
        // A filter would leave the new folder out, and a field focused where
        // nothing draws it takes the keyboard with it.
        let filter = profile.session.explorer_filter.clone();
        let tree = profile.session.explorer_tree.clone();
        if !filter.read(cx).value().is_empty() {
            filter.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.shortcuts_changed(cx);
        let row = gpui::SharedString::from(folder_row(&name));
        tree.update(cx, |tree, cx| {
            tree.reveal_item(&row, gpui::ScrollStrategy::Center, cx);
        });
        self.start_rename(name, true, row, window, cx);
    }

    pub(crate) fn commit_folder_rename(
        &mut self,
        previous: String,
        typed: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if let Err(message) = profile.shortcuts.rename_folder(&previous, &typed) {
            if let Some(rename) = profile.session.query_rename.as_mut() {
                rename.error = Some(message);
            }
            cx.notify();
            return;
        }
        self.end_query_rename(cx);
        self.shortcuts_changed(cx);
    }

    /// Deleting a folder loses only the grouping, so one holding nothing goes
    /// without asking.
    pub(crate) fn ask_delete_folder(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let held = profile
            .shortcuts
            .folder(&name)
            .map_or(0, |folder| folder.members.len());
        if held == 0 {
            self.delete_folder(name, cx);
            return;
        }
        profile.session.pending_delete = Some(PendingDelete::Folder(name, held));
        cx.notify();
    }

    pub(crate) fn delete_folder(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let asked = profile.session.pending_delete.take().is_some();
        profile.shortcuts.delete_folder(&name);
        self.shortcuts_changed(cx);
        if asked {
            self.refocus_front();
        }
    }

    pub(crate) fn drop_shortcut(
        &mut self,
        dragged: &DragShortcut,
        landing: Landing,
        cx: &mut Context<Self>,
    ) {
        self.explorer_drop = None;
        let Some(profile) = self
            .profile_mut()
            .filter(|profile| profile.id == dragged.profile)
        else {
            cx.notify();
            return;
        };
        let before = profile.shortcuts.clone();
        profile
            .shortcuts
            .drop_on(&dragged.pin, dragged.from.as_ref(), &landing);
        match profile.shortcuts == before {
            true => cx.notify(),
            false => self.shortcuts_changed(cx),
        }
    }

    /// `row` is under a dragged shortcut that would land beside it, after it
    /// when `after`; `None` once it is not, which clears only its own line.
    pub(crate) fn hover_drop(
        &mut self,
        row: &gpui::SharedString,
        after: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        let spot = match after {
            Some(after) => Some((row.clone(), after)),
            None if self.explorer_drop.as_ref().is_some_and(|(at, _)| at == row) => None,
            None => return,
        };
        if self.explorer_drop != spot {
            self.explorer_drop = spot;
            cx.notify();
        }
    }

    fn shortcuts_changed(&mut self, cx: &mut Context<Self>) {
        self.remember_profiles(cx);
        if let Some(id) = self.profile().map(|profile| profile.id.clone()) {
            self.refresh_query_library(&id, cx);
        }
        cx.notify();
    }
}

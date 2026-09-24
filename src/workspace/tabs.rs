//! The tab strip, the theme, and what a tab asks before it closes.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

impl Workspace {
    pub(crate) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_hidden = !self.sidebar_hidden;
        // A folded sidebar takes the open switcher panel with it: the panel is
        // anchored to a row that is no longer on screen.
        self.switcher_open = false;
        cx.notify();
    }

    pub(crate) fn toggle_row_panel(
        &mut self,
        _: &ToggleRowPanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // With no panel on screen, flipping the flag anyway would open the
        // next selection already folded.
        if !self.row_panel.on_screen.get() {
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        match profile.session.active {
            Tab::Query(id) => {
                if let Some(tab) = profile.session.query_tab_mut(id) {
                    tab.row_panel_folded = !tab.row_panel_folded;
                }
            }
            Tab::Object(id) => {
                if let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    && let ObjectBody::Relation {
                        row_panel_folded, ..
                    } = &mut tab.body
                {
                    *row_panel_folded = !*row_panel_folded;
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(session) = self.profile().map(|profile| &profile.session) else {
            return;
        };
        // The chip row draws unsaved buffers, then saved queries, then object
        // tabs, so cycling walks them in that order.
        let tabs: Vec<Tab> = session
            .queries
            .iter()
            .filter(|query| query.open_query.is_none())
            .map(|tab| Tab::Query(tab.id))
            .chain(
                session
                    .saved_queries
                    .iter()
                    .filter_map(|name| session.tab_holding(name))
                    .map(Tab::Query),
            )
            .chain(session.objects.iter().map(|tab| Tab::Object(tab.id)))
            .collect();
        if tabs.len() < 2 {
            return;
        }
        let Some(index) = tabs.iter().position(|tab| *tab == session.active) else {
            return;
        };
        let next = tabs[(index as isize + step).rem_euclid(tabs.len() as isize) as usize];
        self.activate_tab(next, cx);
    }

    pub(crate) fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, cx);
    }

    pub(crate) fn previous_tab(&mut self, _: &PreviousTab, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, cx);
    }

    /// Swap to the next registered theme. Every colour dbdelve paints is read
    /// from the global at render time, so repainting is the whole change — and
    /// side-by-side comparison is the only honest way to pick between palettes.
    pub(crate) fn cycle_theme(
        &mut self,
        _: &CycleTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(theme(cx).next(), window, cx);
    }

    /// Written through to disk, so the palette a person picked is the one the
    /// next launch paints.
    pub(crate) fn set_theme(&mut self, theme: Theme, window: &mut Window, cx: &mut Context<Self>) {
        // `set_editor_zoom` and `set_preview_rows` both skip the write-through
        // when nothing changed; picking the theme already installed should not
        // rewrite `profiles.toml` or re-post the notice either.
        if theme.name == theme::theme(cx).name {
            return;
        }
        install_theme(theme.with_opacity(self.settings.opacity), window, cx);
        // The titlebar deliberately no longer names the theme -- permanent
        // chrome should not narrate a setting -- so the switch itself says
        // where it landed.
        if self.profile().is_some() {
            self.note(format!("Theme: {}", theme.name), cx);
        }
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    /// Written through to disk like the zoom, and reinstalled rather than
    /// merely notified: `install_theme` snapshots the derived colours into
    /// gpui-component's globals, so a repaint alone would leave the library's
    /// half of the window on the old alpha.
    pub(crate) fn set_opacity(
        &mut self,
        opacity: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The field is rewritten before the early return, not after it: typing
        // 120 against a window already at the ceiling changes nothing, and the
        // number we refused would otherwise stay on screen as if it had taken.
        let percent = opacity_percent(opacity).to_string();
        self.opacity_input
            .update(cx, |input, cx| input.set_value(percent, window, cx));
        if self.settings.opacity == opacity {
            return;
        }
        self.settings.opacity = opacity;
        install_theme(theme::theme(cx).with_opacity(opacity), window, cx);
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    /// What the typed percentage means, once the user is done typing it. Both
    /// the way out of the field -- enter and blur -- come through here.
    pub(crate) fn commit_opacity_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.opacity_input.read(cx).value();
        let opacity = opacity_from_percent_input(&typed, self.settings.opacity);
        self.set_opacity(opacity, window, cx);
    }

    /// A step is a step from what the field says, not from whatever was last
    /// committed: the step buttons do not take focus, so typing 79 and
    /// pressing + never blurs the field, and stepping the committed 72 would
    /// land on 77 and drop the 79 on the way.
    pub(crate) fn step_opacity(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_opacity_input(window, cx);
        let stepped = adjusted_opacity(self.settings.opacity, delta);
        self.set_opacity(stepped, window, cx);
    }

    /// Return to the editor, backing out of whatever is in front of it.
    pub(crate) fn show_editor(
        &mut self,
        _: &ShowEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.form.is_some() && !self.profiles.is_empty() {
            self.form = None;
            cx.notify();
            return;
        }
        // Whatever is in front, in the order it is stacked: the palette paints
        // over the settings modal, which paints over the discard-close
        // confirmation, which paints over the close confirmation, which paints
        // over the new-row form, which paints over the apply review, which
        // paints over the surface -- so `escape`
        // backs out of them in that order, one at a time. The palette stays
        // first so that picking a font from inside settings closes the font
        // list and leaves the modal it was opened from standing.
        if self.close_palette(cx) {
            return;
        }
        if self.close_settings(window, cx) {
            return;
        }
        if self.cancel_stale_edit(cx) {
            return;
        }
        if self.cancel_discard_close(cx) {
            return;
        }
        if self.cancel_close_tab(cx) {
            return;
        }
        // Before the apply review, because the form paints over it: generating
        // from the form is what puts a review up, so the form is the newer of
        // the two whenever both exist.
        if self.close_new_row(cx) {
            return;
        }
        if self.close_apply_review(cx) {
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.naming {
            profile.session.naming = false;
            profile.session.editor_needs_focus = true;
            cx.notify();
            return;
        }
        if matches!(profile.session.active, Tab::Query(_)) {
            // Nothing of dbdelve's is stacked over the editor, so the keystroke
            // is not ours. Handing it on is what lets the completion popup --
            // which is the input's, not dbdelve's -- close on `escape`; this
            // binding is unscoped and would otherwise win it at every depth.
            cx.propagate();
            return;
        }
        let Some(&QueryTab { id, .. }) = profile.session.queries.first() else {
            return;
        };
        profile.session.active = Tab::Query(id);
        profile.session.editor_needs_focus = true;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// `tab` takes the highlighted suggestion, and indents when there is none
    /// to take.
    pub(crate) fn accept_completion(
        &mut self,
        _: &AcceptCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .profile()
            .and_then(|profile| profile.session.editor(profile.session.active))
        else {
            return;
        };
        // Through the mode trait: 0.6.4 made the inherent method `pub(crate)`,
        // and this is the only public way left to offer the menu an action.
        let accepted = editor.update(cx, |editor, cx| {
            EditorMode::handle_context_menu_action(
                editor,
                Box::new(Enter {
                    secondary: false,
                    shift: false,
                }),
                window,
                cx,
            )
        });
        if !accepted {
            window.dispatch_action(Box::new(IndentInline), cx);
        }
    }

    /// `cmd+w` on whatever surface is in front.
    ///
    /// An object tab closes: it is a view onto something the database still
    /// holds, and reopening it costs a click. A saved query is a file, and
    /// closing its tab is deleting that file — the strip has no room for a
    /// query that exists but is not listed — so that one asks first. The
    /// scratch buffer has no closed state at all and is left alone.
    pub(crate) fn close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        // The palette is over the tab and holds the keyboard: a stroke that
        // reached here through it would close a tab nobody was looking at.
        if self.palette.is_some() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let session = &profile.session;
        let unsaved = session
            .queries
            .iter()
            .filter(|tab| tab.open_query.is_none())
            .count();
        let Some(target) = close_target(session.active, session.open_query(), unsaved) else {
            return;
        };
        self.ask_before_close(target, cx);
    }

    /// Close a tab, asking first if it holds cell edits nobody has applied.
    ///
    /// Every gesture that takes a tab away comes through here -- `cmd+w`, the
    /// chip's own close button, the palette -- because the edits are lost the
    /// same way whichever one it was, and a guard on one path is a guard on
    /// none.
    pub(crate) fn ask_before_close(&mut self, target: CloseTarget, cx: &mut Context<Self>) {
        let unapplied = self.profile().is_some_and(|profile| {
            target
                .tab(&profile.session)
                .and_then(|tab| profile.session.results(tab))
                .is_some_and(|results| results.read(cx).delegate().has_pending())
        });
        if !unapplied {
            self.close_now(target, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_discard = Some(target);
        }
        cx.notify();
    }

    /// Carry out a close that has been decided on. A saved query asks its own
    /// question from here: closing its tab deletes its file.
    pub(crate) fn close_now(&mut self, target: CloseTarget, cx: &mut Context<Self>) {
        match target {
            CloseTarget::Object(id) => self.close_object(id, cx),
            CloseTarget::Buffer(id) => self.close_buffer(id, cx),
            CloseTarget::SavedQuery(name) => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.pending_close = Some(name);
                }
                cx.notify();
            }
        }
    }

    /// Close the tab the discard prompt was raised over, edits and all.
    pub(crate) fn confirm_discard_close(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return;
        };
        self.close_now(target, cx);
    }

    pub(crate) fn cancel_discard_close(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
            .is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    pub(crate) fn cancel_close_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_close.take())
            .is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }
}

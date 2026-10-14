//! The tab strip, the theme, and what a tab asks before it closes.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::tab_drag::{self, Drag, Slot};

/// How tall an Edit mode input grows before it scrolls instead.
const ROW_INPUT_MAX_LINES: usize = 8;

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
        if let Some((folded, _, _)) = self.active_row_panel() {
            *folded = !*folded;
        }
        cx.notify();
    }

    /// Read ↔ Edit. Turning Edit on asks for the mode an edit needs, as a
    /// grid edit does, rather than showing inputs that would refuse every
    /// value typed into them. Turning it off takes the inputs away, and
    /// `sync_row_panel_inputs` stages what the one being typed in holds.
    pub(crate) fn toggle_row_panel_edit(&mut self, cx: &mut Context<Self>) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(_, editing, _)| *editing);
        if !editing && !self.require(Mode::ReadWrite, cx) {
            return;
        }
        if let Some((folded, editing, _)) = self.active_row_panel() {
            *editing = !*editing;
            if *editing {
                *folded = false;
            }
            self.row_panel.focus_input = *editing;
        }
        cx.notify();
    }

    /// The keystroke for the pencil. It turns Edit on only where the pencil
    /// is drawn, a row with something in it to edit, but always turns it off,
    /// so a tab whose rows stopped being editable is not left stuck in it.
    pub(crate) fn toggle_row_edit(
        &mut self,
        _: &ToggleRowEdit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(_, editing, _)| *editing);
        let editable = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .is_some_and(|results| {
                let table = results.read(cx);
                table
                    .selected_row()
                    .is_some_and(|row| table.delegate().row_editable(row))
            });
        if self.row_panel.on_screen.get() && (editing || editable) {
            self.toggle_row_panel_edit(cx);
        }
    }

    /// The tab in front's row-panel state: whether it is folded, whether it
    /// is editing, and what it is filtered by. `None` for a tab with no rows
    /// to inspect.
    pub(crate) fn active_row_panel(
        &mut self,
    ) -> Option<(&mut bool, &mut bool, &mut gpui::SharedString)> {
        let session = &mut self.profile_mut()?.session;
        match session.active {
            Tab::Query(id) => session.query_tab_mut(id).map(|tab| {
                (
                    &mut tab.row_panel_folded,
                    &mut tab.row_panel_editing,
                    &mut tab.row_panel_filter,
                )
            }),
            Tab::Object(id) => match &mut session.objects.iter_mut().find(|tab| tab.id == id)?.body
            {
                ObjectBody::Relation {
                    row_panel_folded,
                    row_panel_editing,
                    row_panel_filter,
                    ..
                } => Some((row_panel_folded, row_panel_editing, row_panel_filter)),
                _ => None,
            },
        }
    }

    /// Give Edit mode an input for every editable field of the selected row,
    /// from render for the window an input needs. Built for the grid and row
    /// in front, and built again when either changes; an input already there
    /// is refilled when its cell changed under it -- an edit in the grid, a
    /// discard, a refresh -- unless it is the one being typed in.
    pub(crate) fn sync_row_panel_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(folded, editing, _)| *editing && !*folded);
        let target = editing
            .then(|| {
                let results = self.profile()?.session.active_results()?.clone();
                let row = results.read(cx).selected_row()?;
                Some((results, row))
            })
            .flatten();
        if self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            !target
                .as_ref()
                .is_some_and(|(results, row)| inputs.name(results, *row, cx))
        }) {
            self.drop_row_inputs(window, cx);
        }
        self.row_panel.typing = None;
        let Some((results, row)) = target else {
            return;
        };
        let rows = results.read(cx).delegate().rows_id();
        let inputs = self
            .row_panel
            .inputs
            .get_or_insert_with(|| views::RowInputs {
                grid: results.clone(),
                row,
                rows,
                fields: Vec::new(),
            });
        // Only the cells that need an input made or refilled are copied out,
        // since this runs every frame and a value can be a whole document.
        let changed: Vec<(usize, Option<gpui::SharedString>)> = {
            let grid = results.read(cx).delegate();
            inputs.fields.resize_with(grid.columns().len(), || None);
            inputs
                .fields
                .iter()
                .enumerate()
                .filter_map(|(col, field)| {
                    if !grid.editable(row, col) {
                        return field.is_some().then_some((col, None));
                    }
                    let seed = grid.edit_seed(row, col);
                    (field.as_ref().map(|field| field.seed.as_ref()) != Some(seed))
                        .then(|| (col, Some(gpui::SharedString::from(seed.to_string()))))
                })
                .collect()
        };
        for (col, seed) in changed {
            let slot = &mut inputs.fields[col];
            match (slot.as_mut(), seed) {
                (_, None) => {
                    if slot
                        .as_ref()
                        .is_some_and(|field| field.input.focus_handle(cx).is_focused(window))
                    {
                        refocus_front_later(window, cx);
                    }
                    *slot = None;
                }
                (Some(field), Some(seed)) => {
                    // Text typed and not staged yet is kept over a change made
                    // elsewhere; anything else follows the cell.
                    let typed = field.input.read(cx).value();
                    if typed != field.seed && typed != seed {
                        continue;
                    }
                    if typed != seed {
                        field
                            .input
                            .update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
                    }
                    field.seed = seed;
                }
                (None, Some(seed)) => {
                    let input = cx.new(|cx| {
                        gpui_component::input::TextareaState::new(window, cx)
                            .auto_grow(1, ROW_INPUT_MAX_LINES)
                            .submit_on_enter(true)
                    });
                    input.update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
                    cx.subscribe(&input, |workspace, input, event: &InputEvent, cx| {
                        // `shift-enter` is a newline in the value, not a commit.
                        if matches!(
                            event,
                            InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur
                        ) {
                            workspace.commit_row_input(&input, cx);
                        }
                    })
                    .detach();
                    *slot = Some(views::RowInput { input, seed });
                }
            }
        }
        self.row_panel.typing = inputs.fields.iter().position(|field| {
            field
                .as_ref()
                .is_some_and(|field| field.input.focus_handle(cx).is_focused(window))
        });
        if std::mem::take(&mut self.row_panel.focus_input) {
            let active = results
                .read(cx)
                .delegate()
                .active()
                .filter(|(at, _)| *at == row)
                .and_then(|(_, col)| inputs.fields.get(col)?.as_ref());
            if let Some(field) = active.or_else(|| inputs.fields.iter().flatten().next()) {
                field.input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
    }

    /// `up` off a field's first line or `down` off its last: stage it and put
    /// the keyboard in the field beside, scrolled into view.
    pub(crate) fn step_row_input(
        &mut self,
        from: &Entity<gpui_component::input::TextareaState>,
        to: Option<(usize, Entity<gpui_component::input::TextareaState>)>,
        scope: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((ix, to)) = to else {
            return;
        };
        cx.stop_propagation();
        self.commit_row_input(from, cx);
        to.update(cx, |input, cx| input.focus(window, cx));
        crate::scroller::scroll_to("row-inspector", scope.to_string(), ix, cx);
    }

    /// Put Edit mode's inputs away, staging first whatever any of them holds
    /// that is not staged yet. No blur is coming to do it: what takes them away
    /// is a button -- Done, the fold, closing the panel -- which does not take
    /// focus, or a click on another row, whose blur gpui dispatches only after
    /// the frame this runs in. Focus goes back to the front for the reason
    /// `searched` gives.
    fn drop_row_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inputs) = self.row_panel.inputs.as_ref() else {
            return;
        };
        let inputs: Vec<_> = inputs
            .fields
            .iter()
            .flatten()
            .map(|field| field.input.clone())
            .collect();
        for input in &inputs {
            self.commit_row_input(input, cx);
        }
        if inputs
            .iter()
            .any(|input| input.focus_handle(cx).is_focused(window))
        {
            refocus_front_later(window, cx);
        }
        self.row_panel.inputs = None;
    }

    /// Edit mode's input on a cell of the grid in front, if one is built.
    pub(crate) fn row_input(
        &self,
        (row, col): (usize, usize),
        cx: &App,
    ) -> Option<Entity<gpui_component::input::TextareaState>> {
        let results = self.profile()?.session.active_results()?;
        let inputs = self
            .row_panel
            .inputs
            .as_ref()
            .filter(|inputs| inputs.name(results, row, cx))?;
        Some(inputs.fields.get(col)?.as_ref()?.input.clone())
    }

    /// Whether one of Edit mode's inputs has the keyboard.
    pub(crate) fn row_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            inputs
                .fields
                .iter()
                .flatten()
                .any(|field| field.input.focus_handle(cx).is_focused(window))
        })
    }

    /// Show the tab in front's filter in the row panel's field, which every
    /// tab shares. From render, before the panel reads the field, for the
    /// reason `sync_page_input` gives: a tab comes to the front in more places
    /// than one.
    pub(crate) fn sync_row_panel_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.row_panel.search.clone();
        let Some((_, _, filter)) = self.active_row_panel() else {
            return;
        };
        let filter = filter.clone();
        if input.read(cx).value() != filter {
            input.update(cx, |state, cx| state.set_value(filter, window, cx));
        }
    }

    pub(crate) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.profile().map(|profile| &profile.session) else {
            return;
        };
        // Cycling walks the strip left to right, wherever the chips were
        // dragged to.
        let tabs = session.strip_tabs();
        if tabs.len() < 2 {
            return;
        }
        let Some(index) = tabs.iter().position(|tab| *tab == session.active) else {
            return;
        };
        let next = tabs[(index as isize + step).rem_euclid(tabs.len() as isize) as usize];
        self.activate_tab(next, window, cx);
    }

    /// A chip has been picked up. The chips are what the strip last drew, so
    /// their bounds are where the drag measures from and not where it draws.
    pub(crate) fn begin_tab_drag(&mut self, key: TabKey, pointer_x: f32, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let order = profile.session.strip_order();
        let bounds = self.tab_strip.bounds.borrow();
        let mut slots: Vec<Slot> = bounds
            .iter()
            .filter(|(key, _)| order.contains(key))
            .map(|(key, bounds)| Slot {
                key: key.clone(),
                left: f32::from(bounds.left()),
                width: f32::from(bounds.size.width),
            })
            .collect();
        drop(bounds);
        slots.sort_by(|a, b| a.left.total_cmp(&b.left));
        let Some(index) = slots.iter().position(|slot| slot.key == key) else {
            return;
        };
        self.tab_strip.drag = Some(Drag {
            grab: pointer_x - slots[index].left,
            key,
            slots,
            index,
            pointer_x,
        });
        cx.notify();
    }

    pub(crate) fn move_tab_drag(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        if let Some(drag) = &mut self.tab_strip.drag {
            drag.pointer_x = pointer_x;
            cx.notify();
        }
    }

    /// Dropped: the strip keeps the order the drag had made room for.
    pub(crate) fn end_tab_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.tab_strip.drag.take() else {
            return;
        };
        let keys: Vec<TabKey> = drag.slots.iter().map(|slot| slot.key.clone()).collect();
        let layout = drag.layout();
        // The strip is about to be laid out in the new order, so each chip is
        // drawn where it was and glides the rest of the way.
        for (slot, moved) in drag.slots.iter().zip(&layout.landing) {
            self.tab_strip.shift.settle_into(&slot.key, *moved);
        }
        let order = tab_drag::reordered(&keys, &drag.key, layout.target);
        if let Some(profile) = self.profile_mut() {
            profile.session.tab_order = order;
        }
        cx.notify();
    }

    pub(crate) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    pub(crate) fn previous_tab(
        &mut self,
        _: &PreviousTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_tab(-1, window, cx);
    }

    pub(crate) fn select_theme(
        &mut self,
        _: &SelectTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Theme, window, cx);
    }

    /// Put back the theme the picker opened over. Every way out of the palette
    /// comes through here, confirming included: the row's theme is then
    /// installed again by `set_theme`, which is the one path that saves it.
    pub(crate) fn end_theme_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The snapshot predates any opacity change made while previewing.
        if let Some(theme) = self.theme_before_preview.take() {
            let opacity = self.settings.opacity_for(&theme);
            install_theme(theme.with_opacity(opacity), window, cx);
            cx.refresh_windows();
        }
    }

    /// Written through to disk, so the palette a person picked is the one the
    /// next launch paints.
    pub(crate) fn set_theme(&mut self, theme: Theme, window: &mut Window, cx: &mut Context<Self>) {
        // `set_font_size` and `set_default_row_limit` both skip the write-through
        // when nothing changed; picking the theme already installed should not
        // rewrite `profiles.toml` or re-post the notice either.
        if theme.name == theme::theme(cx).name {
            return;
        }
        let theme = theme.with_opacity(self.settings.opacity_for(&theme));
        install_theme(theme, window, cx);
        let percent = opacity_percent(theme.opacity).to_string();
        self.opacity_input
            .update(cx, |input, cx| input.set_value(percent, window, cx));
        // The titlebar deliberately no longer names the theme -- permanent
        // chrome should not narrate a setting -- so the switch itself says
        // where it landed.
        if self.profile().is_some() {
            self.note(format!("Theme: {}", theme.name), cx);
        }
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    /// The theme is written into dbdelve's own directory first, so picking it
    /// is the same pick a theme already listed would be.
    pub(crate) fn import_theme(
        &mut self,
        source: theme::user::Source,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match theme::user::import(source, index) {
            // `set_theme` skips a theme already in force by name, and a
            // re-import may have brought it new colours.
            Ok(theme) if theme.name == theme::theme(cx).name => {
                install_theme(
                    theme.with_opacity(self.settings.opacity_for(&theme)),
                    window,
                    cx,
                );
                cx.refresh_windows();
            }
            Ok(theme) => self.set_theme(theme, window, cx),
            Err(message) => self.note(message, cx),
        }
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
        //
        // Both sides are compared in whole percents: a stepped 0.77000004
        // against a typed 0.77 would rewrite `profiles.toml` for nothing.
        let theme = *theme::theme(cx);
        let percent = opacity_percent(opacity.clamp(theme::OPACITY_MIN, theme::OPACITY_MAX));
        self.opacity_input.update(cx, |input, cx| {
            input.set_value(percent.to_string(), window, cx)
        });
        if percent == opacity_percent(self.settings.opacity_for(&theme)) {
            return;
        }
        self.settings.set_opacity_for(&theme, opacity);
        let opacity = self.settings.opacity_for(&theme);
        install_theme(theme.with_opacity(opacity), window, cx);
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    /// What the typed percentage means, once the user is done typing it. Both
    /// the way out of the field -- enter and blur -- come through here.
    pub(crate) fn commit_opacity_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.opacity_input.read(cx).value();
        let theme = theme::theme(cx);
        let current = self.settings.opacity_for(theme);
        let opacity = opacity_from_percent_input(&typed, current);
        self.set_opacity(opacity, window, cx);
    }

    /// A step is a step from what the field says, not from whatever was last
    /// committed: the step buttons do not take focus, so typing 79 and
    /// pressing + never blurs the field, and stepping the committed 72 would
    /// land on 77 and drop the 79 on the way.
    pub(crate) fn step_opacity(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_opacity_input(window, cx);
        let theme = theme::theme(cx);
        let stepped = adjusted_opacity(theme.opacity, delta);
        self.set_opacity(stepped, window, cx);
    }

    /// Back out of whatever dbdelve has in front of the tab, one layer at a time.
    pub(crate) fn show_editor(
        &mut self,
        _: &ShowEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_palette(window, cx) {
            return;
        }
        if self.pending_import.take().is_some() {
            cx.notify();
            return;
        }
        if self.project_name.is_some() {
            self.drop_project_name();
            cx.notify();
            return;
        }
        // Backs out of naming a new project to the list, as its × does,
        // rather than out of everything typed into the form.
        if let Some(form) = &mut self.form
            && form.naming_project
        {
            form.naming_project = false;
            form.needs_focus = Some(form.name.clone());
            cx.notify();
            return;
        }
        if self.form.is_some() {
            self.form = None;
            self.refocus_front();
            cx.notify();
            return;
        }
        // Whatever is in front, in the order it is stacked: the palette paints
        // over the settings modal, which paints over the stale-edit prompt,
        // the stopped-run prompt and the run prompt, which paint over the
        // discard-close confirmation, which paints over the close
        // confirmation, which paints over the new-row form, which paints over
        // the apply review, which paints over the surface -- so `escape`
        // backs out of them in that order, one at a time. The palette stays
        // first so that picking a font from inside settings closes the font
        // list and leaves the modal it was opened from standing.
        if self.close_settings(window, cx) {
            return;
        }
        if self.cancel_stale_edit(cx) {
            return;
        }
        // Gated on what is drawn, not on the state alone: `queue_failure`
        // outlives the queue it was about when a new run replaces it, and an
        // `escape` meant for something else must not stop that new queue.
        if self.render_queue_failure(cx).is_some() {
            self.stop_queue(cx);
            return;
        }
        if self.render_pending_run(cx).is_some() {
            self.cancel_pending_run(cx);
            return;
        }
        if self.cancel_discard_close(cx) {
            return;
        }
        if self.cancel_close_tab(cx) {
            return;
        }
        // Before the apply review, though confirming a row clears the form in
        // the same step that opens the review, so the two are never both open
        // on one tab. `close_new_row` is scoped to the active tab's own form,
        // so one left open elsewhere falls through to the steps below instead
        // of eating this keystroke.
        if self.close_new_row(cx) {
            return;
        }
        if self.close_apply_review(cx) {
            return;
        }
        // Below the modals and prompts, which paint over the switcher.
        if self.switcher_open {
            self.switcher_open = false;
            cx.notify();
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
        // Nothing of dbdelve's is stacked over the tab, so the keystroke is not
        // ours. Handing it on is what lets the completion popup close and a
        // grid cell's input cancel its edit; this binding is unscoped and would
        // otherwise win it at every depth. Never a tab switch, on any tab: that
        // is what used to take the `escape` meant for a cell edit.
        cx.propagate();
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
    pub(crate) fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        // The palette is over the tab and holds the keyboard: a stroke that
        // reached here through it would close a tab nobody was looking at.
        if self.palette.is_some() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let session = &profile.session;
        if session.active_query_tab().is_none() && session.active_object().is_none() {
            return;
        }
        let target = close_target(session.active, session.open_query());
        self.ask_before_close(target, window, cx);
    }

    /// Close a tab, asking first if it holds cell edits nobody has applied.
    ///
    /// Every gesture that takes a tab away comes through here -- `cmd+w`, the
    /// chip's own close button, the palette -- because the edits are lost the
    /// same way whichever one it was, and a guard on one path is a guard on
    /// none.
    pub(crate) fn ask_before_close(
        &mut self,
        target: CloseTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unapplied = self.profile().is_some_and(|profile| {
            target
                .tab(&profile.session)
                .and_then(|tab| profile.session.results(tab))
                .is_some_and(|results| results.read(cx).delegate().has_pending())
        });
        if !unapplied {
            self.close_now(target, window, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_discard = Some(target);
        }
        cx.notify();
    }

    /// Carry out a close that has been decided on. A saved query asks its own
    /// question from here: closing its tab deletes its file.
    ///
    /// Nothing is stopped here: each close stops its tab's statement as the
    /// tab goes, and a saved query's goes only once its delete is confirmed.
    pub(crate) fn close_now(
        &mut self,
        target: CloseTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            CloseTarget::Object(id) => self.close_object(id, window, cx),
            CloseTarget::Buffer(id) => self.close_buffer(id, window, cx),
            CloseTarget::SavedQuery(name) => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.pending_close = Some(name);
                }
                cx.notify();
            }
        }
    }

    /// Ask the server to stop what `tab` is running, if it is running something.
    ///
    /// For a tab on its way out: left running, its statement holds the
    /// connection while nothing is left to show it.
    pub(crate) fn stop_run(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(connection) = self.profile().and_then(Profile::connection) else {
            return;
        };
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some((QueryState::Running { cancel, .. }, _)) = profile.session.slot(tab) else {
            return;
        };
        let mut cancels = vec![cancel.clone()];
        // A queue's statements running at once are each stopped by their own.
        if let Tab::Query(id) = tab
            && let Some(queue) = profile
                .session
                .query_tab(id)
                .and_then(|query| query.queue.as_ref())
        {
            cancels.extend(
                queue
                    .done
                    .iter()
                    .filter_map(|finished| match &finished.state {
                        QueryState::Running { cancel, .. } => Some(cancel.clone()),
                        _ => None,
                    }),
            );
        }
        let cancel_task = cx.background_executor().spawn(on_own_thread(move || {
            cancels
                .iter()
                .map(|cancel| connection.cancel(cancel))
                .fold(Ok(()), Result::and)
        }));
        // Said as `cancel_query` says it: the tab has gone, so a statement
        // still running behind it is all the more worth knowing about.
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    /// Close the tab the discard prompt was raised over, edits and all.
    pub(crate) fn confirm_discard_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return;
        };
        self.close_now(target, window, cx);
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

/// Hand the keyboard back to the tab in front on the next frame, for an input
/// taken away while it had it: an unmounted input keeps the window's focus,
/// and with it every keybinding.
fn refocus_front_later(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, |workspace, _, cx| {
        workspace.refocus_front();
        cx.notify();
    });
}

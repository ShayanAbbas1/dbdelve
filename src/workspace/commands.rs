//! The command palette and the settings panel it can open.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::keybindings;

impl Workspace {
    pub(crate) fn fuzzy_open(
        &mut self,
        _: &FuzzyOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Search(Scope::Objects), window, cx);
    }

    pub(crate) fn go_to_column(
        &mut self,
        _: &GoToColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_results(cx) {
            return;
        }
        self.open_palette(PaletteMode::Columns, window, cx);
    }

    /// Scroll the grid in front to a column and put the ring on it.
    pub(crate) fn reveal_column(
        &mut self,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        let focused = results.update(cx, |table, cx| {
            crate::result_grid::reveal_column(table, col, window, cx)
        });
        // The grid holds the keyboard now. Closing the palette asked for the
        // tab's own focus back, which in a query tab is the editor, and the
        // next render would hand it there, leaving the arrow keys typing.
        if focused && let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = false;
        }
    }

    /// Change how the grid in front lays out its columns, then have the table
    /// rebuild its own copy of them.
    fn relay_columns(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut crate::result_grid::ResultGrid),
    ) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        results.update(cx, |table, cx| {
            change(table.delegate_mut());
            crate::result_grid::relaid(table, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// The column under the ring, which the cell menu acts on.
    fn active_column(&self, cx: &App) -> Option<usize> {
        let results = self.profile()?.session.active_results()?;
        results.read(cx).delegate().active().map(|(_, col)| col)
    }

    pub(crate) fn hide_column(&mut self, _: &HideColumn, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(col) = self.active_column(cx) {
            self.relay_columns(cx, |grid| grid.hide_column(col));
        }
    }

    pub(crate) fn toggle_pin_column(
        &mut self,
        _: &TogglePinColumn,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(col) = self.active_column(cx) {
            self.relay_columns(cx, |grid| grid.toggle_pinned(col));
        }
    }

    pub(crate) fn show_all_columns(
        &mut self,
        _: &ShowAllColumns,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.relay_columns(cx, |grid| grid.show_all_columns());
    }

    pub(crate) fn reveal_column_named(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(col) = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .and_then(|results| results.read(cx).delegate().column_named(name))
        else {
            self.note(format!("The rows on screen have no column {name}."), cx);
            return;
        };
        self.reveal_column(col, window, cx);
    }

    pub(crate) fn command_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Search(Scope::Commands), window, cx);
    }

    pub(crate) fn search_everything(
        &mut self,
        _: &SearchEverything,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Search(Scope::All), window, cx);
    }

    /// Over a palette already up, the stroke does what [`Palette::stroke`]
    /// says.
    pub(crate) fn open_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The palette draws over the import's question, and would let the
        // project it picked be renamed or deleted under it. Its own rows for
        // these are left out meanwhile (`palette::waits_for_import`); this
        // stops the keystrokes and menus that open them directly.
        let names_projects = matches!(
            mode,
            PaletteMode::NameProject(_)
                | PaletteMode::Projects(_)
                | PaletteMode::MoveToProject(_)
                | PaletteMode::ConnectionActions(_)
                | PaletteMode::ProjectActions(_)
                | PaletteMode::Confirm(_)
        );
        if names_projects && self.pending_import.is_some() {
            return;
        }
        let stroke = self
            .palette
            .as_ref()
            .map(|palette| palette.read(cx).stroke(mode));
        if let Some(Stroke::Rescope(scope)) = stroke
            && let Some(palette) = self.palette.clone()
        {
            palette.update(cx, |palette, cx| palette.set_scope(scope, window, cx));
            return;
        }
        // Swapping straight to another list skips `close_palette`, and a
        // theme being previewed must not outlive the list that previews it.
        self.end_theme_preview(window, cx);
        // The theme lists and the engine list are the only ones reachable
        // before any connection exists, from the welcome surface or the
        // connection form.
        let picks_theme = matches!(mode, PaletteMode::Theme | PaletteMode::ImportTheme(_));
        let before_any_connection = picks_theme || mode == PaletteMode::Engine;
        if stroke == Some(Stroke::Close) || (self.profile().is_none() && !before_any_connection) {
            self.close_palette(window, cx);
            return;
        }
        if let PaletteMode::ImportTheme(source) = mode
            && !source.is_ready()
        {
            self.close_palette(window, cx);
            self.open_import_list_once_found(source, window, cx);
            return;
        }

        // A theme list opens on the theme in force where it lists it, so the
        // preview it starts with is no change at all.
        if picks_theme {
            self.theme_before_preview = Some(*theme(cx));
        }
        // On the picker rather than at startup, which most launches never
        // need, and ahead of the import rows it lists.
        if mode == PaletteMode::Theme {
            theme::user::prewarm();
        }
        let first = match mode {
            PaletteMode::Theme => Theme::all()
                .iter()
                .position(|candidate| candidate.name == theme(cx).name),
            PaletteMode::ImportTheme(source) => source
                .found()
                .iter()
                .position(|candidate| candidate.name == theme(cx).name),
            PaletteMode::Schema => self.profile().and_then(crate::palette::active_schema_row),
            PaletteMode::MoveToProject(index) => self.profiles.get(index).and_then(|profile| {
                crate::palette::current_project_row(index, &profile.id, &self.projects)
            }),
            PaletteMode::Database => self.profile().and_then(|profile| {
                let databases = &profile.databases;
                databases
                    .names
                    .iter()
                    .position(|name| Some(name) == databases.current.as_ref())
            }),
            PaletteMode::Engine => crate::palette::active_engine_row(self),
            _ => None,
        };
        // A list put up in place of one opened from the panel, or by a row
        // confirmed there, was opened from the panel too. The theme list's
        // import rows open their list as the theme list closes, while the
        // panel's search waits to be focused.
        let from_settings = self.settings_search_needs_focus
            || self.settings_focus.contains_focused(window, cx)
            || self.palette_from_settings;
        self.palette_from_settings = self.settings_open && from_settings;
        // The palette takes the keyboard, and a capture left listening behind
        // it would take the first key typed after it closes.
        self.cancel_rebind(cx);
        if let Some(current) = self.current_group().map(str::to_string) {
            self.collapsed_projects.remove(&current);
        }
        let handle = cx.weak_entity();
        let palette = cx.new(|cx| Palette::new(mode, self, handle, window, cx));
        if let Some(first) = first {
            palette.update(cx, |palette, cx| palette.select_item(first, window, cx));
        }
        cx.subscribe_in(&palette, window, Self::on_palette_event)
            .detach();
        self.palette = Some(palette);
        cx.notify();
    }

    /// A palette opened before this profile's catalog arrived lists its
    /// objects once they do.
    pub(crate) fn catalog_landed(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some(palette), Some(profile)) = (self.palette.clone(), self.profile()) else {
            return;
        };
        if profile.id == id {
            palette.update(cx, |palette, cx| palette.catalog_changed(profile, cx));
        }
    }

    /// Waiting for a source's scan off the UI thread, with no list up meanwhile
    /// and so no preview left on screen. The note saying so comes down from
    /// wherever `note` put it, if nothing has said anything since.
    fn open_import_list_once_found(
        &mut self,
        source: theme::user::Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let finding = format!("Finding {} themes…", source.name());
        self.note(finding.clone(), cx);
        let found = cx
            .background_executor()
            .spawn(on_own_thread(move || source.found()));
        cx.spawn_in(window, async move |workspace, cx| {
            found.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let notice = match workspace.profile_mut() {
                    Some(profile) => &mut profile.session.notice,
                    None => match &mut workspace.form {
                        Some(form) => &mut form.error,
                        None => &mut workspace.welcome_notice,
                    },
                };
                if notice.as_deref() == Some(finding.as_str()) {
                    *notice = None;
                }
                // Something else opened while the scan ran is left alone.
                if workspace.palette.is_none() {
                    workspace.open_palette(PaletteMode::ImportTheme(source), window, cx);
                }
            });
        })
        .detach();
    }

    /// The palette is dismissed before its command runs, always. A command can
    /// open a tab, a form or a modal, and none of them can come up underneath
    /// an overlay that is still holding the keyboard.
    pub(crate) fn on_palette_event(
        &mut self,
        palette: &Entity<Palette>,
        event: &PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let PaletteEvent::Confirm {
            command: Command::ToggleProject(at),
            ..
        } = event
        {
            self.toggle_project(*at, cx);
            let collapsed = self.collapsed_projects.clone();
            palette.update(cx, |palette, cx| palette.set_collapsed(collapsed, cx));
            return;
        }
        let searching = matches!(palette.read(cx).mode(), PaletteMode::Search(_));
        let (command, scope) = match event {
            PaletteEvent::Dismiss => {
                self.close_palette(window, cx);
                return;
            }
            PaletteEvent::Act { command, scope } => (command.clone(), *scope),
            PaletteEvent::Confirm {
                command,
                scope,
                label,
            } => {
                if searching && palette::remembered(*scope, command) {
                    let recent = (*scope, label.clone());
                    self.palette_recents.retain(|kept| *kept != recent);
                    self.palette_recents.insert(0, recent);
                    self.palette_recents.truncate(palette::RECENTS_KEPT);
                }
                (command.clone(), *scope)
            }
        };
        let from_settings = self.palette_from_settings;
        self.take_palette(true, window, cx);
        // The palette leaves out these rows while an import asks, but this
        // one may have opened before the import began asking.
        if self.pending_import.is_some() && palette::waits_for_import(scope, &command) {
            self.note("Choose where the imported connections go first.".into(), cx);
            return;
        }
        self.palette_from_settings = from_settings;
        self.run_command(command, window, cx);
        // Kept only for a list the command put up in the palette's place.
        if self.palette.is_none() {
            self.palette_from_settings = false;
        }
    }

    /// Take the palette down and hand the keyboard back.
    ///
    /// Handing it back is the whole job. The palette's search field is what had
    /// focus, and it goes with the palette — leaving the window focused on
    /// nothing, with no dispatch path, and every binding dead until something
    /// is clicked. Including the one that would reopen the palette.
    ///
    /// Every way out routes through here for that reason: `escape`, the same
    /// stroke again, a click outside, and confirming a row.
    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.take_palette(false, window, cx)
    }

    /// A palette opened from the settings panel hands the keyboard back to
    /// its search when it is dismissed, and when a setting is picked from it.
    /// Any other row confirmed there puts focus where its command wants it --
    /// a table opened, a new query tab -- and the panel taking it back would
    /// leave that dead.
    fn take_palette(
        &mut self,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(palette) = self.palette.take() else {
            return false;
        };
        let picks_setting = matches!(
            palette.read(cx).mode(),
            PaletteMode::Theme | PaletteMode::ImportTheme(_) | PaletteMode::Font(_)
        );
        self.end_theme_preview(window, cx);
        let to_settings = std::mem::take(&mut self.palette_from_settings)
            && self.settings_open
            && (picks_setting || !confirmed);
        if let Some(form) = self.form.as_mut() {
            form.needs_focus = Some(form.lead_field());
        } else if to_settings {
            self.settings_search_needs_focus = true;
        } else {
            self.refocus_front();
        }
        cx.notify();
        true
    }

    /// `cmd+,` and the titlebar's gear: open settings, or put them away.
    pub(crate) fn open_settings(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.close_settings(window, cx) {
            self.show_settings(window, cx);
        }
    }

    pub(crate) fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open {
            // Seeded on the way in rather than at startup: the opacity is
            // restored from disk well after the workspace is built, and this
            // is the only moment the field is about to be looked at.
            let percent = opacity_percent(theme::theme(cx).opacity).to_string();
            self.opacity_input
                .update(cx, |input, cx| input.set_value(percent, window, cx));
            self.settings_open = true;
        }
        self.settings_search_needs_focus = true;
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.settings_open {
            return false;
        }
        // Left alone when the panel never had it: `cmd+,` from the explorer's
        // filter puts the panel away without moving the caret out of it.
        if self.settings_focus.contains_focused(window, cx) {
            self.refocus_front();
        }
        self.drop_settings(window, cx);
        cx.notify();
        true
    }

    /// Takes the panel down without handing the keyboard anywhere, for a
    /// caller putting something else in front that takes it.
    pub(crate) fn drop_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Backing out of the panel is not a way to set the opacity: the field
        // leaves the tree on the next frame, which blurs it, and a blur
        // commits. Put the live value back first so that commit is a no-op --
        // otherwise escaping out of a half-typed `5` on the way to `50` leaves
        // the window at 5 percent's clamp.
        let percent = opacity_percent(theme::theme(cx).opacity).to_string();
        self.opacity_input
            .update(cx, |input, cx| input.set_value(percent, window, cx));
        self.settings_open = false;
        self.settings_search_needs_focus = false;
        self.rebinding = None;
        if matches!(self.custom_limit, Some((LimitTarget::Default, _))) {
            self.custom_limit = None;
        }
        self.clear_settings_search(window, cx);
    }

    pub(crate) fn set_settings_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_tab = tab;
        self.rebinding = None;
        self.clear_settings_search(window, cx);
        crate::scroller::scroll_to("settings-body", "", 0, cx);
        cx.notify();
    }

    /// Empties the settings search, saying whether there was anything in it.
    pub(crate) fn clear_settings_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let typed = !self.settings_search.read(cx).value().is_empty();
        if typed {
            // `set_value` does not report a change, so the matches go by hand.
            self.settings_search
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.settings_matches = None;
            crate::scroller::scroll_to("settings-body", "", 0, cx);
            cx.notify();
        }
        typed
    }

    /// Filters the rows once per keystroke, never per frame. A capture in
    /// progress is called off: its row may be about to be filtered away.
    pub(crate) fn search_settings(&mut self, cx: &mut Context<Self>) {
        self.refilter_settings(cx);
        self.rebinding = None;
        crate::scroller::scroll_to("settings-body", "", 0, cx);
        cx.notify();
    }

    /// Run again whenever a chord changes: the search matches the chords bound
    /// today, not the ones bound when it was typed.
    fn refilter_settings(&mut self, cx: &mut Context<Self>) {
        let typed = self.settings_search.read(cx).value();
        self.settings_matches = (!typed.trim().is_empty())
            .then(|| filter_settings(&typed, &self.settings.custom_keybindings));
    }

    /// Put a row into "press a key" mode. Only one at a time -- starting a
    /// new capture always replaces whatever was mid-capture before it. The
    /// keystroke itself arrives at the interceptor `Workspace::new` installs,
    /// not here.
    ///
    /// The panel takes focus: a click on Edit leaves it wherever it was, and
    /// the interceptor reads focus outside the panel as a click elsewhere.
    pub(crate) fn start_rebind(
        &mut self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebinding = Some(id);
        self.settings_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn cancel_rebind(&mut self, cx: &mut Context<Self>) {
        if self.rebinding.take().is_some() {
            cx.notify();
        }
    }

    /// The keystroke captured for `id`. Refuses on conflict rather than
    /// stealing the chord from whoever already has it, per the Keybindings
    /// tab's whole premise: `keybindings::conflict` is the single source of
    /// truth both here and in the row that shows what's currently bound.
    pub(crate) fn apply_rebind(&mut self, id: &'static str, chord: String, cx: &mut Context<Self>) {
        self.rebinding = None;
        let context = keybindings::REGISTRY
            .iter()
            .find(|spec| spec.id == id)
            .and_then(|spec| spec.context);
        if let Some(owner) =
            keybindings::conflict(&chord, context, id, &self.settings.custom_keybindings)
        {
            self.note(format!("\"{chord}\" is already bound to {owner}."), cx);
            return;
        }
        self.settings
            .custom_keybindings
            .insert(id.to_string(), chord.clone());
        self.refilter_settings(cx);
        self.remember_profiles(cx);
        self.note(
            format!("Bound to {chord}. Restart dbdelve for it to take effect."),
            cx,
        );
        cx.notify();
    }

    /// Drop `id`'s override and fall back to its shipped default(s).
    pub(crate) fn reset_keybinding(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.settings.custom_keybindings.remove(id).is_none() {
            return;
        }
        self.refilter_settings(cx);
        self.remember_profiles(cx);
        self.note(
            "Reset to default. Restart dbdelve for it to take effect.".into(),
            cx,
        );
        cx.notify();
    }

    /// Every row runs through the method its button or keystroke already calls.
    /// The palette is another way in, never a second implementation.
    pub(crate) fn run_command(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            Command::OpenObject(target) => self.open_explorer_target(target, window, cx),
            Command::OpenQuery(name) => self.open_saved_query(name, window, cx),
            Command::OpenScratch => self.open_scratch_query(window, cx),
            Command::NewQuery => self.new_query(&NewQuery, window, cx),
            Command::RunQuery => self.run_query(&RunQuery, window, cx),
            Command::ExplainQuery(mode) => self.explain_query(&ExplainQuery { mode }, window, cx),
            Command::FormatQuery => self.format_query(&FormatQuery, window, cx),
            Command::ShowPlan(showing) => self.show_plan(showing, cx),
            Command::SaveQuery => self.save_query(&SaveQuery, window, cx),
            Command::RenameQuery => self.rename_query(window, cx),
            Command::QueryHistory => {
                self.open_palette(PaletteMode::Search(Scope::History), window, cx)
            }
            Command::RecallStatement(sql) => self.recall_statement(sql, window, cx),
            Command::ShowStructure(structure) => self.show_structure(structure, window, cx),
            Command::RefreshRelation(id) => self.refresh_relation(id, cx),
            Command::CountRows(id) => self.count_rows(id, cx),
            Command::NextPage => self.turn_page(true, cx),
            Command::PreviousPage => self.turn_page(false, cx),
            Command::FilterRows => self.focus_filter(window, cx),
            Command::ClearFilter => self.clear_filter(&ClearFilter, window, cx),
            Command::NewRow => self.new_row(&NewRow, window, cx),
            Command::ImportRows => self.import_rows(&ImportRows, window, cx),
            Command::CloseObject(id) => self.ask_before_close(CloseTarget::Object(id), window, cx),
            Command::SetNull => self.set_null(&SetNull, window, cx),
            Command::SetEmpty => self.set_empty(&SetEmpty, window, cx),
            Command::SetDefault => self.set_default(&SetDefault, window, cx),
            Command::DeleteRow => self.delete_row(&DeleteRow, window, cx),
            Command::ApplyEdits => self.apply_edits(&ApplyEdits, window, cx),
            Command::DiscardEdits => self.discard_edits(&DiscardEdits, window, cx),
            Command::NextEdit => self.next_edit(&NextEdit, window, cx),
            Command::PreviousEdit => self.previous_edit(&PreviousEdit, window, cx),
            Command::ExportResults(format) => self.export_results(format, cx),
            Command::CopyRow => self.copy_row(&CopyRow, window, cx),
            Command::CopyResults(format) => self.copy_results_as(format, cx),
            Command::SwitchProfile(index) => self.activate(index, cx),
            Command::SwitchConnection => self.open_connections(window, cx),
            Command::ConnectionActions(index) => {
                self.open_palette(PaletteMode::ConnectionActions(index), window, cx)
            }
            Command::ToggleProject(at) => self.toggle_project(at, cx),
            Command::ProjectActions(at) => {
                self.open_palette(PaletteMode::ProjectActions(at), window, cx)
            }
            Command::EditConnection(index) => self.edit_profile(index, window, cx),
            Command::DuplicateConnection(index) => self.duplicate_profile(index, window, cx),
            Command::ChooseProject(index) => {
                self.open_palette(PaletteMode::MoveToProject(index), window, cx)
            }
            Command::MoveToProject(index, project) => {
                self.move_to_project(index, project.as_deref(), cx)
            }
            Command::Confirm(target) => self.open_palette(PaletteMode::Confirm(target), window, cx),
            Command::RemoveConnection(index) => self.remove_profile(index, cx),
            Command::NewConnectionIn(project) => self.new_connection_in(project, window, cx),
            Command::PickProject(verb) => {
                self.open_palette(PaletteMode::Projects(verb), window, cx)
            }
            Command::NameProject(at) => self.open_palette(PaletteMode::NameProject(at), window, cx),
            Command::CreateProject(name) => self.create_project(name, cx),
            Command::RenameProject(old, name) => self.rename_project(&old, name, cx),
            Command::DeleteProject(name) => self.remove_project(&name, cx),
            Command::NextProfile => self.cycle_profile(1, cx),
            Command::PreviousProfile => self.cycle_profile(-1, cx),
            Command::NewConnection => self.open_connection_form(&NewConnection, window, cx),
            Command::NewProject => self.new_project(&NewProject, window, cx),
            Command::ImportConnections(source) => self.import_connections(source, window, cx),
            Command::RefreshConnection => self.reconnect(self.active, cx),
            Command::SelectTheme => self.select_theme(&SelectTheme, window, cx),
            Command::SelectDatabase => self.select_database(&SelectDatabase, window, cx),
            Command::SetDatabase(name) => self.set_database(name, window, cx),
            Command::SetEngine(engine) => {
                if let Some(form) = &mut self.form {
                    form.set_engine(engine);
                }
                cx.notify();
            }
            Command::SelectSchema => self.select_schema(window, cx),
            Command::SetSchema(name) => self.set_schema(name, cx),
            Command::SetTheme(theme) => self.set_theme(*theme, window, cx),
            Command::BrowseThemes(source) => {
                self.open_palette(PaletteMode::ImportTheme(source), window, cx)
            }
            Command::ImportTheme(source, index) => self.import_theme(source, index, window, cx),
            Command::PickFont(slot) => self.open_palette(PaletteMode::Font(slot), window, cx),
            Command::SetFont(slot, family) => self.set_font(slot, family, cx),
            Command::ToggleSidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            Command::ToggleRowPanel => self.toggle_row_panel(&ToggleRowPanel, window, cx),
            Command::ToggleRowEdit => self.toggle_row_edit(&ToggleRowEdit, window, cx),
            // Explicit, because the palette still holds focus here and would
            // read as the chrome.
            Command::ResetEditorZoom => self.set_zoom(FontSlot::Editor, 100, cx),
            Command::OpenSettings => self.show_settings(window, cx),
            Command::GoToColumn => self.go_to_column(&GoToColumn, window, cx),
            Command::RevealColumn(col) => self.reveal_column(col, window, cx),
        }
    }
}

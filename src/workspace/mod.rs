//! The workspace: the one root every action mutates and every view reads.
//!
//! The struct and its render live here; the methods that act on it are
//! grouped by concern in the sibling modules, which are parts of this same
//! inherent impl rather than types of their own.

mod commands;
mod editing;
mod filters;
mod forms;
mod modes;
mod objects;
mod profiles;
mod projects;
mod queries;
mod tabs;

use std::collections::HashMap;

use gpui_component::menu::DropdownMenu;

pub(crate) use queries::error_in_buffer;

use crate::connection_form::{Origin, password_to_persist};
use crate::session::{catalog_relation, write_buffer, write_grids};
use crate::sql::{Mode, appended_statement, remember_statement, update_batch};
use crate::theme::{install_fonts, install_theme, restored_fonts, restored_theme};
use crate::*;

/// What the app is set to, as opposed to what a connection is. The theme and
/// the fonts stay in their globals -- every view reads those at render time,
/// without a workspace to ask.
pub(crate) struct Settings {
    pub(crate) chrome_font_size: f32,
    pub(crate) editor_font_size: f32,
    pub(crate) grid_font_size: f32,
    pub(crate) row_limit: Option<usize>,
    /// How much of the window the desktop shows through, for themes with no
    /// entry in `theme_opacity`. Nothing writes it any more; it is what a
    /// single shared value from before per-theme opacity restores as.
    pub(crate) opacity: f32,
    /// Opacity per theme, keyed by theme name: light glass starts at its own
    /// default, so one shared value painted a different look on each.
    pub(crate) theme_opacity: HashMap<String, f32>,
    /// One request to GitHub at launch. Off switch because local-first users
    /// get to say no to the only request DBDelve makes on its own.
    pub(crate) check_for_updates: bool,
    /// Some people want the connection colour on the switcher only, not a
    /// painted band across the window.
    pub(crate) color_titlebar: bool,
    /// Whether a new tab sorts on a header click by reordering the rows it
    /// holds rather than by asking the server again. Off by default, which is
    /// how every tab sorted before this was a choice.
    pub(crate) client_sort: bool,
    /// Keybinding overrides, keyed by action id. Applied to the keymap on
    /// the next launch -- see `src/keybindings.rs`.
    pub(crate) custom_keybindings: HashMap<String, String>,
}

impl Settings {
    pub(crate) fn font_size(&self, slot: FontSlot) -> f32 {
        match slot {
            FontSlot::Chrome => self.chrome_font_size,
            FontSlot::Editor => self.editor_font_size,
            FontSlot::Grid => self.grid_font_size,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            chrome_font_size: layout::BODY_FONT_SIZE,
            editor_font_size: EDITOR_FONT_SIZE_DEFAULT,
            grid_font_size: layout::BODY_FONT_SIZE,
            row_limit: Some(PREVIEW_ROW_LIMIT),
            opacity: theme::OPACITY_DEFAULT,
            theme_opacity: HashMap::new(),
            check_for_updates: true,
            color_titlebar: true,
            client_sort: false,
            custom_keybindings: HashMap::new(),
        }
    }
}

impl Settings {
    pub(crate) fn opacity_for(&self, theme: &Theme) -> f32 {
        opacity_for(&self.theme_opacity, self.opacity, theme)
    }

    pub(crate) fn set_opacity_for(&mut self, theme: &Theme, opacity: f32) {
        let opacity = opacity.clamp(theme::OPACITY_MIN, theme::OPACITY_MAX);
        self.theme_opacity.insert(theme.name.to_string(), opacity);
    }
}

/// A stored entry wins. Without one, light glass starts at its own default
/// rather than the shared `fallback`, which for an existing user came from
/// dark glass; every other theme takes the fallback.
pub(crate) fn opacity_for(
    theme_opacity: &HashMap<String, f32>,
    fallback: f32,
    theme: &Theme,
) -> f32 {
    theme_opacity.get(theme.name).copied().unwrap_or_else(|| {
        if theme.is_glass && theme.appearance == theme::Appearance::Light {
            theme.default_opacity()
        } else {
            fallback
        }
    })
}

/// Which section of the Settings modal is in front. Transient like
/// `sidebar_hidden` -- which tab was open is not a preference worth
/// remembering across launches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Keybindings,
}

/// Connections read from another client, held while the user picks the
/// project they go into.
pub(crate) struct PendingImport {
    pub(crate) source: crate::import::Source,
    pub(crate) report: crate::import::Report,
    /// How many are not here already, which is what the question counts.
    pub(crate) fresh: usize,
    /// `None` for No project.
    pub(crate) project: Option<String>,
}

pub(crate) struct Workspace {
    pub(crate) profiles: Vec<Profile>,
    pub(crate) settings: Settings,
    pub(crate) active: usize,
    pub(crate) form: Option<ConnectionForm>,
    pub(crate) switcher_open: bool,
    /// The switcher's search field, there while the switcher is open.
    pub(crate) connection_search: Option<Entity<InputState>>,
    pub(crate) connection_search_needs_focus: bool,
    /// Which of the search's matches up and down have moved to, and Enter
    /// opens.
    pub(crate) search_selection: usize,
    pub(crate) projects: Vec<store::StoredProject>,
    /// The switcher's project name field, while a project is being named.
    pub(crate) project_name: Option<Entity<InputState>>,
    /// The project that field renames; `None` while it names a new one.
    pub(crate) renaming_project: Option<String>,
    pub(crate) project_name_needs_focus: bool,
    /// The field a row-limit menu turned into after Custom…, and whose limit
    /// it sets. One at a time: it closes the moment it loses focus.
    pub(crate) custom_limit: Option<(LimitTarget, Entity<InputState>)>,
    pub(crate) custom_limit_needs_focus: bool,
    /// The project whose delete button has been clicked once and is waiting
    /// for the second.
    pub(crate) pending_project_deletion: Option<String>,
    /// The groups expanded in the switcher, `None` being No project, in the
    /// order they were expanded. Looking inside a group switches nothing, so
    /// this is apart from the group of the connection in front.
    pub(crate) expanded_groups: Vec<Option<String>>,
    /// The connection whose "Add to a project" choices are unfolded in the
    /// switcher, by id.
    pub(crate) assigning_project: Option<String>,
    /// Whether the settings modal is up. On the workspace rather than a
    /// session, because nothing it changes belongs to one connection.
    pub(crate) settings_open: bool,
    /// Which tab the settings modal is showing. Not persisted -- see
    /// [`SettingsTab`].
    pub(crate) settings_tab: SettingsTab,
    /// The action id currently listening for its next keystroke, if the
    /// Keybindings tab has one mid-capture. Not persisted: a capture in
    /// progress does not survive the modal closing, let alone a relaunch.
    pub(crate) rebinding: Option<&'static str>,
    /// Whether the explorer column is folded away. Not persisted: a hidden
    /// sidebar is a thing done for the next minute, not a preference.
    pub(crate) sidebar_hidden: bool,
    /// Zoom over each font size, as a percentage. Not persisted: the sizes in
    /// Settings are the preference, and a zoom is for the next screen share
    /// or the one wide cell, with ⌘0 the way back.
    pub(crate) chrome_zoom: u32,
    pub(crate) editor_zoom: u32,
    pub(crate) grid_zoom: u32,
    pub(crate) shell_split: Entity<ResizableState>,
    pub(crate) tab_strip: crate::tab_drag::TabStrip,
    /// Where the sidebar's edge is unless a drag has moved it. The library
    /// rescales every panel by its share when the window changes size, so this
    /// is what puts the sidebar back.
    pub(crate) sidebar_width: std::cell::Cell<gpui::Pixels>,
    /// The window's width the last time the sidebar was put right, which is how
    /// a window resize is told from a drag of the handle.
    pub(crate) sidebar_container: std::cell::Cell<gpui::Pixels>,
    /// The sidebar's actual width the last time `settle_sidebar` looked, which
    /// may be less than `sidebar_width` when the container is too narrow to
    /// hold it. Comparing against this rather than against `sidebar_width`
    /// itself is what tells a real drag from that squeeze settling back out.
    pub(crate) sidebar_last_size: std::cell::Cell<gpui::Pixels>,
    /// The list a reference arrow opens, and which lookup it is waiting on.
    pub(crate) reference_popup: Option<ReferencePopup>,
    pub(crate) reference_checks: u64,
    pub(crate) row_panel: views::RowPanel,
    /// Whether the plan pane's copy button was just used, so it can show a
    /// tick the way `row_panel.copied` does. One flag rather than a keyed
    /// slot: only one plan pane is ever on screen at a time.
    pub(crate) plan_copied: bool,
    pub(crate) pending_removal: Option<String>,
    /// An import's read is still out, so a second click doesn't start another
    /// round of Keychain prompts whose summary would say all were duplicates.
    pub(crate) importing: bool,
    pub(crate) pending_import: Option<PendingImport>,
    /// The other clients installed here, looked for once at launch rather
    /// than on every frame of the welcome surface.
    pub(crate) importable: Vec<crate::import::Source>,
    /// What `note` says while there is no connection and no form to say it
    /// on: the welcome surface's line.
    pub(crate) welcome_notice: Option<String>,
    /// Why the welcome surface's name field refused what was typed: a line
    /// of its own, so a refusal never covers the notice, which may be the
    /// only word that the profiles file failed to load.
    pub(crate) project_name_error: Option<String>,
    /// The welcome surface's half of `editor_needs_focus`: set when whatever
    /// held focus over it has gone, applied on the next frame.
    pub(crate) welcome_needs_focus: bool,
    /// Whether `store::load_profiles` failed and could not move the file it
    /// failed on aside. Set once at startup and never cleared: the file is
    /// still sitting there, and a session that never saw it must not be the
    /// one that overwrites it, whatever it has made since.
    pub(crate) store_unreadable: bool,
    pub(crate) next_generation: u64,
    /// The palette, built from scratch every time it opens. Its rows are a
    /// snapshot of what the catalog held and which tab was in front, and both
    /// can move underneath it — so it is thrown away on the way out rather
    /// than kept and refreshed.
    pub(crate) palette: Option<Entity<ListState<Palette>>>,
    /// The theme in force when the theme picker opened, while it is open. The
    /// installed one is only a preview until a row is confirmed.
    pub(crate) theme_before_preview: Option<Theme>,
    /// The opacity field in the settings modal. Kept here rather than built
    /// with the card, because an input is state the user is part-way through
    /// typing into and a fresh one every frame would swallow the keystroke.
    pub(crate) opacity_input: Entity<InputState>,
    /// The window's own focus, for the moments when nothing inside it can hold
    /// any. See [`Focus::Window`].
    pub(crate) focus: FocusHandle,
    pub(crate) newer_release: Option<update::Release>,
}

impl Workspace {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let opacity_input = cx.new(|cx| InputState::new(window, cx));
        // With the window, because committing reinstalls the theme. Enter and
        // blur both count as done: a percentage is short enough that clicking
        // away from it is as much an answer as pressing return.
        cx.subscribe_in(
            &opacity_input,
            window,
            |workspace, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    workspace.commit_opacity_input(window, cx);
                }
            },
        )
        .detach();

        let mut workspace = Self {
            profiles: Vec::new(),
            settings: Settings::default(),
            active: 0,
            form: None,
            switcher_open: false,
            connection_search: None,
            connection_search_needs_focus: false,
            search_selection: 0,
            projects: Vec::new(),
            project_name: None,
            renaming_project: None,
            project_name_needs_focus: false,
            custom_limit: None,
            custom_limit_needs_focus: false,
            pending_project_deletion: None,
            expanded_groups: Vec::new(),
            assigning_project: None,
            settings_open: false,
            settings_tab: SettingsTab::default(),
            rebinding: None,
            sidebar_hidden: false,
            chrome_zoom: 100,
            editor_zoom: 100,
            grid_zoom: 100,
            shell_split: cx.new(|_| ResizableState::default()),
            tab_strip: crate::tab_drag::TabStrip::default(),
            sidebar_width: std::cell::Cell::new(px(layout::SIDEBAR_DEFAULT_WIDTH)),
            sidebar_container: std::cell::Cell::new(px(0.)),
            sidebar_last_size: std::cell::Cell::new(px(layout::SIDEBAR_DEFAULT_WIDTH)),
            reference_popup: None,
            reference_checks: 0,
            row_panel: views::RowPanel {
                on_screen: Default::default(),
                copied: None,
            },
            plan_copied: false,
            pending_removal: None,
            importing: false,
            pending_import: None,
            importable: crate::import::Source::ALL
                .into_iter()
                .filter(|source| source.found())
                .collect(),
            welcome_notice: None,
            project_name_error: None,
            welcome_needs_focus: true,
            store_unreadable: false,
            next_generation: 0,
            palette: None,
            theme_before_preview: None,
            opacity_input,
            focus: cx.focus_handle(),
            newer_release: None,
        };

        let mut load_failure = None;
        match store::load_profiles() {
            Ok((profiles, active, stored_fonts, stored_settings, projects)) => {
                // Before the first frame, so the window is drawn in the faces
                // the user picked rather than repainted into them.
                let stored_settings = stored_settings.unwrap_or_default();
                // Ahead of the fonts and the theme, which size gpui-component's
                // widgets off the chrome size as they install.
                let chrome_font_size =
                    restored_font_size(FontSlot::Chrome, stored_settings.chrome_font_size);
                let grid_font_size =
                    restored_font_size(FontSlot::Grid, stored_settings.grid_font_size);
                workspace.settings.chrome_font_size = chrome_font_size;
                workspace.settings.grid_font_size = grid_font_size;
                layout::set_chrome_font_size(chrome_font_size);
                layout::set_grid_font_size(grid_font_size);
                let available = cx.text_system().all_font_names();
                install_fonts(restored_fonts(stored_fonts, &available), cx);
                workspace.settings.opacity = restored_opacity(stored_settings.opacity);
                workspace.settings.theme_opacity = stored_settings
                    .theme_opacity
                    .iter()
                    .flatten()
                    .map(|(name, opacity)| (name.clone(), restored_opacity(Some(*opacity))))
                    .collect();
                let restored = restored_theme(stored_settings.theme.as_deref());
                install_theme(
                    restored.with_opacity(workspace.settings.opacity_for(&restored)),
                    window,
                    cx,
                );
                // The zoom was per-profile until settings existed, so a file
                // with no app-wide value has one under whichever profile was in
                // front -- and reading it there is what keeps a person's zoom
                // across the upgrade instead of resetting it.
                workspace.settings.editor_font_size = restored_font_size(
                    FontSlot::Editor,
                    stored_settings.editor_font_size.or_else(|| {
                        profiles
                            .iter()
                            .find(|stored| Some(stored.id.as_str()) == active.as_deref())
                            .or_else(|| profiles.first())
                            .and_then(|stored| stored.editor_font_size)
                    }),
                );
                workspace.settings.row_limit = stored_settings.row_limit();
                workspace.settings.check_for_updates =
                    stored_settings.check_for_updates.unwrap_or(true);
                workspace.settings.color_titlebar = stored_settings.color_titlebar.unwrap_or(true);
                workspace.settings.client_sort = stored_settings.client_sort.unwrap_or(false);
                workspace.settings.custom_keybindings = stored_settings
                    .custom_keybindings
                    .clone()
                    .unwrap_or_default();
                for stored in profiles {
                    workspace.restore_profile(stored, window, cx);
                }
                // Where the last session was left. An id that no longer names a
                // profile leaves the first one in front, which is where an
                // install with no history starts anyway.
                if let Some(id) = active
                    && let Some(index) = workspace
                        .profiles
                        .iter()
                        .position(|profile| profile.id == id)
                {
                    workspace.active = index;
                }
                let live = workspace
                    .profiles
                    .iter()
                    .map(|profile| profile.id.as_str())
                    .collect::<Vec<_>>();
                workspace.projects = projects::normalized_projects(projects, &live);
            }
            Err(failure) => {
                workspace.store_unreadable = !failure.moved_aside;
                load_failure = Some(failure.message);
            }
        }

        match connection_config_from_environment() {
            Ok(Some(config)) => {
                let existing = workspace.profiles.iter().position(|profile| {
                    profile.config.engine() == config.engine()
                        && profile.config.endpoint() == config.endpoint()
                        && profile.config.server().map(|server| server.user.as_str())
                            == config.server().map(|server| server.user.as_str())
                });
                workspace.active = match existing {
                    Some(index) => index,
                    None => {
                        let name = default_profile_name(&config);
                        workspace.create_profile(
                            name,
                            config,
                            None,
                            Mode::default(),
                            Origin::Environment,
                            window,
                            cx,
                        )
                    }
                };
                // The environment picked the profile, so it is the one to come
                // back to next launch -- when there may be no environment.
                workspace.remember_profiles(cx);
            }
            Ok(None) => {}
            Err(message) => {
                let mut form = ConnectionForm::new(None, window, cx);
                form.error = Some(message);
                workspace.form = Some(form);
            }
        }

        // After the environment's form, which takes the notice over the
        // welcome surface when there is one.
        if let Some(message) = load_failure {
            workspace.note(message, cx);
        }

        if workspace.settings.check_for_updates {
            let check = cx
                .background_executor()
                .spawn(async { update::newer_release() });
            cx.spawn(async move |workspace, cx| {
                let Some(release) = check.await else {
                    return;
                };
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.newer_release = Some(release);
                    cx.notify();
                });
            })
            .detach();
        }

        // The settings modal owns the keyboard while it is up, and an
        // interceptor is the only place that can give it to it: GPUI matches a
        // keystroke against the keymap and dispatches the action *before* any
        // element listener runs, so a capture handler on the modal would see
        // `cmd+enter` only after it had already run the query underneath. It is
        // also the only place early enough to read back a chord the app already
        // has bound, which is most of what a rebind is for.
        let this = cx.weak_entity();
        cx.intercept_keystrokes(move |event, window, cx| {
            let keystroke = event.keystroke.clone();
            this.update(cx, |workspace, cx| {
                // The palette opens over the modal (a font is picked from it) and
                // holds the keyboard while it does.
                if !workspace.settings_open
                    || workspace.palette.is_some()
                    || keybindings::is_modifier(&keystroke.key)
                {
                    return;
                }
                match (workspace.rebinding, keystroke.key.as_str()) {
                    // Passed on: with nothing mid-capture, escape is what
                    // closes the modal.
                    (None, "escape") => return,
                    (Some(_), "escape") => workspace.cancel_rebind(cx),
                    // `unparse`, not `to_string` -- the latter is the glyphs a
                    // menu draws, and nothing reads those back.
                    (Some(id), _) => workspace.apply_rebind(id, keystroke.unparse(), cx),
                    // Owning the keyboard was written when nothing in the
                    // modal could be typed into, and a field that cannot see
                    // a keystroke is a field nobody can fill. A capture in
                    // progress still outranks it -- that is the arm above.
                    //
                    // Only what the field can actually consume, though: every
                    // chord the app binds is `secondary-`, so passing those on
                    // too would close the tab behind the modal on `cmd-w`. The
                    // clipboard keys are the exception -- the input binds them
                    // in its own context, which dispatch reaches before
                    // anything global.
                    (None, _)
                        if workspace.opacity_input.focus_handle(cx).is_focused(window)
                            && (!keystroke.modifiers.secondary()
                                || matches!(
                                    keystroke.key.as_str(),
                                    "a" | "c" | "v" | "x" | "z"
                                )) =>
                    {
                        return;
                    }
                    (None, _) => {}
                }
                cx.stop_propagation();
            })
            .ok();
        })
        .detach();

        // Buffers are otherwise written only when one is swapped for another,
        // so without this everything typed since the last swap dies with the
        // process -- which is the one moment a person expects it to be kept.
        cx.on_app_quit(|workspace: &mut Self, cx: &mut Context<Self>| {
            workspace.persist_buffers(cx);
            async {}
        })
        .detach();
        // Closing the window does not quit the application, so without this the
        // red button is a way to lose everything typed since the last swap.
        cx.on_release(|workspace, cx| workspace.persist_buffers(cx))
            .detach();

        workspace.connect_active(cx);
        workspace
    }

    pub(crate) fn profile(&self) -> Option<&Profile> {
        self.profiles.get(self.active)
    }

    pub(crate) fn profile_mut(&mut self) -> Option<&mut Profile> {
        self.profiles.get_mut(self.active)
    }

    /// The engine every statement dbdelve generates is written for. With no
    /// profile there is nothing to run it against, so the default is only ever
    /// used to build a string nobody sends.
    pub(crate) fn engine(&self) -> Engine {
        self.profile()
            .map(|profile| profile.config.engine())
            .unwrap_or_default()
    }

    pub(crate) fn issued_to(&mut self, id: &str, generation: u64) -> Option<&mut Profile> {
        self.profiles
            .iter_mut()
            .find(|profile| profile.id == id && profile.generation == generation)
    }

    /// A notice describes what happened to the last thing the user asked for, so
    /// asking for the next thing takes it down. Otherwise a refusal like "this
    /// column cannot be edited" sits in the status bar for the rest of the
    /// session.
    ///
    /// Called from the gestures that run SQL rather than from
    /// `execute_and_then`, because a statement dbdelve runs on its own — restoring
    /// a tab at startup — would otherwise clear a notice nobody has read yet,
    /// and one of those says the connection came up weaker than it asked for.
    pub(crate) fn clear_notice(&mut self) {
        match self.profile_mut() {
            Some(profile) => profile.session.notice = None,
            None => self.welcome_notice = None,
        }
    }

    /// Said over restored rows while their refresh is in flight, and taken
    /// down by that refresh landing -- only if it is still what the status bar
    /// says.
    pub(crate) const REFRESHING: &str = "These rows are being refreshed.";

    /// Hands focus back to what is in front once whatever held it has gone:
    /// the tab, or the welcome surface when there is no connection. A field
    /// unmounted with focus in it leaves the window focused on nothing, and
    /// every keybinding dead.
    pub(crate) fn refocus_front(&mut self) {
        match self.profile_mut() {
            Some(profile) => profile.session.editor_needs_focus = true,
            None => self.welcome_needs_focus = true,
        }
    }

    pub(crate) fn note(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = Some(message);
        } else if let Some(form) = &mut self.form {
            form.error = Some(message);
        } else {
            self.welcome_notice = Some(message);
        }
        cx.notify();
    }

    pub(crate) fn zoom_editor_in(
        &mut self,
        _: &ZoomEditorIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot = self.zoom_target(window, cx);
        self.set_zoom(slot, self.zoom_step(slot, true), cx);
    }

    pub(crate) fn zoom_editor_out(
        &mut self,
        _: &ZoomEditorOut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot = self.zoom_target(window, cx);
        self.set_zoom(slot, self.zoom_step(slot, false), cx);
    }

    pub(crate) fn reset_editor_zoom(
        &mut self,
        _: &ResetEditorZoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot = self.zoom_target(window, cx);
        self.set_zoom(slot, 100, cx);
    }

    pub(crate) fn zoom(&self, slot: FontSlot) -> u32 {
        match slot {
            FontSlot::Chrome => self.chrome_zoom,
            FontSlot::Editor => self.editor_zoom,
            FontSlot::Grid => self.grid_zoom,
        }
    }

    /// The chrome's zoom stops where its size would leave the range Settings
    /// offers, for the titlebar's sake; the editor and the grid have room to
    /// take every level.
    fn zoom_step(&self, slot: FontSlot, zoom_in: bool) -> u32 {
        let next = stepped_zoom(self.zoom(slot), zoom_in);
        if slot != FontSlot::Chrome {
            return next;
        }
        let (min, _, max) = font_size_range(FontSlot::Chrome);
        if (min..=max).contains(&zoomed(self.settings.chrome_font_size, next)) {
            next
        } else {
            self.chrome_zoom
        }
    }

    fn set_zoom(&mut self, slot: FontSlot, percent: u32, cx: &mut Context<Self>) {
        match slot {
            FontSlot::Chrome => {
                self.chrome_zoom = percent;
                self.apply_chrome_size(cx);
            }
            FontSlot::Editor => self.editor_zoom = percent,
            FontSlot::Grid => {
                self.grid_zoom = percent;
                self.apply_grid_size(cx);
            }
        }
        cx.refresh_windows();
    }

    fn apply_chrome_size(&self, cx: &mut App) {
        layout::set_chrome_font_size(zoomed(self.settings.chrome_font_size, self.chrome_zoom));
        let theme = *theme(cx);
        theme.apply_to_components(cx);
    }

    /// The size the editor is drawn at: the saved size under the zoom.
    pub(crate) fn editor_size(&self) -> f32 {
        zoomed(self.settings.editor_font_size, self.editor_zoom)
    }

    fn apply_grid_size(&self, cx: &mut App) {
        layout::set_grid_font_size(zoomed(self.settings.grid_font_size, self.grid_zoom));
        // The table reads the gutter's width once and keeps it, so every grid
        // open anywhere has to be told it changed.
        for profile in &self.profiles {
            for grid in profile.session.grids() {
                grid.update(cx, |table, cx| table.refresh(cx));
            }
        }
    }

    /// Zoom acts on the pane the keyboard is in: the editor or the grid while
    /// one holds focus, a cell being edited included, and the chrome
    /// everywhere else.
    fn zoom_target(&self, window: &Window, cx: &App) -> FontSlot {
        // Settings and the palette float over the panes, and what is being
        // read then is them, whichever pane kept focus underneath.
        if self.settings_open || self.palette.is_some() {
            return FontSlot::Chrome;
        }
        let Some(session) = self.profile().map(|profile| &profile.session) else {
            return FontSlot::Chrome;
        };
        let holds_focus = |handle: FocusHandle| handle.contains_focused(window, cx);
        if session
            .editor(session.active)
            .is_some_and(|editor| holds_focus(editor.focus_handle(cx)))
        {
            FontSlot::Editor
        } else if session
            .active_results()
            .is_some_and(|results| holds_focus(results.focus_handle(cx)))
        {
            FontSlot::Grid
        } else {
            FontSlot::Chrome
        }
    }

    pub(crate) fn step_font_size(&mut self, slot: FontSlot, delta: f32, cx: &mut Context<Self>) {
        let adjusted = adjusted_font_size(slot, self.settings.font_size(slot), delta);
        self.set_font_size(slot, adjusted, cx);
    }

    /// Written through to disk, because a size that resets on relaunch is a
    /// setting the user has to make again every morning.
    pub(crate) fn set_font_size(&mut self, slot: FontSlot, size: f32, cx: &mut Context<Self>) {
        if self.settings.font_size(slot) == size {
            return;
        }
        match slot {
            // A zoom kept over a new chrome size could carry it past the
            // titlebar's range, where no step back fits.
            FontSlot::Chrome => {
                self.settings.chrome_font_size = size;
                self.chrome_zoom = 100;
                self.apply_chrome_size(cx);
            }
            FontSlot::Editor => self.settings.editor_font_size = size,
            FontSlot::Grid => {
                self.settings.grid_font_size = size;
                self.apply_grid_size(cx);
            }
        }
        self.remember_profiles(cx);
        // The whole window, not just this view: the grid is its own entity,
        // and nothing about its rows changed to make it redraw.
        cx.refresh_windows();
    }

    /// The default a relation tab opens with. Written through for the same
    /// reason the zoom is, and deliberately not applied to the tabs already
    /// open: their row count is a property of those rows, and changing a
    /// default must never re-run a query nobody asked to re-run.
    pub(crate) fn set_default_row_limit(&mut self, rows: Option<usize>, cx: &mut Context<Self>) {
        if self.settings.row_limit == rows {
            return;
        }
        self.settings.row_limit = rows;
        self.remember_profiles(cx);
        cx.notify();
    }

    pub(crate) fn set_check_for_updates(&mut self, check: bool, cx: &mut Context<Self>) {
        if self.settings.check_for_updates == check {
            return;
        }
        self.settings.check_for_updates = check;
        self.remember_profiles(cx);
        cx.notify();
    }

    pub(crate) fn set_color_titlebar(&mut self, color: bool, cx: &mut Context<Self>) {
        if self.settings.color_titlebar == color {
            return;
        }
        self.settings.color_titlebar = color;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// The sorting a new tab starts with. Not applied to the tabs already
    /// open, for the reason `set_default_row_limit` is not: each holds its own.
    pub(crate) fn set_client_sort(&mut self, client: bool, cx: &mut Context<Self>) {
        if self.settings.client_sort == client {
            return;
        }
        self.settings.client_sort = client;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Written through for the same reason the zoom is: a font that resets on
    /// relaunch is a setting the user has to make again every morning.
    pub(crate) fn set_font(&mut self, slot: FontSlot, family: String, cx: &mut Context<Self>) {
        let mut picked = fonts(cx).clone();
        picked.set(slot, family.into());
        install_fonts(picked, cx);
        self.remember_profiles(cx);
        cx.refresh_windows();
    }
}

/// What `settle_sidebar` should do with the split this frame, decided from
/// numbers alone so the decision is testable without a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarSettle {
    /// The container changed size: put the sidebar back at its remembered
    /// width, clamped to whatever room the content panel's minimum leaves.
    Resize,
    /// The container didn't move, so nothing but a drag could have changed
    /// the handle: adopt the new width as the one to remember.
    Adopt,
    /// Nothing changed since the last time this settled.
    Settled,
}

/// `last_size` is the sidebar's actual width the last time this ran, which
/// can differ from `target` (the width to put it back at) when the container
/// was too narrow to hold `target` in full -- that squeeze must resolve
/// through `Resize`, never `Adopt`, or widening the window again could never
/// recover the width the user actually asked for.
fn sidebar_settle(
    container: gpui::Pixels,
    last_container: gpui::Pixels,
    sizes_0: gpui::Pixels,
    last_size: gpui::Pixels,
) -> SidebarSettle {
    if container != last_container {
        SidebarSettle::Resize
    } else if sizes_0 != last_size {
        SidebarSettle::Adopt
    } else {
        SidebarSettle::Settled
    }
}

impl Workspace {
    /// Puts the sidebar back at its width after a window resize, and takes the
    /// width a drag has left it at.
    fn settle_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_hidden {
            return;
        }
        let (target, last_container, last_size) = (
            self.sidebar_width.get(),
            self.sidebar_container.get(),
            self.sidebar_last_size.get(),
        );
        self.shell_split.update(cx, |state, cx| {
            let container = state.container_size();
            if container <= px(1.) || state.sizes().len() != 2 {
                return;
            }
            match sidebar_settle(container, last_container, state.sizes()[0], last_size) {
                SidebarSettle::Resize => {
                    self.sidebar_container.set(container);
                    if (state.sizes()[0] - target).abs() > px(0.5) {
                        state.resize_panel(0, target, window, cx);
                    }
                    self.sidebar_last_size.set(state.sizes()[0]);
                }
                SidebarSettle::Adopt => {
                    self.sidebar_width.set(state.sizes()[0]);
                    self.sidebar_last_size.set(state.sizes()[0]);
                }
                SidebarSettle::Settled => {}
            }
        });
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        self.settle_sidebar(window, cx);
        self.row_panel.on_screen.set(false);
        // Every way the switcher closes ends here, so this is the one place
        // its name field is put away -- before the focus handoff below. The
        // welcome surface has the field too, and keeps it while in front.
        if !self.switcher_open {
            if !(self.profiles.is_empty() && self.form.is_none()) {
                self.drop_project_name();
            }
            self.drop_connection_search();
        }
        // Deferred to render for the `&mut Window` a background task does not
        // have: the catalog that names these tabs resolves off-thread, and a
        // grid cannot be built without a window.
        self.restore_objects(window, cx);
        self.sync_page_input(window, cx);
        // Where a query tab that reached the front without `activate_tab` gets
        // its snapshot read. `session.active` is written in three places --
        // `Session::new`, `activate_tab` and `escape` -- and the first and last
        // set a `Tab::Query` without hydrating it, so this cannot be narrowed
        // to the opening tab.
        //
        // What keeps it off a live result is `hydrate_tab`'s own `Idle` guard,
        // which holds only because no reachable state leaves a query tab
        // `Idle` while its grid holds rows: a run blanks the grid before it
        // starts, and nothing sets `Idle` back afterwards. A "clear results"
        // or a cancel that resets state would break that, and this line would
        // then read a snapshot over rows the user is looking at.
        //
        // Gated on a flag rather than on the disk, so every later frame is a
        // bool test.
        if let Some(active @ Tab::Query(_)) = self.profile().map(|profile| profile.session.active) {
            self.hydrate_tab(active, window, cx);
        }

        // Deferred for the same reason plus one: an element has to be mounted
        // before it can take focus.
        let take_focus = self.profile_mut().and_then(|profile| {
            if profile.session.naming {
                let wanted = profile.session.save_name_needs_focus;
                return wanted.then(|| {
                    profile.session.save_name_needs_focus = false;
                    Focus::Field(profile.session.save_name.clone())
                });
            }
            if !profile.session.editor_needs_focus {
                return None;
            }
            // The chip for whatever just took focus glides into view beside
            // it, one-shot the same way: a tab just opened or activated off
            // screen is a tab the strip should show, not one it leaves the
            // user to go scroll for.
            if let Some(index) = profile
                .session
                .strip_order()
                .iter()
                .position(|key| profile.session.tab_of(key) == Some(profile.session.active))
            {
                crate::scroller::scroll_to("tab-strip", index, cx);
            }
            // Whatever the surface in front is: a keystroke reaches the
            // workspace along the focused element's dispatch path, so a
            // surface with nothing focused makes every keybinding dead.
            let focus = match profile.session.active {
                // No tab at all is still a surface: the window holds focus
                // for the bindings that open one.
                Tab::Query(id) => match profile.session.query_tab(id) {
                    Some(tab) => Focus::Buffer(tab.editor.clone()),
                    None => Focus::Window,
                },
                Tab::Object(id) => match profile
                    .session
                    .objects
                    .iter()
                    .find(|tab| tab.id == id)
                    .map(|tab| &tab.body)
                {
                    Some(ObjectBody::Relation { results, .. }) => Focus::Grid(results.clone()),
                    // A routine's tab is read: nothing in it takes a
                    // keystroke. The window still has to hold focus, or the
                    // bindings that leave this tab go with it -- as it does
                    // for the last object tab of all, closed.
                    Some(ObjectBody::Routine(_)) | None => Focus::Window,
                },
            };
            profile.session.editor_needs_focus = false;
            Some(focus)
        });
        // Before the tab's own focus, and separately: the form is a surface of
        // its own, and a field it just unmounted took the window's only
        // dispatch path with it.
        if let Some(input) = self.form.as_mut().and_then(|form| form.needs_focus.take()) {
            input.focus_handle(cx).focus(window, cx);
        }
        // Asked for by name rather than read off what holds focus: that is
        // checked against the last frame, where the field that just closed
        // still sat inside this same floor.
        if self.form.is_none()
            && self.profiles.is_empty()
            && std::mem::take(&mut self.welcome_needs_focus)
        {
            self.focus.focus(window, cx);
        }
        if std::mem::take(&mut self.custom_limit_needs_focus)
            && let Some((_, input)) = &self.custom_limit
        {
            input.focus_handle(cx).focus(window, cx);
        }
        if std::mem::take(&mut self.project_name_needs_focus)
            && let Some(input) = &self.project_name
        {
            input.focus_handle(cx).focus(window, cx);
        }
        if std::mem::take(&mut self.connection_search_needs_focus)
            && let Some(input) = &self.connection_search
        {
            input.focus_handle(cx).focus(window, cx);
        }

        match take_focus {
            Some(Focus::Buffer(editor)) => editor.focus_handle(cx).focus(window, cx),
            Some(Focus::Field(input)) => input.focus_handle(cx).focus(window, cx),
            Some(Focus::Grid(grid)) => grid.focus_handle(cx).focus(window, cx),
            Some(Focus::Window) => self.focus.focus(window, cx),
            None => {}
        }

        // Last, and unconditionally: the palette is modal, and it holds the
        // keyboard against anything above that just claimed it. One a modal
        // cannot be typed into is one that cannot be dismissed either.
        if let Some(list) = &self.palette {
            let handle = list.focus_handle(cx);
            if !handle.is_focused(window) {
                handle.focus(window, cx);
            }
        }

        if self.form.is_some() {
            return div()
                .id("connection-form")
                .size_full()
                // The floor under the focus, the same one the workspace root
                // has: without it the form's bindings dispatch nowhere the
                // moment no field holds focus.
                .track_focus(&self.focus)
                // Chrome, so the form's card is the raised plane on it.
                .text_color(t.text)
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .flex()
                .flex_col()
                .on_action(cx.listener(Self::select_theme))
                .on_action(cx.listener(Self::show_editor))
                .on_action(cx.listener(Self::palette_next))
                .on_action(cx.listener(Self::palette_previous))
                .on_action(cx.listener(Self::next_profile))
                .on_action(cx.listener(Self::zoom_editor_in))
                .on_action(cx.listener(Self::zoom_editor_out))
                .on_action(cx.listener(Self::reset_editor_zoom))
                .on_action(cx.listener(Self::previous_profile))
                // Without a titlebar of its own the form has no drag handle at
                // all, since the platform's is transparent.
                .child(titlebar(t, None, Vec::new(), Vec::new(), Vec::new()))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_connection_form(cx)),
                )
                .children(self.render_import_choice(cx))
                .children(self.render_palette(cx));
        }
        let Some(profile) = self.profile() else {
            return div()
                .id("welcome")
                .size_full()
                .track_focus(&self.focus)
                .text_color(t.text)
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .flex()
                .flex_col()
                .on_action(cx.listener(Self::select_theme))
                .on_action(cx.listener(Self::show_editor))
                .on_action(cx.listener(Self::palette_next))
                .on_action(cx.listener(Self::palette_previous))
                .on_action(cx.listener(Self::open_connection_form))
                .on_action(cx.listener(Self::new_project))
                .on_action(cx.listener(Self::import_from))
                .on_action(cx.listener(Self::open_settings))
                .on_action(cx.listener(Self::zoom_editor_in))
                .on_action(cx.listener(Self::zoom_editor_out))
                .on_action(cx.listener(Self::reset_editor_zoom))
                .child(titlebar(t, None, Vec::new(), Vec::new(), Vec::new()))
                .child(div().flex_1().min_h_0().child(self.render_welcome(cx)))
                .children(self.settings_open.then(|| views::render_settings(self, cx)))
                .children(self.render_import_choice(cx))
                .children(self.render_palette(cx));
        };
        let failed = matches!(profile.state, ProfileState::Failed(_));
        let status = match &profile.state {
            ProfileState::Idle => "Connection is idle.".to_string(),
            ProfileState::Connecting => format!("Connecting to {}…", profile.config.endpoint()),
            // Connected is the one state worth spending on decoration: every
            // other one is news, and news beats where the connection points.
            // Its name is already on the switcher in the titlebar.
            ProfileState::Connected(_) => {
                match profile.config.server().map(|server| &server.database) {
                    Some(database) if !database.is_empty() => {
                        format!("{} / {database}", profile.config.endpoint())
                    }
                    _ => profile.config.endpoint(),
                }
            }
            ProfileState::Failed(message) => message.clone(),
        };

        // The rows the grid actually holds, which is fewer than the result had
        // whenever a restored snapshot was capped.
        let showing = profile
            .session
            .active_results()
            .map(|results| results.read(cx).delegate().result().rows.len());
        let stopped = profile
            .session
            .active_results()
            .and_then(|results| results.read(cx).delegate().result().stopped);
        let column_count = profile
            .session
            .active_results()
            .map(|results| results.read(cx).delegate().columns().len());
        let snapshot_age = profile
            .session
            .active_results()
            .and_then(|results| results.read(cx).delegate().captured())
            .map(|captured| relative_age(store::captured_at().saturating_sub(captured)));
        let stale_buffer =
            snapshot_age.is_some() && matches!(profile.session.active, Tab::Query(_));
        // Only with the statement to show: a snapshot older than `last_query`
        // would have the prompt's Refresh run whatever the cursor is on. And
        // only a read, since a write restored from the last session would
        // write again.
        let refreshable_snapshot = stale_buffer
            && profile.session.active_query_tab().is_some_and(|tab| {
                tab.last_query
                    .as_deref()
                    .is_some_and(|sql| sql::rerunnable(profile.config.engine(), sql))
            });
        // A query tab's own slot rather than the result in front: a queue run
        // in turn can be showing a statement that finished while the next one
        // runs. Run at once, the slot only holds the run open, and each
        // statement streams into its own entry, so the one in front is read.
        let running = match profile.session.active_query_tab() {
            Some(tab) if tab.queue.as_ref().is_some_and(|queue| queue.at_once()) => {
                let (state, results) = tab.shown();
                views::render_running(state, Some(results), cx)
            }
            Some(tab) => views::render_running(&tab.query, Some(&tab.results), cx),
            None => profile.session.active_query().and_then(|query| {
                views::render_running(query, profile.session.active_results(), cx)
            }),
        };
        let paging =
            running.or_else(|| views::render_paging(profile, self.custom_limit.as_ref(), cx));
        let view_sorting = views::render_view_sorting(profile, cx);
        let relation = profile
            .session
            .active_object()
            .and_then(|tab| match &tab.body {
                ObjectBody::Relation {
                    count,
                    filter,
                    query,
                    limit,
                    offset,
                    showing_structure: false,
                    ..
                } => Some((tab, count, filter, query, *limit, *offset)),
                _ => None,
            });
        let engine = profile.config.engine();
        let relation_rows = relation.and_then(|(tab, count, filter, query, limit, offset)| {
            let estimate = match &profile.catalog {
                CatalogState::Loaded(catalog, _) => {
                    catalog_relation(catalog, &tab.schema, &tab.name)
                        .and_then(|relation| relation.rows)
                }
                _ => None,
            };
            let whole = offset == 0
                && matches!(
                    query,
                    QueryState::Complete { rows, .. } if limit.is_none_or(|limit| *rows < limit)
                );
            session::relation_rows(count, filter, estimate, engine.exact_row_estimates(), whole)
        });
        // The way to an exact number, which is never run unasked. Its tooltip
        // is the statement it runs.
        let count_control = relation
            .filter(|_| profile.connection().is_some())
            .and_then(|(tab, count, filter, ..)| {
                let id = tab.id;
                let workspace = cx.entity().downgrade();
                match count {
                    RowCount::Counting { cancelling, .. } => Some(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .child(div().whitespace_nowrap().text_color(t.text_faint).child(
                                match cancelling {
                                    true => "Cancelling count…",
                                    false => "Counting rows…",
                                },
                            ))
                            .children((!cancelling).then(|| {
                                button("cancel-count", "Cancel", Tone::Quiet, Control::Compact, t)
                                    .on_click(move |_, _, cx| {
                                        _ = workspace.update(cx, |workspace, cx| {
                                            workspace.cancel_count(id, cx);
                                        });
                                    })
                            }))
                            .into_any_element(),
                    ),
                    _ if count.answers(filter) => None,
                    _ => {
                        let sql = explorer::count_sql(engine, &tab.schema, &tab.name, filter);
                        Some(
                            button("count-rows", "Count", Tone::Quiet, Control::Compact, t)
                                .tooltip(sql)
                                .on_click(move |_, _, cx| {
                                    _ = workspace.update(cx, |workspace, cx| {
                                        workspace.count_rows(id, cx);
                                    });
                                })
                                .into_any_element(),
                        )
                    }
                }
            });
        let query_status = match profile.session.active_query() {
            Some(QueryState::Complete {
                rows,
                bytes,
                elapsed,
                ..
            }) => {
                // The rows on screen, unless the relation's own count or
                // estimate is beside the stats already.
                let count = relation_rows.is_none().then(|| {
                    let count = row_readout(showing.unwrap_or(*rows), *rows);
                    match stopped {
                        Some(db::Stopped::Cancelled) => format!("{count}, cancelled"),
                        Some(db::Stopped::AtLimit) => format!("{count}, stopped at limit"),
                        None => count,
                    }
                });
                let joined = |rest: String| match count {
                    Some(count) => format!("{count} \u{b7} {rest}"),
                    None => rest,
                };
                // A snapshot knows neither how many bytes crossed the wire nor
                // how long it took, so reporting `0 B · 0.0ns` invents two
                // numbers. What it does know is when it was taken.
                // A relation refreshes its own snapshot as soon as it can. A
                // buffer's statement is the user's to run again, and nothing
                // else would tell them the rows are waiting on it.
                Some(match snapshot_age {
                    Some(age) if stale_buffer => joined(format!("from {age} ago")),
                    Some(age) => joined(format!("snapshot from {age} ago")),
                    None => joined(format!(
                        "{} \u{b7} {elapsed:.1?}",
                        human_bytes(*bytes as u64)
                    )),
                })
            }
            // The plan's own rows are not this tab's result and never reached
            // the grid, so the count beside them still belongs to whatever ran
            // last. What the run itself is worth saying is how long the server
            // spent answering.
            Some(QueryState::Explained { elapsed, mode }) => {
                Some(format!("{} · {elapsed:.1?}", mode.label()))
            }
            _ => None,
        };
        let left_stats = [
            relation_rows,
            column_count.filter(|columns| *columns > 0).map(|columns| {
                format!(
                    "{columns} {}",
                    if columns == 1 { "column" } else { "columns" }
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let left_stats = (!left_stats.is_empty()).then(|| left_stats.join(" \u{b7} "));
        // The dot carries the state and the text carries the words. A whole
        // status line in green shouts about being connected, which is the
        // least interesting thing dbdelve can tell you.
        let status_group = div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .min_w_0()
            .child(ui::status_dot(t, &profile.state))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(if failed { t.danger } else { t.text_muted })
                    .child(status),
            )
            .when(failed, |group| {
                group.child(
                    div()
                        .flex_shrink_0()
                        .child(ui::reconnect_button("reconnect", t)),
                )
            });
        let (sidebar_status, inline_status) = if self.sidebar_hidden {
            (None, Some(status_group))
        } else {
            (Some(status_group), None)
        };
        let notice = profile.session.notice.clone();
        let newer_release = self.newer_release.clone();
        let pending_count = self.pending_edit_count(cx);
        let has_pending = pending_count > 0;
        let grid_shown = !profile.session.active_object().is_some_and(|tab| {
            matches!(
                tab.body,
                ObjectBody::Relation {
                    showing_structure: true,
                    ..
                }
            )
        });
        let has_results = self.has_results(cx);
        let show_result_actions = has_results && !has_pending;
        let apply_workspace = cx.entity().downgrade();
        let discard_workspace = apply_workspace.clone();
        let previous_workspace = apply_workspace.clone();
        let next_workspace = apply_workspace.clone();
        let copy_workspace = apply_workspace.clone();
        let csv_workspace = apply_workspace.clone();
        let json_workspace = apply_workspace.clone();
        let refresh_workspace = apply_workspace.clone();

        // Chrome for content, not a fixture of the window: with no tab open
        // every field below is empty, and an empty bar is a background and a
        // hairline with nothing on either side of them.
        let bar_has_content = inline_status.is_some()
            || left_stats.is_some()
            || count_control.is_some()
            || notice.is_some()
            || paging.is_some()
            || view_sorting.is_some()
            || query_status.is_some()
            || refreshable_snapshot
            || has_results
            || has_pending;

        let results_status = bar_has_content.then(|| {
            div()
                .h(px(layout::chrome(layout::STATUS_HEIGHT)))
                .flex_shrink_0()
                .border_t_1()
                .border_color(t.border)
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .px(px(layout::SPACE_MD))
                .bg(t.data_glass())
                .children(inline_status)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .children(view_sorting)
                        .children(left_stats.map(|stats| {
                            div()
                                .flex_shrink_0()
                                .whitespace_nowrap()
                                .text_color(t.text_faint)
                                .child(stats)
                        }))
                        .children(count_control)
                        .children(notice.map(|notice| {
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(t.text_muted)
                                .child(notice)
                        })),
                )
                // Between two sides that share the free space equally, so
                // it sits at the middle of the bar whatever either holds.
                .children(paging)
                // One right-hand cluster taking the other half of that
                // space, so the readout stays against the edge.
                //
                // Each control appears only when it does something. A pair
                // of buttons that do nothing is a pair to read past --
                // see `apply_edits` for its `cmd+s` binding.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .justify_end()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .children(query_status.map(|query_status| {
                            div()
                                .text_color(match stale_buffer {
                                    true => t.text_muted,
                                    false => t.text_faint,
                                })
                                .child(query_status)
                        }))
                        // Through the stale-rows prompt rather than straight
                        // to a run, so the statement is read before it is
                        // sent.
                        .children(refreshable_snapshot.then(|| {
                            button(
                                "refresh-snapshot",
                                "Refresh…",
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = refresh_workspace.update(cx, |workspace, cx| {
                                    workspace.ask_refresh_stale(cx);
                                });
                            })
                        }))
                        // Not beside pending edits: these write the rows as
                        // fetched, not the values on screen, and the review
                        // controls need the room.
                        .children(show_result_actions.then(|| {
                            button(
                                "copy-results",
                                "Copy Results",
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = copy_workspace.update(cx, |workspace, cx| {
                                    workspace.copy_results_as(Format::Tsv, cx);
                                });
                            })
                        }))
                        // Named, not one button over a menu: the choice is
                        // between two things, and a control that opens
                        // another control to ask which is a click spent on
                        // nothing. It also puts the format on screen, which
                        // a lone "Export" left to the file extension.
                        .children(show_result_actions.then(|| {
                            button("export-csv", "Export CSV", Tone::Quiet, Control::Compact, t)
                                .on_click(move |_, _, cx| {
                                    _ = csv_workspace.update(cx, |workspace, cx| {
                                        workspace.export_results(Format::Csv, cx);
                                    });
                                })
                        }))
                        .children(show_result_actions.then(|| {
                            button(
                                "export-json",
                                "Export JSON",
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = json_workspace.update(cx, |workspace, cx| {
                                    workspace.export_results(Format::Json, cx);
                                });
                            })
                        }))
                        // A tinted cell scrolled out of view is an edit
                        // nobody knows is about to be written.
                        .children(has_pending.then(|| {
                            let label = div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(t.text_muted)
                                .child(format!(
                                    "{pending_count} pending {}",
                                    if pending_count == 1 { "edit" } else { "edits" }
                                ));
                            // Behind Structure the grid is not drawn, and a
                            // walk over it would move a ring nobody sees.
                            if !grid_shown {
                                return label.into_any_element();
                            }
                            div()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .child(
                                    icon_button(
                                        "previous-edit",
                                        icon::CHEVRON_LEFT,
                                        Tone::Quiet,
                                        Control::Compact,
                                        t,
                                    )
                                    .tooltip("Previous edit")
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = previous_workspace.update(cx, |workspace, cx| {
                                                workspace.previous_edit(&PreviousEdit, window, cx);
                                            });
                                        },
                                    ),
                                )
                                .child(label)
                                .child(
                                    icon_button(
                                        "next-edit",
                                        icon::CHEVRON_RIGHT,
                                        Tone::Quiet,
                                        Control::Compact,
                                        t,
                                    )
                                    .tooltip("Next edit")
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = next_workspace.update(cx, |workspace, cx| {
                                                workspace.next_edit(&NextEdit, window, cx);
                                            });
                                        },
                                    ),
                                )
                                .into_any_element()
                        }))
                        .children(has_pending.then(|| {
                            button("discard-edits", "Discard", Tone::Quiet, Control::Compact, t)
                                .on_click(move |_, window, cx| {
                                    _ = discard_workspace.update(cx, |workspace, cx| {
                                        workspace.discard_edits(&DiscardEdits, window, cx);
                                    });
                                })
                        }))
                        .children(has_pending.then(|| {
                            button(
                                "apply-edits",
                                "Apply edits",
                                Tone::Primary,
                                Control::Compact,
                                t,
                            )
                            .on_click(move |_, window, cx| {
                                _ = apply_workspace.update(cx, |workspace, cx| {
                                    workspace.apply_edits(&ApplyEdits, window, cx);
                                });
                            })
                        })),
                )
        });

        let content = div()
            // Flush, not a floating card: the split handle already draws the
            // one seam, and the planes inside separate by tone.
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(views::render_main_content(
                profile,
                self.editor_size(),
                (self.chrome_zoom, self.editor_zoom, self.grid_zoom),
                &self.row_panel,
                self.plan_copied,
                &self.tab_strip,
                self.custom_limit.as_ref(),
                cx,
            )))
            .children(results_status);
        // The strip's chips glide for as long as the render above found one
        // still on its way.
        self.tab_strip.shift.drive(window);
        // With the sidebar folded there is nothing to split, and a split with
        // one panel still paints the handle it no longer divides anything with.
        let main_pane = if self.sidebar_hidden {
            content.into_any_element()
        } else {
            h_resizable("workspace-shell-split")
                .with_state(&self.shell_split)
                .child(
                    resizable_panel()
                        .size(px(layout::SIDEBAR_DEFAULT_WIDTH))
                        .flex_none()
                        .size_range(px(layout::SIDEBAR_MIN_WIDTH)..px(layout::SIDEBAR_MAX_WIDTH))
                        .child(
                            div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_h_0()
                                        .child(self.render_explorer(profile, cx)),
                                )
                                .child(
                                    div()
                                        .h(px(layout::chrome(layout::STATUS_HEIGHT)))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .px(px(layout::SPACE_MD))
                                        .border_t_1()
                                        .border_color(t.border)
                                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                                        .overflow_hidden()
                                        .children(sidebar_status),
                                ),
                        ),
                )
                .child(resizable_panel().child(content))
                .into_any_element()
        };

        div()
            .id("workspace")
            .relative()
            // The floor under the focus, so a surface with nothing focusable
            // on it still has a dispatch path for the workspace's own
            // bindings. An inner element that can take focus claims it first
            // and stops this one from taking it back.
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::explain_query))
            .on_action(cx.listener(Self::format_query))
            .on_action(cx.listener(Self::toggle_comment))
            .on_action(cx.listener(Self::choose_mode))
            .on_action(cx.listener(Self::reset_confirmations))
            .on_action(cx.listener(Self::cancel_query))
            .on_action(cx.listener(Self::apply_edits))
            .on_action(cx.listener(Self::discard_edits))
            .on_action(cx.listener(Self::next_edit))
            .on_action(cx.listener(Self::previous_edit))
            .on_action(cx.listener(Self::sort_column))
            .on_action(cx.listener(Self::set_row_limit))
            .on_action(cx.listener(Self::set_query_limit))
            .on_action(cx.listener(Self::set_default_row_limit_action))
            .on_action(cx.listener(Self::custom_row_limit))
            .on_action(cx.listener(Self::refresh_active_relation))
            .on_action(cx.listener(Self::refresh_connection))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::previous_page))
            .on_action(cx.listener(Self::clear_filter))
            .on_action(cx.listener(Self::add_filter))
            .on_action(cx.listener(Self::remove_filter))
            .on_action(cx.listener(Self::set_filter_column))
            .on_action(cx.listener(Self::set_filter_raw))
            .on_action(cx.listener(Self::set_filter_operator))
            .on_action(cx.listener(Self::toggle_filter_join))
            .on_action(cx.listener(Self::toggle_next_join))
            .on_action(cx.listener(Self::new_row))
            .on_action(cx.listener(Self::show_editor))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::select_database))
            .on_action(cx.listener(Self::save_query))
            .on_action(cx.listener(Self::new_query))
            .on_action(cx.listener(Self::next_profile))
            .on_action(cx.listener(Self::previous_profile))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::open_connection_form))
            .on_action(cx.listener(Self::new_project))
            .on_action(cx.listener(Self::import_from))
            .on_action(cx.listener(Self::zoom_editor_in))
            .on_action(cx.listener(Self::zoom_editor_out))
            .on_action(cx.listener(Self::reset_editor_zoom))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::fuzzy_open))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::palette_next))
            .on_action(cx.listener(Self::palette_previous))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_row_panel))
            .on_action(cx.listener(Self::accept_completion))
            .on_action(cx.listener(Self::open_settings))
            .size_full()
            // The shell is the frost: titlebar, sidebar and status bar paint
            // nothing of their own, they are the glass the window root already
            // laid down. The editor and the results step forward from it by
            // tone, and by letting less of the desktop through.
            .text_color(t.text)
            .text_size(px(layout::chrome(layout::TEXT_MD)))
            .flex()
            .flex_col()
            .child(titlebar(
                t,
                profile.color.filter(|_| self.settings.color_titlebar),
                std::iter::once({
                    let mode = profile.mode;
                    let silenced = profile
                        .confirmed
                        .iter()
                        .map(|kind| kind.label())
                        .chain(profile.confirmed_stale.then_some("stale rows"))
                        .collect::<Vec<_>>();
                    ui::mode_pill(t, mode)
                        .dropdown_menu(move |menu, _, _| {
                            let menu = Mode::ALL.into_iter().fold(menu, |menu, option| {
                                menu.menu_with_check(
                                    option.label(),
                                    option == mode,
                                    Box::new(SetMode { mode: option }),
                                )
                            });
                            // Absent unless this connection has actually
                            // silenced something: suppression is per
                            // connection, so the way back out belongs on the
                            // connection, and one it never asked to reset is
                            // one entry with nothing to do.
                            if silenced.is_empty() {
                                menu
                            } else {
                                menu.separator().menu(
                                    format!(
                                        "Reset silenced confirmations ({})",
                                        silenced.join(", ")
                                    ),
                                    Box::new(ResetConfirmations),
                                )
                            }
                        })
                        .into_any_element()
                })
                .chain(newer_release.map(|release| {
                    // On a wrapper: `dropdown_menu` wraps the button, so a
                    // margin on the button never reaches the titlebar's row.
                    div()
                        .ml_auto()
                        .child(ui::update_pill(t, &release.version).dropdown_menu(
                            move |menu, _, _| {
                                menu.label("Update it with your package manager,")
                                    .label("or download it from GitHub.")
                                    .separator()
                                    .link(
                                        format!("Download {}", release.version),
                                        release.url.clone(),
                                    )
                            },
                        ))
                        .into_any_element()
                }))
                .collect(),
                vec![
                    icon_button(
                        "toggle-sidebar",
                        icon::SIDEBAR,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.toggle_sidebar(&ToggleSidebar, window, cx);
                    }))
                    .into_any_element(),
                    self.render_profile_switcher(cx),
                ],
                vec![
                    icon_button(
                        "open-settings",
                        icon::SETTINGS,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenSettings), cx);
                    })
                    .into_any_element(),
                ],
            ))
            .child(div().flex_1().min_h_0().child(main_pane))
            .children(self.render_apply_review(cx))
            .children(self.render_close_confirmation(cx))
            .children(self.render_discard_confirmation(cx))
            .children(self.render_pending_run(cx))
            .children(self.render_queue_failure(cx))
            .children(self.render_stale_edit(cx))
            .children(self.settings_open.then(|| views::render_settings(self, cx)))
            .children(self.render_import_choice(cx))
            .children(self.render_palette(cx))
            .children(self.render_reference_popup(cx))
    }
}

/// A relation a reference arrow lists, by its index into the structure's
/// `referenced_by`: a row was found in it, or its check failed with this error.
pub(crate) type ReferenceAnswer = (usize, gpui::SharedString, Result<(), String>);

/// What a reference arrow opened: where it hangs, and the relations that turned
/// out to hold rows for the key, or whose check failed with the error it gave.
/// `None` while the lookup is still out.
pub(crate) struct ReferencePopup {
    pub(crate) at: gpui::Point<gpui::Pixels>,
    pub(crate) choices: Option<Vec<ReferenceAnswer>>,
    check: u64,
}

pub(crate) const EDITOR_FONT_SIZE_DEFAULT: f32 = 14.0;

pub(crate) const EDITOR_FONT_SIZE_MIN: f32 = 11.0;

pub(crate) const EDITOR_FONT_SIZE_MAX: f32 = 24.0;

pub(crate) const FONT_SIZE_STEP: f32 = 1.0;

/// The range the controls offer for a slot, and where Reset puts it. The
/// chrome stops at 17 because the titlebar does not grow with it: the window
/// buttons are placed against it once, when the window opens, and past 17 the
/// controls in it would touch its edges.
/// Blocking work on a thread of its own, for the background executor to await
/// without holding one of its threads: on Linux it has only as many as there
/// are cores, and a run parked on the server for each of them would stall
/// everything else spawned there, a Cancel included.
pub(crate) async fn on_own_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || sender.send(work()));
    // Dropped unsent only by a panic on that thread, which is a panic here.
    receiver
        .await
        .expect("the thread running blocking work panicked")
}

pub(crate) fn font_size_range(slot: FontSlot) -> (f32, f32, f32) {
    match slot {
        FontSlot::Chrome => (11.0, layout::BODY_FONT_SIZE, 17.0),
        FontSlot::Editor => (
            EDITOR_FONT_SIZE_MIN,
            EDITOR_FONT_SIZE_DEFAULT,
            EDITOR_FONT_SIZE_MAX,
        ),
        FontSlot::Grid => (10.0, layout::BODY_FONT_SIZE, 20.0),
    }
}

pub(crate) fn adjusted_font_size(slot: FontSlot, current: f32, delta: f32) -> f32 {
    let (min, _, max) = font_size_range(slot);
    (current + delta).clamp(min, max)
}

/// A size read back from disk. Clamped rather than trusted, because
/// `profiles.toml` is a text file: a size outside the range the controls offer
/// would otherwise be unreachable by the controls that set it. The finiteness
/// check is not decoration -- `clamp` on a NaN returns the NaN.
pub(crate) fn restored_font_size(slot: FontSlot, stored: Option<f32>) -> f32 {
    let (min, default, max) = font_size_range(slot);
    stored
        .filter(|size| size.is_finite())
        .map(|size| size.clamp(min, max))
        .unwrap_or(default)
}

/// The steps a browser zooms through, so ⌘+ and ⌘− land where people expect.
pub(crate) const ZOOM_LEVELS: [u32; 11] = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200];

/// The next level in or out from `current`, or `current` itself at either end.
pub(crate) fn stepped_zoom(current: u32, zoom_in: bool) -> u32 {
    let next = if zoom_in {
        ZOOM_LEVELS.into_iter().find(|&level| level > current)
    } else {
        ZOOM_LEVELS.into_iter().rev().find(|&level| level < current)
    };
    next.unwrap_or(current)
}

pub(crate) fn zoomed(size: f32, percent: u32) -> f32 {
    size * percent as f32 / 100.0
}

pub(crate) fn adjusted_opacity(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(theme::OPACITY_MIN, theme::OPACITY_MAX)
}

/// An opacity read back from disk, clamped for the same reason the zoom is:
/// `profiles.toml` is a text file, and a value outside the range the controls
/// offer is one they cannot walk back. NaN would survive the `clamp`.
pub(crate) fn restored_opacity(stored: Option<f32>) -> f32 {
    stored
        .filter(|opacity| opacity.is_finite())
        .map(|opacity| opacity.clamp(theme::OPACITY_MIN, theme::OPACITY_MAX))
        .unwrap_or(theme::OPACITY_DEFAULT)
}

pub(crate) fn opacity_percent(opacity: f32) -> u32 {
    (opacity * 100.0).round() as u32
}

/// A percentage someone typed, back into the fraction the theme wants.
///
/// Out of range is clamped rather than refused -- the field is a shortcut past
/// the `−` and `+` buttons, and those clamp too. Anything that is not a number
/// at all, the empty field included, leaves the setting where it was; so does
/// an infinity, which parses happily and would survive `clamp`.
///
/// The number already on screen is returned as the very f32 it came from
/// rather than recomputed: stepping lands on values like 0.77000004, whose
/// percentage divides back to a different f32, and `set_opacity` would take
/// that for a change and rewrite `profiles.toml`.
pub(crate) fn opacity_from_percent_input(typed: &str, current: f32) -> f32 {
    let Ok(percent) = typed.trim().trim_end_matches('%').trim().parse::<f32>() else {
        return current;
    };
    if !percent.is_finite() || percent.round() == opacity_percent(current) as f32 {
        return current;
    }
    (percent / 100.0).clamp(theme::OPACITY_MIN, theme::OPACITY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_too_narrow_to_hold_the_sidebar_does_not_relabel_the_squeeze() {
        // 400 wanted, container only leaves room for 350: Resize, not Adopt,
        // even though the achieved size differs from what was asked for.
        assert_eq!(
            sidebar_settle(px(450.), px(1200.), px(350.), px(400.)),
            SidebarSettle::Resize
        );
        // Settled at the squeezed width: another frame at the same container
        // size must leave the remembered target alone.
        assert_eq!(
            sidebar_settle(px(450.), px(450.), px(350.), px(350.)),
            SidebarSettle::Settled
        );
        // Widening back out is another container change, so the target gets
        // another chance to apply in full.
        assert_eq!(
            sidebar_settle(px(1200.), px(450.), px(350.), px(350.)),
            SidebarSettle::Resize
        );
    }

    #[test]
    fn a_drag_at_a_fixed_container_size_is_adopted() {
        assert_eq!(
            sidebar_settle(px(1200.), px(1200.), px(300.), px(400.)),
            SidebarSettle::Adopt
        );
    }

    #[test]
    fn a_font_size_stays_inside_its_slots_range() {
        for slot in [FontSlot::Chrome, FontSlot::Editor, FontSlot::Grid] {
            let (min, default, max) = font_size_range(slot);
            assert_eq!(adjusted_font_size(slot, max, FONT_SIZE_STEP), max);
            assert_eq!(adjusted_font_size(slot, min, -FONT_SIZE_STEP), min);
            assert_eq!(
                adjusted_font_size(slot, default, FONT_SIZE_STEP),
                default + FONT_SIZE_STEP
            );
        }
    }

    #[test]
    fn a_restored_font_size_is_clamped_rather_than_trusted() {
        // `profiles.toml` is a text file. A size outside the range the controls
        // offer would be one the controls cannot undo, and a NaN would survive
        // `clamp` and reach the text system.
        for slot in [FontSlot::Chrome, FontSlot::Editor, FontSlot::Grid] {
            let (min, default, max) = font_size_range(slot);
            assert_eq!(restored_font_size(slot, None), default);
            assert_eq!(restored_font_size(slot, Some(f32::NAN)), default);
            assert_eq!(restored_font_size(slot, Some(f32::INFINITY)), default);
            assert_eq!(restored_font_size(slot, Some(900.0)), max);
            assert_eq!(restored_font_size(slot, Some(0.0)), min);
            assert_eq!(
                restored_font_size(slot, Some(default + FONT_SIZE_STEP)),
                default + FONT_SIZE_STEP
            );
        }
    }

    #[test]
    fn opacity_is_stored_per_theme() {
        let [first, second, ..] = Theme::all();
        let mut settings = Settings::default();
        settings.set_opacity_for(&first, 0.9);
        assert_eq!(settings.opacity_for(&first), 0.9);
        assert_eq!(settings.opacity_for(&second), settings.opacity);
        settings.set_opacity_for(&second, 0.6);
        assert_eq!(settings.opacity_for(&first), 0.9);
        assert_eq!(settings.opacity_for(&second), 0.6);
    }

    #[test]
    fn light_glass_without_an_entry_starts_at_its_default_not_the_shared_fallback() {
        let settings = Settings {
            opacity: 0.6,
            ..Settings::default()
        };
        let light = Theme::all()
            .into_iter()
            .find(|t| t.is_glass && t.appearance == theme::Appearance::Light)
            .unwrap();
        assert_eq!(settings.opacity_for(&light), 0.79);
        assert_eq!(settings.opacity_for(&Theme::glass()), 0.6);
        let mut settings = settings;
        settings.set_opacity_for(&light, 0.5);
        assert_eq!(settings.opacity_for(&light), 0.5);
    }

    #[test]
    fn a_restored_opacity_is_clamped_rather_than_trusted() {
        assert_eq!(restored_opacity(None), theme::OPACITY_DEFAULT);
        assert_eq!(restored_opacity(Some(f32::NAN)), theme::OPACITY_DEFAULT);
        assert_eq!(restored_opacity(Some(2.0)), theme::OPACITY_MAX);
        assert_eq!(restored_opacity(Some(-1.0)), theme::OPACITY_MIN);
        assert_eq!(restored_opacity(Some(0.8)), 0.8);
    }

    #[test]
    fn a_typed_opacity_is_clamped_rather_than_refused() {
        assert_eq!(opacity_from_percent_input("80", 0.72), 0.8);
        assert_eq!(opacity_from_percent_input("85%", 0.72), 0.85);
        assert_eq!(opacity_from_percent_input("  85 % ", 0.72), 0.85);
        assert_eq!(opacity_from_percent_input("120", 0.72), theme::OPACITY_MAX);
        assert_eq!(opacity_from_percent_input("10", 0.72), theme::OPACITY_MIN);
        // Nothing to read is not a reason to change anything.
        assert_eq!(opacity_from_percent_input("", 0.72), 0.72);
        assert_eq!(opacity_from_percent_input("dark", 0.72), 0.72);
        assert_eq!(opacity_from_percent_input("inf", 0.72), 0.72);
        // Retyping what the readout says must be the same f32 it was showing,
        // not the one the division would have produced.
        let stepped = adjusted_opacity(theme::OPACITY_DEFAULT, theme::OPACITY_STEP);
        assert_eq!(
            opacity_from_percent_input(&opacity_percent(stepped).to_string(), stepped),
            stepped
        );
    }

    #[test]
    fn zoom_steps_through_the_levels_and_stops_at_either_end() {
        assert_eq!(stepped_zoom(100, true), 110);
        assert_eq!(stepped_zoom(100, false), 90);
        assert_eq!(stepped_zoom(200, true), 200);
        assert_eq!(stepped_zoom(50, false), 50);
        let mut zoom = 50;
        for level in ZOOM_LEVELS.into_iter().skip(1) {
            zoom = stepped_zoom(zoom, true);
            assert_eq!(zoom, level);
        }
    }
}

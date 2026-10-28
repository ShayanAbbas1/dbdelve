//! Opening, saving, restoring and switching between connections.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use gpui_component::menu::PopupMenuItem;

use crate::connection_form::{ConnectionTest, duplicate_profile_name, keep_schema};
use crate::import::{self, Source};
use crate::session::STALE_ROWS;
use crate::sql::{Destructive, Mode};

use std::path::Path;

impl Workspace {
    pub(crate) fn remember_profiles(&mut self, cx: &mut Context<Self>) {
        // The file we could not read at startup is still the user's, and what
        // this session holds is not what they have -- so it must not flatten
        // it. The notice said nothing would be saved.
        if self.store_unreadable {
            return;
        }
        let profiles = self
            .profiles
            .iter()
            .map(|profile| profile.stored())
            .collect::<Vec<_>>();
        let active = self.profile().map(|profile| profile.id.clone());
        let picked = fonts(cx);
        let fonts = store::StoredFonts {
            chrome: Some(picked.chrome.to_string()),
            editor: Some(picked.editor.to_string()),
            grid: Some(picked.grid.to_string()),
        };
        let settings = store::StoredSettings {
            // Mid-preview, the installed theme is only being tried on.
            theme: Some(
                self.theme_before_preview
                    .unwrap_or(*theme(cx))
                    .name
                    .to_string(),
            ),
            match_system_appearance: Some(self.settings.match_system_appearance),
            light_theme: Some(self.settings.light_theme.clone()),
            dark_theme: Some(self.settings.dark_theme.clone()),
            chrome_font_size: Some(self.settings.chrome_font_size),
            editor_font_size: Some(self.settings.editor_font_size),
            grid_font_size: Some(self.settings.grid_font_size),
            preview_rows: Some(self.settings.row_limit.unwrap_or(0)),
            opacity: Some(self.settings.opacity),
            check_for_updates: Some(self.settings.check_for_updates),
            color_titlebar: Some(self.settings.color_titlebar),
            client_sort: Some(self.settings.client_sort),
            welcomed: Some(self.settings.welcomed),
            custom_keybindings: Some(self.settings.custom_keybindings.clone()),
            theme_opacity: Some(self.settings.theme_opacity.clone()),
        };
        if let Err(message) = store::save_profiles(&store::ProfileList {
            profiles,
            active,
            fonts: Some(fonts),
            settings: Some(settings),
            projects: self.projects.clone(),
            unavailable: self.unavailable.clone(),
        }) {
            self.note(message, cx);
        }
    }

    pub(crate) fn restore_profile(
        &mut self,
        stored: store::StoredProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No mode at all is a profile written before dbdelve had TLS, and
        // `prefer` is exactly what it was connecting as. A mode this build
        // cannot read is the other case, and it fails closed: whatever was
        // asked for, it was not something weaker than the strictest rung.
        let (sslmode, unreadable_mode) = match stored.sslmode.as_deref() {
            None => (SslMode::default(), None),
            Some(stored) => match SslMode::parse(stored) {
                Ok(mode) => (mode, None),
                Err(message) => (SslMode::VerifyFull, Some(message)),
            },
        };
        // No engine at all is a profile written before dbdelve had a second one,
        // and Postgres is what it was. An engine this build cannot read is held
        // back by `decode_profiles` as an `UnavailableProfile`, so the `Err` arm
        // is reached only by a file the mixed reader fell back on: it is read
        // as Postgres and says so rather than connecting somewhere the user did
        // not ask for without mentioning it.
        let (engine, unreadable_engine) = match stored.engine.as_deref() {
            None => (Engine::Postgres, None),
            Some(stored) => match Engine::parse(stored) {
                Ok(engine) => (engine, None),
                Err(message) => (Engine::Postgres, Some(message)),
            },
        };
        let config = stored_config(&stored, engine, sslmode);
        let stored_queries = stored_buffers(
            stored.open_queries,
            stored.next_query_id,
            stored.open_query.clone(),
        );
        // Snapshots whose tab is gone -- a renamed table strands its file
        // under the old name, and nothing else will ever remove it.
        // A buffer's queued results are live too, and the count is what says
        // how many: a tab that ran fewer statements the second time leaves the
        // rest behind, and they are orphans the moment it does.
        let live_grids =
            stored_queries
                .iter()
                .flat_map(|tab| {
                    std::iter::once(store::query_grid_key(tab.id)).chain(
                        (0..tab.queued_results).map(|index| store::queued_grid_key(tab.id, index)),
                    )
                })
                .chain(stored.open_objects.iter().map(|object| {
                    store::object_grid_key(&object.schema, &object.name, &object.filter)
                }))
                .collect();
        // The grid directory is the id's, so when an unavailable profile holds
        // the id too, its tabs' snapshots are in there as well, and none of
        // them can be told apart as stale.
        if !held_by_unavailable(&stored.id, &self.unavailable) {
            store::prune_grids(&stored.id, &live_grids);
        }
        let mut session = Session::new(
            stored.id.clone(),
            stored_queries,
            stored.next_query_id.unwrap_or(0),
            stored.open_objects,
            config.engine(),
            Sorting::new(self.settings.client_sort),
            self.settings.row_limit,
            window,
            cx,
        );
        // An unreadable sslmode only means anything to an engine that has one.
        let notice = unreadable_engine
            .map(|message| format!("{message} Reading it as Postgres."))
            .or_else(|| {
                unreadable_mode
                    .filter(|_| config.server().is_some())
                    .map(|message| format!("{message} Connecting as verify-full."))
            });
        if let Some(message) = notice {
            session.notice = Some(message);
        }
        let id = stored.id.clone();
        self.profiles.push(Profile {
            id: stored.id,
            name: stored.name,
            config,
            // A slug this build cannot read is decoration, so it drops to no
            // colour rather than refusing the profile it was written on.
            color: stored.color.as_deref().and_then(ConnectionColor::from_slug),
            // No mode at all is a profile written before modes existed, and
            // Read-write is what it has always been connecting as. A slug this
            // build cannot read was written by a build that has a mode this one
            // does not, and it fails closed to Read-only: whatever it named, it
            // was not a licence this build can vouch for, and the badge says
            // Read-only where the user can see it and raise it in one click.
            // Either way the profile loads -- the alternative was every
            // connection in the file becoming unreadable at once.
            mode: stored.mode.as_deref().map_or(Mode::default(), |slug| {
                Mode::from_slug(slug).unwrap_or(Mode::ReadOnly)
            }),
            // A silenced kind this build cannot read is dropped, which only
            // means that kind still asks.
            confirmed: stored
                .confirmed
                .iter()
                .filter_map(|slug| Destructive::from_slug(slug))
                .collect(),
            confirmed_stale: stored.confirmed.iter().any(|slug| slug == STALE_ROWS),
            generation: 0,
            state: ProfileState::Idle,
            in_transaction: false,
            session_changed: false,
            catalog: CatalogState::Loading,
            databases: Databases::default(),
            left_database: false,
            session,
        });
        // The saved queries are listed before anything has connected.
        self.refresh_explorer(&id, cx);
    }

    // Eight, because a connection is eight things and a struct holding them for
    // two call sites would be a parameter list with extra steps.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_profile(
        &mut self,
        name: String,
        config: ConnectionConfig,
        color: Option<ConnectionColor>,
        mode: Mode,
        origin: Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // An unavailable profile's id is taken too: reusing it would hand the
        // new profile its keychain entry and saved queries.
        let existing = self
            .profiles
            .iter()
            .map(|profile| profile.id.clone())
            .chain(self.unavailable.iter().map(|gone| gone.id.clone()))
            .collect::<Vec<_>>();
        let id = store::profile_id(&name, &existing);
        let session = Session::new(
            id.clone(),
            first_buffer(None),
            0,
            Vec::new(),
            config.engine(),
            Sorting::new(self.settings.client_sort),
            self.settings.row_limit,
            window,
            cx,
        );
        let password = password_to_persist(&config, origin).map(str::to_string);
        self.profiles.push(Profile {
            id: id.clone(),
            name,
            config,
            color,
            mode,
            confirmed: Vec::new(),
            confirmed_stale: false,
            generation: 0,
            state: ProfileState::Idle,
            in_transaction: false,
            session_changed: false,
            catalog: CatalogState::Loading,
            databases: Databases::default(),
            left_database: false,
            session,
        });
        if let Some(password) = password
            && let Err(message) = store::set_password(&id, &password)
        {
            self.note(message, cx);
        }
        self.remember_profiles(cx);
        self.profiles.len() - 1
    }

    /// The Keychain is read in the background, as `begin_connect` does. A failed
    /// read seeds no password rather than an error: the user just retypes it.
    pub(crate) fn duplicate_profile(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let id = profile.id.clone();
        let name = duplicate_profile_name(
            &profile.name,
            &self
                .profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect::<Vec<_>>(),
        );

        let password_task = {
            let id = id.clone();
            cx.background_executor()
                .spawn(async move { store::password(&id).ok().flatten() })
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let password = password_task.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                // Removed while the Keychain answered --
                // nothing left to seed a form from.
                let Some(profile) = workspace.profiles.iter().find(|profile| profile.id == id)
                else {
                    return;
                };
                let mut form = ConnectionForm::duplicating(profile, name, password, window, cx);
                form.project = workspace.group_of(&id).map(str::to_string);
                workspace.form = Some(form);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fills the open form from the URL in its URL field. Shared by the form's
    /// own field, pasted into or entered, and the welcome surface's, so a URL
    /// given to either lands the same way.
    pub(crate) fn fill_from_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let url = form.url.read(cx).value();
        form.last_url = url.clone();
        let text = url.trim();
        let parsed = match super::welcome::recognise(text) {
            super::welcome::Recognised::File(Engine::DuckDb) => {
                Ok(ConnectionConfig::duckdb(text.to_string(), 0))
            }
            super::welcome::Recognised::File(_) => Ok(ConnectionConfig::Sqlite {
                path: text.to_string(),
                statement_timeout: 0,
            }),
            _ => ConnectionConfig::from_url(text),
        };
        let config = match parsed {
            Ok(config) => config,
            Err(error) => {
                form.error = Some(error);
                cx.notify();
                return;
            }
        };

        // Only the fields the URL's own engine has. Blanking the others would
        // throw away a half-typed connection to a different database, which the
        // user never asked to lose by pasting a URL.
        let filled = match &config {
            ConnectionConfig::Sqlite { path, .. }
            | ConnectionConfig::Turso { path, .. }
            | ConnectionConfig::DuckDb { path, .. } => vec![
                (&form.name, default_profile_name(&config)),
                (&form.path, path.clone()),
            ],
            ConnectionConfig::Postgres(server)
            | ConnectionConfig::MySql(server)
            | ConnectionConfig::MariaDb(server)
            | ConnectionConfig::SqlServer(server)
            | ConnectionConfig::ClickHouse(server)
            | ConnectionConfig::Redis(server)
            | ConnectionConfig::MongoDb(crate::db::MongoConfig { server, .. }) => vec![
                (&form.name, default_profile_name(&config)),
                (&form.host, server.host.clone()),
                (
                    &form.port,
                    server.port.map(|port| port.to_string()).unwrap_or_default(),
                ),
                (&form.database, server.database.clone()),
                (&form.user, server.user.clone()),
                (&form.password, server.password.clone()),
                (
                    &form.root_certificate,
                    server.root_certificate.clone().unwrap_or_default(),
                ),
            ],
            // `from_url` refuses the scheme, so no URL arrives as one.
            ConnectionConfig::Snowflake(_) => Vec::new(),
        };
        let mongo = match &config {
            ConnectionConfig::MongoDb(mongo) => Some(mongo),
            _ => None,
        };
        let filled = filled
            .into_iter()
            .chain(mongo.map(|mongo| (&form.options, mongo.options.clone())));
        for (input, value) in filled {
            let input = input.clone();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }

        let engine = config.engine();
        let sslmode = config.server().map(|server| server.sslmode);
        let srv = mongo.is_some_and(|mongo| mongo.srv);
        if let Some(form) = &mut self.form {
            // A URL for another kind of engine unmounts the field it was
            // pasted into, and `set_engine` hands focus on.
            form.set_engine(engine);
            form.srv = srv;
            if let Some(sslmode) = sslmode {
                // The URL's own mode, so pasting one that demands verification
                // cannot land in a form still set to `prefer`.
                form.sslmode = sslmode;
            }
            form.error = None;
        }
        cx.notify();
    }

    /// An edit to the connection form's URL field. A URL that arrives whole
    /// fills the fields at once; one typed a character at a time waits for
    /// Enter, or every keystroke would fill them from half a URL.
    pub(crate) fn url_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let now = form.url.read(cx).value();
        let before = std::mem::replace(&mut form.last_url, now.clone());
        if pasted_url(&before, &now) {
            self.fill_from_url(window, cx);
        }
        cx.notify();
    }

    /// A compact dropdown at the end of a row of the connection form: `pick`
    /// writes the choice into it.
    pub(crate) fn form_dropdown<T: Clone + PartialEq + 'static>(
        id: &'static str,
        selected: T,
        groups: Vec<Vec<T>>,
        label: impl Fn(&T) -> gpui::SharedString + 'static,
        marker: impl Fn(&T) -> Option<AnyElement> + 'static,
        pick: impl Fn(&mut ConnectionForm, T) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = popup_theme(cx);
        let trigger = ui::picker(id, label(&selected), t)
            .border_1()
            .border_color(t.border);
        Self::with_menu(
            trigger,
            selected,
            groups,
            label,
            marker,
            move |workspace, option, _, _| {
                if let Some(form) = &mut workspace.form {
                    pick(form, option);
                }
            },
            cx,
        )
    }

    /// The face of a field-shaped picker: its current choice and a chevron.
    fn field_trigger(
        id: impl Into<gpui::ElementId>,
        marker: Option<AnyElement>,
        label: gpui::SharedString,
        cx: &mut Context<Self>,
    ) -> gpui_component::button::Button {
        let t = *theme(cx);
        ui::control(id, Tone::Quiet, Control::Standard)
            .w_full()
            .px(px(layout::SPACE_SM))
            .border_1()
            .border_color(t.border)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .text_size(px(layout::chrome(layout::TEXT_MD)))
                    .text_color(t.text)
                    .children(marker)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(label),
                    )
                    .child(row_icon(t, icon::CHEVRON_DOWN)),
            )
    }

    /// A field-shaped button that opens a menu of `groups` of options, with a
    /// separator between groups: the pickers on the connection form and the
    /// import's project. `pick` makes the choice; `marker` is what is drawn
    /// before a label, if anything.
    pub(crate) fn dropdown<T: Clone + PartialEq + 'static>(
        id: impl Into<gpui::ElementId>,
        selected: T,
        groups: Vec<Vec<T>>,
        label: impl Fn(&T) -> gpui::SharedString + 'static,
        marker: impl Fn(&T) -> Option<AnyElement> + 'static,
        pick: impl Fn(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let trigger = Self::field_trigger(id, marker(&selected), label(&selected), cx);
        Self::with_menu(trigger, selected, groups, label, marker, pick, cx)
    }

    /// The menu of `groups` hung off `trigger`, whatever it is dressed as.
    fn with_menu<T: Clone + PartialEq + 'static>(
        trigger: gpui_component::button::Button,
        selected: T,
        groups: Vec<Vec<T>>,
        label: impl Fn(&T) -> gpui::SharedString + 'static,
        marker: impl Fn(&T) -> Option<AnyElement> + 'static,
        pick: impl Fn(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let label = Rc::new(label);
        let marker = Rc::new(marker);
        let pick = Rc::new(pick);
        trigger
            .dropdown_menu(move |menu, _, _| {
                groups.iter().enumerate().fold(
                    menu.scrollable(true).max_h(px(layout::MENU_MAX_HEIGHT)),
                    |menu, (index, group)| {
                        let menu = if index > 0 { menu.separator() } else { menu };
                        group.iter().fold(menu, |menu, option| {
                            let workspace = workspace.clone();
                            let (label, marker, pick) =
                                (label.clone(), marker.clone(), pick.clone());
                            let checked = *option == selected;
                            let shown = option.clone();
                            let option = option.clone();
                            menu.item(
                                PopupMenuItem::element(move |_, _| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(layout::SPACE_XS))
                                        .children(marker(&shown))
                                        .child(label(&shown))
                                })
                                .checked(checked)
                                .on_click(move |_, window, cx| {
                                    _ = workspace.update(cx, |workspace, cx| {
                                        pick(workspace, option.clone(), window, cx);
                                        cx.notify();
                                    });
                                }),
                            )
                        })
                    },
                )
            })
            .into_any_element()
    }

    /// The project is made when the form is saved, so "New project…" only
    /// swaps the list for a field to name it in.
    pub(crate) fn project_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = popup_theme(cx);
        let form = self.form.as_ref().expect("drawn only on the form");
        if form.naming_project {
            return div()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .child(div().w(px(layout::FORM_PROJECT_NAME_WIDTH)).child(
                    gpui_component::Sizable::small(Input::new(&form.project_name)),
                ))
                .child(
                    icon_button(
                        "choose-project",
                        icon::CLOSE,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Choose an existing project")
                    .on_click(cx.listener(|workspace, _, _, cx| {
                        if let Some(form) = &mut workspace.form {
                            form.naming_project = false;
                            // The field going takes the focus with it.
                            form.needs_focus = Some(form.name.clone());
                            cx.notify();
                        }
                    })),
                )
                .into_any_element();
        }
        let existing = std::iter::once(ProjectChoice::In(None))
            .chain(
                self.projects
                    .iter()
                    .map(|project| ProjectChoice::In(Some(project.name.clone()))),
            )
            .collect();
        Self::form_dropdown(
            "project",
            ProjectChoice::In(form.project.clone()),
            vec![existing, vec![ProjectChoice::New]],
            |choice| match choice {
                ProjectChoice::In(None) => "No project".into(),
                ProjectChoice::In(Some(name)) => name.clone().into(),
                ProjectChoice::New => "New project…".into(),
            },
            move |choice| {
                let path = match choice {
                    ProjectChoice::In(None) => return None,
                    ProjectChoice::In(Some(_)) => icon::PROJECT,
                    ProjectChoice::New => icon::PLUS,
                };
                Some(row_icon(t, path).into_any_element())
            },
            |form, choice| match choice {
                ProjectChoice::In(project) => form.project = project,
                ProjectChoice::New => {
                    form.naming_project = true;
                    form.needs_focus = Some(form.project_name.clone());
                }
            },
            cx,
        )
    }

    pub(crate) fn connect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.save_form(window, cx);
    }

    /// The form's Connect, or its Save when editing, from the keyboard.
    pub(crate) fn submit_connection(
        &mut self,
        _: &SubmitConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The engine and project pickers open over the form, inside its key
        // context, and do not take the chord themselves.
        if self.palette.is_some() {
            return;
        }
        self.save_form(window, cx);
    }

    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let color = form.color;
        let mode = form.mode;
        let editing = form.editing.clone();
        let typed = form
            .naming_project
            .then(|| form.project_name.read(cx).value().to_string());
        let project =
            projects::chosen_project(typed.as_deref(), form.project.as_deref(), &self.projects);
        let ((name, mut config), project) = match form
            .config(cx)
            .and_then(|profile| project.map(|project| (profile, project)))
        {
            Ok(saved) => saved,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        if let Some(editing) = editing.as_deref()
            && let Some(profile) = self.profiles.iter().find(|profile| profile.id == editing)
        {
            keep_schema(&mut config, &profile.config);
        }

        if let Some(running) = editing.as_deref().and_then(|id| {
            self.profiles
                .iter()
                .find(|profile| profile.id == id)
                .filter(|profile| profile.config.needs_reconnect(&config))
                .and_then(|_| self.running_import_into(id))
        }) {
            if let Some(form) = &mut self.form {
                form.error = Some(format!(
                    "An import into {running} is running on this connection. Save once it ends."
                ));
            }
            cx.notify();
            return;
        }
        self.form = None;
        if let Some(project) = &project {
            self.project_named(project);
        }
        match editing {
            Some(id) => {
                self.save_profile(&id, name, config, color, window, cx);
                if self.group_of(&id) != project.as_deref()
                    && let Some(index) = self.profiles.iter().position(|profile| profile.id == id)
                {
                    self.move_to_project(index, project.as_deref(), cx);
                }
            }
            None => {
                let index =
                    self.create_profile(name, config, color, mode, Origin::Form, window, cx);
                if project.is_some() {
                    self.move_to_project(index, project.as_deref(), cx);
                }
                self.activate(index, cx);
            }
        }
    }

    /// Opens the form's connection and throws it away, so a profile can be
    /// checked before it is saved. The same open, keychain lookup and
    /// Read-only hold `begin_connect` does, or a pass here would not mean the
    /// connect will.
    pub(crate) fn test_connection(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.form else {
            return;
        };
        let mut config = match form.config(cx) {
            Ok((_, config)) => config,
            Err(error) => {
                form.error = Some(error);
                form.test = None;
                cx.notify();
                return;
            }
        };
        let editing = form.editing.clone();
        let mode = editing
            .as_ref()
            .and_then(|id| self.profiles.iter().find(|profile| &profile.id == id))
            .map_or(form.mode, |profile| profile.mode);
        form.error = None;

        let probe = cx.background_executor().spawn(async move {
            // A blank password on an existing profile means "keep the saved
            // one", which is what the connect would use.
            if let Some(id) = editing
                && let Some(server) = config.server_mut()
                && server.password.is_empty()
                && let Some(password) = store::password(&id)?
            {
                server.password = password;
            }
            let connection = Connection::open(config).map_err(|error| error.message)?;
            if mode == Mode::ReadOnly {
                connection
                    .set_read_only(true)
                    .map_err(|error| error.message)?;
            }
            Ok::<_, String>(())
        });
        let task = cx.spawn(async move |workspace, cx| {
            let result = probe.await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(form) = &mut workspace.form else {
                    return;
                };
                let finished = match result {
                    Ok(()) => ConnectionTest::Passed,
                    Err(message) => ConnectionTest::Failed(message),
                };
                if let Some(ConnectionTest::Running(task)) = form.test.replace(finished) {
                    // This task: dropping it here would cancel it mid-run.
                    task.detach();
                }
                cx.notify();
            });
        });
        form.test = Some(ConnectionTest::Running(task));
        cx.notify();
    }

    pub(crate) fn save_profile(
        &mut self,
        id: &str,
        name: String,
        config: ConnectionConfig,
        color: Option<ConnectionColor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Removed while the form sat open: there is nothing
        // left to save onto, and the id is the only handle the form kept.
        let Some(index) = self.profiles.iter().position(|profile| profile.id == id) else {
            cx.notify();
            return;
        };

        let password = password_to_persist(&config, Origin::Form).map(str::to_string);
        let profile = &mut self.profiles[index];
        let reconnect = profile.config.needs_reconnect(&config);
        let syntax = config.engine().syntax();
        if profile.config.engine().syntax() != syntax {
            profile.session.set_syntax(syntax, window, cx);
        }
        profile.name = name;
        profile.color = color;
        profile.config = config;

        // Only what was typed. A blank field is not an instruction to forget the
        // stored password.
        if let Some(password) = password
            && let Err(message) = store::set_password(id, &password)
        {
            self.note(message, cx);
        }
        self.remember_profiles(cx);

        if reconnect {
            self.reconnect(index, cx);
        }
        cx.notify();
    }

    /// Throw the profile's connection away and open a new one, whatever state
    /// the old one is in: a server that restarted leaves a `Connected` socket
    /// nothing answers on, and one that was down at connect leaves `Failed`
    /// with nothing that tries again.
    pub(crate) fn reconnect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(id) = self.profiles.get(index).map(|profile| profile.id.clone()) else {
            return;
        };
        if let Some(running) = self.running_import_into(&id) {
            self.note(
                format!(
                    "An import into {running} is running, so the connection was not refreshed."
                ),
                cx,
            );
            return;
        }
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        // The rows on an object tab were read over the old connection. Their
        // statement is dbdelve's own, so it re-runs the moment the tab is
        // looked at again -- a query tab holds SQL the user wrote and is
        // theirs to re-run.
        for tab in &mut profile.session.objects {
            if let ObjectBody::Relation { stale, .. } = &mut tab.body {
                *stale = true;
            }
        }
        self.begin_connect(index, cx);
    }

    pub(crate) fn refresh_connection(
        &mut self,
        _: &RefreshConnection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        self.reconnect(self.active, cx);
    }

    /// Ask the server which databases it holds, then offer them in the palette.
    pub(crate) fn select_database(
        &mut self,
        _: &SelectDatabase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let engine = profile.config.engine();
        if !engine.switches_database() {
            self.note(
                format!(
                    "A {} connection has no other database to switch to.",
                    engine.label()
                ),
                cx,
            );
            return;
        }
        let Some(connection) = profile.connection() else {
            self.note("The connection is not open.".into(), cx);
            return;
        };
        let (id, generation) = (profile.id.clone(), profile.generation);
        let fetch = cx
            .background_executor()
            .spawn(async move { connection.databases(engine) });
        cx.spawn_in(window, async move |workspace, cx| {
            let result = fetch.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                match result {
                    Ok(databases) => profile.databases = databases,
                    Err(error) => {
                        profile.session.notice = Some(error.message);
                        cx.notify();
                        return;
                    }
                }
                // Moved to another connection while the list was coming: the
                // palette would open over that one's databases. And
                // `open_palette` toggles, so one already up -- a second press,
                // another list -- is left as it is, as is the form.
                if workspace.profile().is_some_and(|profile| profile.id == id)
                    && workspace.palette.is_none()
                    && workspace.form.is_none()
                {
                    workspace.open_palette(PaletteMode::Database, window, cx);
                }
            });
        })
        .detach();
    }

    /// Move the profile in front onto another database on its server, leaving
    /// behind what it held of this one ([`Self::leave_database`]).
    pub(crate) fn set_database(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        if let Some(running) = self
            .profile()
            .and_then(|profile| self.running_import_into(&profile.id))
        {
            self.note(
                format!("An import into {running} is running, so the database was not switched."),
                cx,
            );
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(configured) = profile.config.database() else {
            return;
        };
        // As the profile would store it: Redis lists `db3` and stores `3`.
        let mut asked = profile.config.clone();
        asked.set_database(name.clone());
        let unmoved = asked.database() == Some(configured);
        if unmoved || already_on(configured, profile.databases.current.as_deref(), &name) {
            return;
        }
        let session = &profile.session;
        let unapplied = |results: &Entity<gpui_component::table::TableState<ResultGrid>>| {
            results.read(cx).delegate().has_pending()
        };
        if session.grids().any(unapplied) {
            let edited = session
                .objects
                .iter()
                .find(|tab| session.results(Tab::Object(tab.id)).is_some_and(unapplied));
            let message = match edited {
                Some(tab) => format!(
                    "{}.{} has cell edits that have not been applied, so the database was not \
                     switched.",
                    tab.schema, tab.name
                ),
                None => "A query tab has cell edits that have not been applied, so the database \
                         was not switched."
                    .into(),
            };
            self.note(message, cx);
            return;
        }

        self.leave_database(window, cx);

        let index = self.active;
        let Some(profile) = self.profile_mut() else {
            return;
        };
        // The database a DuckDB file opens as is held as none picked, so the
        // profile still opens on it once the file is renamed.
        let opened = profile
            .connection()
            .and_then(|connection| connection.current_database().map(str::to_string));
        let left = (
            profile.config.database().unwrap_or_default().to_string(),
            profile.config.schema().map(str::to_string),
        );
        profile
            .config
            .set_database(match opened.as_deref() == Some(name.as_str()) {
                true => String::new(),
                false => name,
            });
        let in_session = profile.config.engine().switches_database_in_session();
        self.remember_profiles(cx);
        match in_session {
            true => self.use_database(index, left, cx),
            false => self.reconnect(index, cx),
        }
    }

    /// Close what the profile in front holds of the database it is leaving.
    ///
    /// Object tabs go, and their grid snapshots with them: both are keyed by
    /// schema and name within the profile, with no database in the key, so
    /// one left open would name a relation that may not exist here and a
    /// relation of the same name would read the old one's rows back. Query
    /// tabs keep their SQL, which is the user's to run wherever they like, but
    /// drop their results and a New row form open on one: those rows belong
    /// to the database left behind, and an edit staged on them, or a row
    /// added beside them, would be applied to this one.
    fn leave_database(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let session = &profile.session;
        let active = session.active;
        let mut closing = session.objects.iter().map(|tab| tab.id).collect::<Vec<_>>();
        // The tab in front closes last, so the one it hands the front to is a
        // query tab that stays rather than a sibling about to close too.
        closing.sort_by_key(|id| Tab::Object(*id) == active);
        let queries = session.queries.iter().map(|tab| tab.id).collect::<Vec<_>>();
        let profile_id = profile.id.clone();
        // Not running, which a switch refuses and a connect cannot have: the
        // dialog names a table of the database being left.
        self.abandon_import(&profile_id, cx);
        for id in closing {
            self.close_object(id, window, cx);
        }
        // Before the reconnect: the cancel goes out over the old connection.
        for id in queries {
            self.stop_queue_on(Tab::Query(id), cx);
            self.stop_run(Tab::Query(id), cx);
        }

        let Some(profile) = self.profile_mut() else {
            return;
        };
        for object in profile.session.pending_objects.drain(..) {
            let key = store::object_grid_key(&object.schema, &object.name, &object.filter);
            let _ = store::remove_grid(&profile.id, &key);
        }
        for tab in &mut profile.session.queries {
            let _ = store::remove_grid(&profile.id, &store::query_grid_key(tab.id));
            for queued in 0..tab.stored(false).queued_results {
                let _ = store::remove_grid(&profile.id, &store::queued_grid_key(tab.id, queued));
            }
            tab.results = crate::result_grid::new_grid(window, cx);
            // A cancelled run stays `Running` until its result comes back and
            // `drop_stale_run` idles it, which keeps a second run off the tab
            // meanwhile.
            if !matches!(tab.query, QueryState::Running { .. }) {
                tab.query = QueryState::Idle;
            }
            tab.queue = None;
            // The slot just replaced is the one a loan would give back.
            tab.lent = None;
            tab.queued_results = 0;
            tab.last_query = None;
            tab.sent_from = None;
            tab.ran_from = None;
            tab.plan = None;
            tab.showing_plan = false;
            tab.showing_messages = false;
        }
        profile.session.clear_prompts();
        profile.session.apply_review = None;
        profile.session.insert_form = None;
        profile.databases = Databases::default();
    }

    /// The connect's half of [`Self::leave_database`], for a profile whose
    /// picked database was gone when it opened.
    pub(crate) fn leave_database_left_behind(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut().filter(|profile| profile.left_database) else {
            return;
        };
        profile.left_database = false;
        self.leave_database(window, cx);
        self.remember_profiles(cx);
    }

    /// Move the open connection onto the database the profile at `index` now
    /// names, where that is a `USE` and not a new connection
    /// ([`Engine::switches_database_in_session`]). The catalog is read again
    /// under a new generation, as a connect would; a `USE` that fails puts
    /// the profile back on `left`, the database and schema it was on.
    fn use_database(
        &mut self,
        index: usize,
        left: (String, Option<String>),
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.profiles.get(index).and_then(Profile::connection) else {
            // Opening applies the pick.
            self.reconnect(index, cx);
            return;
        };
        self.next_generation += 1;
        let generation = self.next_generation;
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        profile.generation = generation;
        profile.catalog = CatalogState::Loading;
        profile.session.structure_requests.clear();
        let id = profile.id.clone();
        connection.set_database(profile.config.database_name().as_deref());
        let applying = connection.clone();
        let set = on_own_thread(move || applying.apply_schema());
        self.refresh_explorer(&id, cx);
        cx.notify();
        cx.spawn(async move |workspace, cx| {
            let result = set.await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                if let Err(error) = result {
                    let (database, schema) = left;
                    profile.config.set_database(database);
                    profile.config.set_schema(schema);
                    connection.set_database(profile.config.database_name().as_deref());
                    connection.set_schema(profile.config.schema());
                    profile.session.notice = Some(error.message);
                    workspace.remember_profiles(cx);
                }
                workspace.load_catalog(&id, generation, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Offer the schemas the catalog already holds, to pick the active one.
    pub(crate) fn select_schema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        if !profile.config.engine().picks_schema() {
            return;
        }
        let unready = match (&profile.state, &profile.catalog) {
            (ProfileState::Connected(_), CatalogState::Loaded(..)) => None,
            (ProfileState::Connected(_), CatalogState::Loading) => {
                Some("The schemas are still loading.")
            }
            (ProfileState::Connected(_), CatalogState::Failed(_)) => {
                Some("The catalog did not load, so there are no schemas to pick from.")
            }
            _ => Some("The connection is not open."),
        };
        match unready {
            Some(message) => self.note(message.into(), cx),
            None => self.open_palette(PaletteMode::Schema, window, cx),
        }
    }

    /// Make `name` the profile's active schema, on the sessions it has open
    /// and the ones it opens later, without a reconnect: tabs, results and
    /// staged edits all stay. A statement running holds the session, so the
    /// `SET` waits for it rather than cancelling it.
    pub(crate) fn set_schema(&mut self, name: String, cx: &mut Context<Self>) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        // Not `active_schema`: picking the one shown while none is stored
        // still pins it, whatever the role's own `search_path` says.
        if profile.config.schema() == Some(name.as_str()) {
            return;
        }
        // A connect in flight was handed the config before this, and the
        // reconnect that would hand it the new one can be refused.
        if matches!(profile.state, ProfileState::Connecting) {
            self.note(
                "The connection is still opening, so the schema was not switched.".into(),
                cx,
            );
            return;
        }
        // A `SET` inside the user's transaction is undone by their ROLLBACK,
        // which would leave the titlebar naming a schema the session left.
        // Snowflake's rides on each request instead, out of a ROLLBACK's reach.
        // The session checks again once a statement still running lets go.
        if profile.in_transaction
            && profile
                .config
                .engine()
                .schema_switch_waits_for_transaction()
        {
            self.note(
                "A transaction is open on this connection, so the schema was not switched.".into(),
                cx,
            );
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let previous = profile.config.schema().map(str::to_string);
        profile.config.set_schema(Some(name.clone()));
        let (id, generation) = (profile.id.clone(), profile.generation);
        let connection = profile.connection();
        self.remember_profiles(cx);
        cx.notify();
        let Some(connection) = connection else {
            return;
        };
        // Recorded here, in the order the switches were made, so that
        // whichever thread applies last applies the latest.
        connection.set_schema(Some(&name));
        let applying = connection.clone();
        let set = on_own_thread(move || applying.apply_schema());
        cx.spawn(async move |workspace, cx| {
            let Err(error) = set.await else {
                return;
            };
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                // A later switch already holds the config, and is its own to
                // report.
                if profile.config.schema() == Some(name.as_str()) {
                    profile.config.set_schema(previous.clone());
                    connection.set_schema(previous.as_deref());
                }
                profile.session.notice = Some(error.message);
                workspace.remember_profiles(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn open_connection_form(
        &mut self,
        _: &NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self.current_group().map(str::to_string);
        self.new_connection_in(project, window, cx);
    }

    /// Opens the form for a new connection that joins `project`.
    pub(crate) fn new_connection_in(
        &mut self,
        project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut form = ConnectionForm::new(None, window, cx);
        form.project = project;
        self.form = Some(form);
        self.pending_project_deletion = None;
        cx.notify();
    }

    /// Opens the form over a saved connection, in its own project.
    pub(crate) fn edit_profile(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let mut form = ConnectionForm::editing(profile, window, cx);
        form.project = self.group_of(&profile.id).map(str::to_string);
        self.form = Some(form);
        cx.notify();
    }

    pub(crate) fn import_from(
        &mut self,
        action: &ImportConnections,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.import_connections(action.source, window, cx);
    }

    /// The source is read in the background, since reading one can mean a
    /// Keychain prompt. Each connection comes in Read-only whatever the other
    /// client allowed: it is the one mode that cannot surprise anybody.
    pub(crate) fn import_connections(
        &mut self,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.importing || self.pending_import.is_some() {
            return;
        }
        self.importing = true;
        self.note(format!("Reading {} connections…", source.label()), cx);
        let read = cx.background_executor().spawn(async move { source.read() });
        cx.spawn_in(window, async move |workspace, cx| {
            let report = read.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                workspace.importing = false;
                let report = match report {
                    Ok(report) => report,
                    Err(message) => return workspace.note(message, cx),
                };
                let existing = workspace
                    .profiles
                    .iter()
                    .map(|profile| &profile.config)
                    .collect::<Vec<_>>();
                let fresh = import::fresh_count(&existing, &report.imported);
                // Nothing new to place, so nothing to ask: the summary says
                // what was skipped and why.
                if fresh == 0 {
                    let summary = workspace.add_imported(source, report, None, window, cx);
                    return workspace.note(summary, cx);
                }
                // The question takes the reading line's place, as a summary
                // would.
                workspace.clear_notice();
                workspace.pending_import = Some(PendingImport {
                    source,
                    report,
                    fresh,
                    project: Some(format!("Imported from {}", source.label())),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// The import's question answered: its connections go into the project
    /// picked, made if it is new.
    pub(crate) fn confirm_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_import.take() else {
            return;
        };
        let summary =
            self.add_imported(pending.source, pending.report, pending.project, window, cx);
        self.note(summary, cx);
    }

    /// Returns the summary line.
    fn add_imported(
        &mut self,
        source: Source,
        report: import::Report,
        project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let had_none = self.profiles.is_empty();
        let mut ids = Vec::new();
        let mut added = import::Report {
            skipped: report.skipped,
            notes: report.notes,
            ..import::Report::default()
        };
        for mut imported in report.imported {
            if import::already_have(
                self.profiles.iter().map(|profile| &profile.config),
                &imported.config,
            ) {
                added.skipped.push(import::Skipped {
                    name: imported.name,
                    reason: "already imported".into(),
                });
                continue;
            }
            let names = self
                .profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect::<Vec<_>>();
            if names.contains(&imported.name) {
                imported.name = duplicate_profile_name(&imported.name, &names);
            }
            // Saved here rather than by `create_profile`, whose failure note
            // the summary would overwrite: a password that didn't reach the
            // keychain is gone at the next launch, and has to be said.
            let mut config = imported.config.clone();
            let password = config
                .server_mut()
                .map(|server| std::mem::take(&mut server.password))
                .filter(|password| !password.is_empty());
            let index = self.create_profile(
                imported.name.clone(),
                config,
                imported.color,
                Mode::ReadOnly,
                Origin::Form,
                window,
                cx,
            );
            if let Some(password) = password {
                let profile = &mut self.profiles[index];
                if let Err(message) = store::set_password(&profile.id, &password)
                    && !added.notes.contains(&message)
                {
                    added.notes.push(message);
                }
                if let Some(server) = profile.config.server_mut() {
                    server.password = password;
                }
            }
            added.imported.push(imported);
            ids.push(self.profiles[index].id.clone());
        }

        if let Some(project) = &project
            && !ids.is_empty()
        {
            // New profiles are in no project yet, so they join without
            // leaving one.
            self.project_named(project).connections.extend(ids);
            self.remember_profiles(cx);
        }
        // A first launch has nothing in front yet. Otherwise the connection
        // in front stays there rather than a batch of new ones each
        // connecting in turn.
        if had_none && !added.imported.is_empty() {
            self.activate(0, cx);
        }
        added.summary(source)
    }

    pub(crate) fn connect_active(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.profile().map(|profile| &profile.state),
            Some(ProfileState::Idle | ProfileState::Failed(_))
        ) {
            self.begin_connect(self.active, cx);
        }
    }

    pub(crate) fn begin_connect(&mut self, index: usize, cx: &mut Context<Self>) {
        self.next_generation += 1;
        let generation = self.next_generation;
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        profile.generation = generation;
        profile.state = ProfileState::Connecting;
        profile.in_transaction = false;
        profile.session_changed = false;
        profile.catalog = CatalogState::Loading;
        // Every structure still out was asked on the old generation, and
        // `issued_to` drops its answer. A structure tab counts an entry here as
        // asked and waits on it, so one caught mid-load would sit on Loading
        // with nothing coming; forgetting them makes its next visit ask again,
        // as `stale` does for a relation tab's rows.
        profile.session.structure_requests.clear();

        let id = profile.id.clone();
        let mut config = profile.config.clone();
        let mode = profile.mode;
        // The sidebar draws the tree whenever there are saved queries, so the
        // old catalog's rows have to leave it with the catalog.
        self.refresh_explorer(&id, cx);
        cx.notify();

        let connection_task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                // A file engine has nothing to authenticate to, so it never
                // reaches the Keychain — and never triggers its prompt.
                if let Some(server) = config.server_mut()
                    && server.password.is_empty()
                {
                    match store::password(&id) {
                        Ok(Some(password)) => server.password = password,
                        // No keychain item is not a missing password: a blank
                        // one is valid, so this connects with what it has.
                        Ok(None) => {}
                        Err(message) => return Err(message),
                    }
                }
                let connection = Connection::open(config).map_err(|error| error.message)?;
                // Only Read-only asks the server for anything here: the
                // servers already default to read-write, and a redundant
                // switch to it risks a pooler or proxy that rejects the
                // statement outright. A connect that fails to establish the
                // hold it promised is worse than one that never opened --
                // handing back a connection that looks Read-only but isn't is
                // the one outcome worse than failing to connect.
                if mode == Mode::ReadOnly {
                    connection
                        .set_read_only(true)
                        .map_err(|error| error.message)?;
                }
                connection.warm();
                Ok(connection)
            }
        });

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    // A picked database or schema that is gone opens where
                    // nothing is picked, and the profile follows it there.
                    let unpicked = result.as_ref().ok().and_then(Connection::unpicked).cloned();
                    if let Some(unpicked) = &unpicked {
                        match unpicked.database {
                            true => {
                                profile.config.set_database(String::new());
                                profile.left_database = true;
                            }
                            false => profile.config.set_schema(None),
                        }
                        profile.session.notice = Some(unpicked.message.clone());
                    }
                    profile.state = match result {
                        Ok(connection) => ProfileState::Connected(Box::new(connection)),
                        Err(message) => ProfileState::Failed(message),
                    };
                    if unpicked.is_some() {
                        workspace.remember_profiles(cx);
                    }
                    workspace.load_catalog(&id, generation, cx);
                    workspace.load_import_columns(&id, cx);
                    // The tab in front is already being looked at, so a
                    // reconnect has no later visit to re-run it on -- unless
                    // it names a relation of a database the connection is
                    // not on, and is about to close.
                    let active = workspace.profile().filter(|profile| {
                        profile.id == id && profile.connection().is_some() && !profile.left_database
                    });
                    if let Some(Tab::Object(object)) = active.map(|profile| profile.session.active)
                    {
                        workspace.load_relation(object, cx);
                    }
                    workspace.run_waiting_files(&id, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    pub(crate) fn load_catalog(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        // Reached with no connection only from a connect that failed, which
        // left the catalog on `Loading` and has nothing after it to clear that:
        // the sidebar would claim it was still loading objects for the rest of
        // the session, behind a status bar already saying the connection was
        // refused. The reason is the status bar's to carry -- repeating it here
        // paints the same sentence twice in the same red.
        let Some(connection) = profile.connection() else {
            profile.catalog = CatalogState::Failed("Not connected.".into());
            self.refresh_explorer(id, cx);
            self.catalog_landed(id, cx);
            return;
        };
        let catalog_task = cx
            .background_executor()
            .spawn(async move { connection.catalog() });

        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let result = catalog_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    profile.catalog = match result {
                        Ok(catalog) => CatalogState::Loaded(catalog, Routines::Loading),
                        Err(error) => CatalogState::Failed(error.message),
                    };
                    // Relations restore before the catalog arrives, wearing
                    // whatever kind was on disk -- a default, for a profile an
                    // older build wrote. This is the first moment there is
                    // anything to correct it from.
                    if let CatalogState::Loaded(catalog, _) = &profile.catalog {
                        for tab in &mut profile.session.objects {
                            if let ObjectKind::Relation(kind) = &mut tab.kind
                                && let Some(actual) = relation_kind(catalog, &tab.schema, &tab.name)
                            {
                                *kind = actual;
                            }
                        }
                    }
                    // The relations are new, so what was known about any
                    // one's columns describes a schema that may no longer
                    // exist. Here and not in `install_completions`, which the
                    // routines and every new buffer also reach without the
                    // relations having changed.
                    profile.session.completion_columns.borrow_mut().clear();
                    workspace.install_completions(&id, cx);
                    workspace.refresh_explorer(&id, cx);
                    workspace.catalog_landed(&id, cx);
                    cx.notify();
                    workspace.load_routines(&id, generation, cx);
                    workspace.load_sizes(&id, generation, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Fill the routines in behind the relations already on screen.
    ///
    /// A second request rather than a slower first one: the two are separate
    /// queries on every engine, and where the routines are the slow half the
    /// explorer would otherwise sit empty until they arrived.
    ///
    /// A failure here leaves the relations alone and is said once in the status
    /// bar. Replacing a working explorer with an error because the functions
    /// could not be listed would cost more than it reports.
    fn load_routines(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        let Some(connection) = profile.connection() else {
            if let CatalogState::Loaded(_, routines) = &mut profile.catalog {
                *routines = Routines::Failed;
            }
            return;
        };
        let routines_task = cx
            .background_executor()
            .spawn(async move { connection.routines() });

        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let result = routines_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    // Only onto a catalog that loaded. One that failed, or
                    // that a reconnect has already replaced, is not this half's
                    // to complete.
                    let CatalogState::Loaded(catalog, routines) = &mut profile.catalog else {
                        return;
                    };
                    match result {
                        Ok(loaded) => {
                            catalog.merge(loaded);
                            *routines = Routines::Loaded;
                        }
                        // On this profile, not through `note`: that writes to
                        // whichever one is active, and the user may have moved on.
                        Err(error) => {
                            *routines = Routines::Failed;
                            profile.session.notice = Some(error.message);
                        }
                    }
                    workspace.install_completions(&id, cx);
                    workspace.refresh_explorer(&id, cx);
                    workspace.catalog_landed(&id, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Fill table sizes in behind the relations, alongside the routines and in
    /// whichever order the two land.
    ///
    /// A failure is dropped without a word: a size is a nicety, and one a role
    /// may simply lack the privilege for, so a notice would repeat on every
    /// connect about something the user never asked for.
    fn load_sizes(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(connection) = self
            .issued_to(id, generation)
            .and_then(|profile| profile.connection())
        else {
            return;
        };
        let sizes_task = cx
            .background_executor()
            .spawn(async move { connection.sizes() });

        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let Ok(sizes) = sizes_task.await else {
                return;
            };
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    let CatalogState::Loaded(catalog, _) = &mut profile.catalog else {
                        return;
                    };
                    catalog.set_sizes(&sizes);
                    workspace.refresh_explorer(&id, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Point this profile's editor at what its catalog now holds.
    ///
    /// The provider is replaced whole rather than kept and mutated: a catalog
    /// arrives as one value and is never patched, so a snapshot behind an `Rc`
    /// needs no interior mutability and cannot be half-updated. Anything but a
    /// loaded catalog leaves the editor with no provider at all, which is the
    /// difference between offering nothing and offering the last database's
    /// tables to a buffer written against this one.
    /// Fetch one relation's columns for the completion cache.
    ///
    /// Reuses `Connection::structure`, which the Structure tab already runs, so
    /// completion adds no SQL of its own to any engine. It asks for more than
    /// it needs -- indexes and constraints come back too -- and that is the
    /// trade: one extra pair of catalog queries per relation the session
    /// actually writes about, against three more engine-specific statements to
    /// maintain and keep in step with hard rule 4.
    pub(crate) fn load_completion_columns(
        &mut self,
        schema: String,
        relation: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let id = profile.id.clone();
        let generation = profile.generation;
        let columns = profile.session.completion_columns.clone();

        let task = cx.background_executor().spawn({
            let (schema, relation) = (schema.clone(), relation.clone());
            async move { connection.structure(&schema, &relation) }
        });
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    // A reconnect clears the cache and starts a new generation,
                    // so a result from the old one describes a database this
                    // profile is no longer talking to.
                    if workspace.issued_to(&id, generation).is_none() {
                        return;
                    }
                    let key = (schema, relation);
                    let state = match result {
                        Ok(structure) => completion::ColumnState::Loaded(
                            structure
                                .columns
                                .into_iter()
                                .map(|column| column.name)
                                .collect(),
                        ),
                        // The attempt count rides on the `Loading` the request
                        // wrote, so a relation that keeps failing runs out.
                        Err(_) => {
                            let attempts = match columns.borrow().get(&key) {
                                Some(completion::ColumnState::Loading(attempts)) => *attempts,
                                _ => 0,
                            };
                            completion::ColumnState::Failed(attempts.saturating_add(1))
                        }
                    };
                    columns.borrow_mut().insert(key, state);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    pub(crate) fn install_completions(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let engine = profile.config.engine();
        profile.session.completions = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => Some(Rc::new(SchemaCompletions::new(
                engine.syntax(),
                Arc::new(catalog.clone()),
                profile.session.completion_columns.clone(),
                cx.weak_entity(),
            ))),
            _ => None,
        };
        let session = &profile.session;
        let provider = session
            .completions
            .clone()
            .map(|provider| provider as Rc<dyn CompletionProvider>);

        // Every buffer, not just the one in front: a tab switch must not be a
        // moment where completion quietly stops working.
        for tab in &session.queries {
            tab.editor.update(cx, |editor, _| {
                editor.lsp_mut().completion_provider = provider.clone();
            });
        }
        // And every raw filter bar, which completes from the same catalog.
        for tab in &session.objects {
            let ObjectBody::Relation { filters, .. } = &tab.body else {
                continue;
            };
            let completions = session.filter_completions(engine, &tab.schema, &tab.name);
            for row in filters.iter().filter(|row| row.raw) {
                row.value.update(cx, |editor, _| {
                    editor.lsp_mut().completion_provider = completions.clone();
                });
            }
        }
    }

    pub(crate) fn refresh_explorer(&mut self, id: &str, cx: &mut Context<Self>) {
        self.rebuild_explorer(id, false, cx);
    }

    /// The sidebar again after only the saved queries changed. The catalog's
    /// rows are the same rows under the same ids, so what the user folded,
    /// opened and selected among them is put back rather than reset.
    pub(crate) fn refresh_query_library(&mut self, id: &str, cx: &mut Context<Self>) {
        self.rebuild_explorer(id, true, cx);
    }

    fn rebuild_explorer(&mut self, id: &str, keep: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let filter = profile.session.explorer_filter.read(cx).value();
        let mut explorer = explorer::queries(&profile.session.saved_queries, &filter);
        if let CatalogState::Loaded(catalog, _) = &profile.catalog {
            explorer.extend(build_explorer_tree(catalog, &filter));
        }
        let tree = profile.session.explorer_tree.clone();
        let (items, selected) = match keep {
            true => {
                let state = tree.read(cx);
                let roots: Vec<_> = (0..)
                    .map_while(|ix| state.entry(ix))
                    .filter(|entry| entry.depth() == 0)
                    .map(|entry| entry.item().clone())
                    .collect();
                let selected = state.selected_item().map(|item| item.id.clone());
                (
                    explorer::with_folds(explorer.items, &explorer::folds(&roots)),
                    selected,
                )
            }
            false => (explorer.items, None),
        };

        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) {
            profile.session.explorer_leaves = Arc::new(explorer.leaves);
            profile.session.explorer_highlights = Arc::new(explorer.highlights);
        }
        tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            if let Some(selected) = selected {
                let at = tree.index_of(&selected);
                tree.set_selected_index(at, cx);
            }
        });
    }

    pub(crate) fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.profiles.len() {
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
        }
        self.active = index;
        self.form = None;
        // Written here rather than at quit, so the profile in front survives a
        // crash as well as a close.
        self.remember_profiles(cx);
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
            profile.session.clear_prompts();
        }
        self.connect_active(cx);
        // A connect that landed while this profile was behind another one ran
        // nothing on its tab in front, which may still be a restored snapshot
        // waiting for its first run.
        if let Some(Tab::Object(object)) = self.profile().map(|profile| profile.session.active) {
            self.load_relation(object, cx);
        }
        cx.notify();
    }

    pub(crate) fn cycle_profile(&mut self, step: isize, cx: &mut Context<Self>) {
        let members = self.current_group_members();
        if members.len() < 2 || self.form.is_some() {
            return;
        }
        let position = members
            .iter()
            .position(|index| *index == self.active)
            .unwrap_or(0) as isize;
        let next = (position + step).rem_euclid(members.len() as isize) as usize;
        self.activate(members[next], cx);
    }

    pub(crate) fn next_profile(&mut self, _: &NextProfile, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_profile(1, cx);
    }

    pub(crate) fn previous_profile(
        &mut self,
        _: &PreviousProfile,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_profile(-1, cx);
    }

    /// Asked about first, in the palette's confirmation (`Mode::Confirm`).
    pub(crate) fn remove_profile(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let id = profile.id.clone();
        let name = profile.name.clone();
        if index == self.active
            && let Err(message) = self.persist_buffer(cx)
        {
            self.note(message, cx);
            return;
        }

        // An unavailable profile under the same id owns everything filed
        // under it, and gets it back in a build that can read it.
        let shared = held_by_unavailable(&id, &self.unavailable);
        // Counted before the directory goes, because afterwards there is
        // nothing left to count and the number is what the note reports.
        let queries = if shared {
            0
        } else {
            store::saved_queries(&id).len()
        };

        let stopped = self.abandon_import(&id, cx);

        let removed = self.profiles.remove(index);
        if let Some(key) = identity_file(&removed.config)
            && let Ok(directory) = store::ssh_key_directory()
            && is_unshared_imported_key(
                key,
                &directory,
                self.profiles
                    .iter()
                    .filter_map(|profile| identity_file(&profile.config))
                    .chain(
                        self.unavailable
                            .iter()
                            .filter_map(store::UnavailableProfile::identity_file),
                    ),
            )
        {
            let _ = std::fs::remove_file(key);
        }
        let removed_queries = if shared {
            Ok(())
        } else {
            store::delete_password(&id);
            let removed_queries = store::delete_queries(&id);
            let _ = store::delete_grids(&id);
            for project in &mut self.projects {
                project.connections.retain(|member| member != &id);
            }
            removed_queries
        };
        self.active = active_after_removal(self.active, index, self.profiles.len());
        self.remember_profiles(cx);
        if self.profiles.is_empty() {
            self.refocus_front();
        }
        self.connect_active(cx);
        let note = removal_note(&name, queries, removed_queries.err());
        self.note(
            match stopped {
                Some(stopped) => format!("{note} {stopped}"),
                None => note,
            },
            cx,
        );
    }

    /// Removes a profile this build cannot open, asked about first like
    /// [`Self::remove_profile`].
    pub(crate) fn remove_unavailable(&mut self, at: usize, cx: &mut Context<Self>) {
        if at >= self.unavailable.len() {
            return;
        }
        let removed = self.unavailable.remove(at);
        // Ids are not unique in a hand-edited file, and what is filed under
        // one belongs to every profile holding it.
        let shared = self.profiles.iter().any(|profile| profile.id == removed.id)
            || held_by_unavailable(&removed.id, &self.unavailable);
        let note = if shared {
            removal_note(&removed.name, 0, None)
        } else {
            let queries = store::saved_queries(&removed.id).len();
            store::delete_password(&removed.id);
            let removed_queries = store::delete_queries(&removed.id);
            let _ = store::delete_grids(&removed.id);
            for project in &mut self.projects {
                project.connections.retain(|member| member != &removed.id);
            }
            removal_note(&removed.name, queries, removed_queries.err())
        };
        self.remember_profiles(cx);
        if self.profiles.is_empty() {
            self.refocus_front();
        }
        self.note(note, cx);
    }
}

/// An entry in the connection form's project list.
#[derive(Clone, PartialEq)]
enum ProjectChoice {
    /// An existing project, or `None` for No project.
    In(Option<String>),
    New,
}

/// The connection a stored profile describes, on `engine` and `sslmode` as
/// [`Workspace::restore_profile`] read them.
fn stored_config(
    stored: &store::StoredProfile,
    engine: Engine,
    sslmode: SslMode,
) -> ConnectionConfig {
    match engine {
        Engine::Sqlite => ConnectionConfig::Sqlite {
            path: stored.path.clone().unwrap_or_default(),
            statement_timeout: stored.statement_timeout.unwrap_or_default(),
        },
        Engine::Turso => ConnectionConfig::Turso {
            path: stored.path.clone().unwrap_or_default(),
            statement_timeout: stored.statement_timeout.unwrap_or_default(),
        },
        Engine::DuckDb => ConnectionConfig::DuckDb {
            path: stored.path.clone().unwrap_or_default(),
            statement_timeout: stored.statement_timeout.unwrap_or_default(),
            database: stored.database.clone(),
            schema: stored.schema.clone(),
        },
        Engine::Postgres
        | Engine::MySql
        | Engine::MariaDb
        | Engine::SqlServer
        | Engine::ClickHouse
        | Engine::MongoDb
        | Engine::Redis => {
            let server = ServerConfig {
                host: stored.host.clone(),
                port: stored.port,
                database: stored.database.clone(),
                user: stored.user.clone(),
                // Never on disk. Read from the Keychain when connecting.
                password: String::new(),
                sslmode,
                root_certificate: stored.root_certificate.clone(),
                statement_timeout: stored.statement_timeout.unwrap_or_default(),
                ssh: stored.ssh.clone(),
                schema: stored.schema.clone(),
            };
            match engine {
                Engine::MySql => ConnectionConfig::MySql(server),
                Engine::MariaDb => ConnectionConfig::MariaDb(server),
                Engine::SqlServer => ConnectionConfig::SqlServer(server),
                Engine::ClickHouse => ConnectionConfig::ClickHouse(server),
                Engine::Redis => ConnectionConfig::Redis(server),
                Engine::MongoDb => ConnectionConfig::MongoDb(crate::db::MongoConfig {
                    server,
                    srv: stored.srv.unwrap_or_default(),
                    options: stored.options.clone().unwrap_or_default(),
                    login_database: stored.login_database.clone(),
                }),
                _ => ConnectionConfig::Postgres(server),
            }
        }
        Engine::Snowflake => ConnectionConfig::Snowflake(SnowflakeConfig {
            account: stored.account.clone().unwrap_or_default(),
            // Blank on disk is the derived host, the same as absent.
            host: Some(stored.host.clone()).filter(|host| !host.is_empty()),
            user: stored.user.clone(),
            private_key: stored.private_key.clone().unwrap_or_default(),
            database: stored.database.clone(),
            warehouse: stored.warehouse.clone(),
            role: stored.role.clone(),
            statement_timeout: stored.statement_timeout.unwrap_or_default(),
            schema: stored.schema.clone(),
        }),
    }
}

/// Whether picking `name` would leave the profile where it is. A blank
/// database is the login's default, which only the server can name, so that is
/// read off the list it sent; otherwise the configured name is the truth -- on
/// MySQL a `USE` moves the session, and with it the list's current one.
fn already_on(configured: &str, listed_current: Option<&str>, name: &str) -> bool {
    name == configured || (configured.is_empty() && listed_current == Some(name))
}

/// Removing an entry below the active one shifts the vector under the index,
/// so clamping to the new length alone silently activates the wrong profile.
pub(crate) fn active_after_removal(active: usize, removed: usize, remaining: usize) -> usize {
    let shifted = if removed < active { active - 1 } else { active };
    shifted.min(remaining.saturating_sub(1))
}

fn identity_file(config: &ConnectionConfig) -> Option<&str> {
    config.server()?.ssh.as_ref()?.identity_file.as_deref()
}

/// A key the importer wrote goes with the last profile naming it; Duplicate
/// copies the path. Anything not directly inside `directory` is the user's own
/// file and never touched.
pub(crate) fn is_unshared_imported_key<'a>(
    key: &str,
    directory: &Path,
    others: impl IntoIterator<Item = &'a str>,
) -> bool {
    let key_path = Path::new(key);
    key_path.file_name().is_some()
        && key_path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| {
                directory
                    .canonicalize()
                    .is_ok_and(|directory| directory == parent)
            })
        && !others.into_iter().any(|other| other == key)
}

/// Why a profile on `engine` cannot be opened: a plugin it names that is not
/// installed, or an engine only a newer build has.
pub(crate) fn unavailable_reason(engine: &str) -> String {
    match engine.strip_prefix("plugin:") {
        Some(plugin) if !plugin.is_empty() => format!("Needs the {plugin} plugin"),
        _ => format!("Made by a newer DBDelve (engine `{engine}`)"),
    }
}

/// Whether a profile this build cannot read holds `id`. Ids are not unique in
/// a hand-edited file, and the keychain entry, saved queries, grid snapshots
/// and project membership are filed by id, so they are that profile's too.
fn held_by_unavailable(id: &str, unavailable: &[store::UnavailableProfile]) -> bool {
    unavailable.iter().any(|gone| gone.id == id)
}

/// What a removal took with it. The count is named because saved queries are
/// the one thing a person could still want back, and a directory that outlived
/// its profile is reported rather than passed over -- the id is derived from the
/// name, so whatever is left there attaches itself to the next profile called
/// the same thing.
pub(crate) fn removal_note(name: &str, queries: usize, problem: Option<String>) -> String {
    if let Some(problem) = problem {
        return format!("Removed {name}, but its saved queries are still on disk: {problem}");
    }

    match queries {
        0 => format!("Removed {name}."),
        1 => format!("Removed {name} and its saved query."),
        _ => format!("Removed {name} and its {queries} saved queries."),
    }
}

/// The one empty buffer, in front, that a connection with no session of its
/// own opens on: somewhere to start writing.
fn first_buffer(name: Option<String>) -> Vec<store::StoredQueryTab> {
    vec![store::StoredQueryTab {
        id: 0,
        name,
        active: true,
        queued_results: 0,
    }]
}

/// The buffers a stored profile reopens with. One written before a buffer was
/// a tab carries one, whose name is in the legacy scalar and whose text
/// `read_scratch` migrates, blank or not. One from a build that has tabs says
/// how many it had, and none is none: its user closed them all.
fn stored_buffers(
    open_queries: Vec<store::StoredQueryTab>,
    next_query_id: Option<u64>,
    open_query: Option<String>,
) -> Vec<store::StoredQueryTab> {
    match open_queries.is_empty() && next_query_id.is_none() {
        true => first_buffer(open_query),
        false => open_queries,
    }
}

/// Whether an edit to the URL field brought a URL in whole -- pasted rather
/// than typed -- that names an engine the fields can be filled for. Read off
/// what the edit inserted rather than the length, so a paste over a selection
/// counts and deleting part of a URL does not.
pub(crate) fn pasted_url(before: &str, after: &str) -> bool {
    let before: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    let prefix = before
        .iter()
        .zip(&after_chars)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after_chars[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let inserted = after_chars.len() - prefix - suffix;
    let fills = match super::welcome::recognise(after) {
        super::welcome::Recognised::Url(engine) => engine.fields() != Fields::Account,
        super::welcome::Recognised::File(_) => true,
        _ => false,
    };
    inserted > 1 && fills
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restored_configs(text: &str) -> String {
        let store::ProfileList { profiles, .. } =
            store::decode_profiles(text).expect("the fixture loads");
        profiles
            .iter()
            .map(|stored| {
                let engine = Engine::parse(stored.engine.as_deref().unwrap_or("postgres"))
                    .expect("a known engine");
                let sslmode = stored
                    .sslmode
                    .as_deref()
                    .map_or(Ok(SslMode::default()), SslMode::parse)
                    .expect("a known sslmode");
                format!(
                    "{} {:#?}\n",
                    stored.id,
                    stored_config(stored, engine, sslmode)
                )
            })
            .collect()
    }

    #[test]
    fn every_engines_profile_restores_the_connection_it_did() {
        assert_eq!(
            restored_configs(include_str!("../testdata/every-engine.toml")),
            include_str!("../testdata/every-engine.configs.txt")
        );
    }

    /// A supported `pg` and, hand-edited to the same id, one on a plugin.
    const SHARED_ID: &str = "[[profiles]]\nid = 'pg'\nname = 'Postgres'\nhost = 'h'\n\
        database = 'd'\nuser = 'u'\nengine = 'postgres'\n\n\
        [[profiles]]\nid = 'pg'\nname = 'ODBC'\nengine = 'plugin:odbc'\n";

    #[test]
    fn an_unavailable_profile_says_what_it_is_missing() {
        assert_eq!(unavailable_reason("plugin:odbc"), "Needs the odbc plugin");
        assert_eq!(
            unavailable_reason("zz-unknown"),
            "Made by a newer DBDelve (engine `zz-unknown`)"
        );
        // No id to name: not a plugin anyone could install.
        assert_eq!(
            unavailable_reason("plugin:"),
            "Made by a newer DBDelve (engine `plugin:`)"
        );
    }

    #[test]
    fn a_restore_prunes_no_grids_while_an_unavailable_profile_holds_the_id() {
        let list = store::decode_profiles(SHARED_ID).expect("a mixed file loads");
        assert_eq!((list.profiles.len(), list.unavailable.len()), (1, 1));
        assert!(held_by_unavailable(&list.profiles[0].id, &list.unavailable));
        // A file with no unavailable profile prunes every profile's grids, as before.
        let alone = store::decode_profiles(include_str!("../testdata/every-engine.toml")).unwrap();
        assert!(alone.unavailable.is_empty());
        assert!(
            alone
                .profiles
                .iter()
                .all(|stored| !held_by_unavailable(&stored.id, &alone.unavailable))
        );
    }

    #[test]
    fn removing_a_profile_keeps_what_an_unavailable_one_under_its_id_still_owns() {
        let list = store::decode_profiles(SHARED_ID).expect("a mixed file loads");
        assert!(held_by_unavailable("pg", &list.unavailable));
        // Only the id decides: another id's keychain entry, queries, grids and
        // project membership go with it as before.
        assert!(!held_by_unavailable("odbc", &list.unavailable));
        assert!(!held_by_unavailable("pg", &[]));
    }

    #[test]
    fn a_duckdb_profiles_picked_database_and_schema_survive_a_restart() {
        let mut picked = ConnectionConfig::duckdb("/data/dev.duckdb".into(), 30);
        picked.set_database("other".into());
        picked.set_schema(Some("analytics".into()));
        let file = |fields: &str| -> store::StoredProfile {
            toml::from_str(&format!(
                "id = 'dev'\nname = 'Dev'\nhost = ''\nuser = ''\nengine = 'duckdb'\n\
                 path = '/data/dev.duckdb'\n{fields}"
            ))
            .expect("a profile")
        };
        // The two fields `Profile::stored` writes from the config.
        let stored = file(&format!(
            "statement_timeout = 30\ndatabase = '{}'\nschema = '{}'",
            picked.database().unwrap(),
            picked.schema().unwrap()
        ));
        assert_eq!(
            stored_config(&stored, Engine::DuckDb, SslMode::default()),
            picked
        );
        // Written before either could be picked: the file's own database.
        assert_eq!(
            stored_config(&file("database = ''"), Engine::DuckDb, SslMode::default()),
            ConnectionConfig::duckdb("/data/dev.duckdb".into(), 0)
        );
    }

    #[test]
    fn only_a_recognised_url_arriving_whole_fills_the_form() {
        let url = "postgresql://ada@db.internal:5432/orders";
        assert!(pasted_url("", url));
        // Pasted over another URL, or over a selection in one.
        assert!(pasted_url("mysql://root@localhost/shop", url));
        assert!(pasted_url("postgresql://ada@/orders", url));
        assert!(pasted_url("", "~/data/app.sqlite"));
        // Typed a character at a time, it waits for Enter.
        assert!(!pasted_url("postgresql://ada@db.internal:5432/order", url));
        // Cutting part of one away is not a URL arriving.
        assert!(!pasted_url(url, "postgresql://ada@db.internal:5432"));
        // Not a URL, or one with no fields here to fill.
        assert!(!pasted_url("", "hello world"));
        assert!(!pasted_url("", "nope://db/app"));
        assert!(!pasted_url("", "snowflake://acme-prod"));
    }

    #[test]
    fn a_database_switch_is_a_no_op_only_onto_the_configured_one() {
        assert!(already_on("prod", Some("prod"), "prod"));
        // A `USE` moved the session, but the profile is still configured for
        // `prod`, so `scratch` is a real switch and `prod` is not.
        assert!(!already_on("prod", Some("scratch"), "scratch"));
        assert!(already_on("prod", Some("scratch"), "prod"));
        // Blank is wherever the login landed, which the listing names.
        assert!(already_on("", Some("master"), "master"));
        assert!(!already_on("", Some("master"), "tempdb"));
    }

    #[test]
    fn a_connection_with_nothing_to_restore_opens_on_one_empty_buffer() {
        let blank = store::StoredQueryTab {
            id: 0,
            name: None,
            active: true,
            queued_results: 0,
        };
        // A connection just made.
        assert_eq!(first_buffer(None), std::slice::from_ref(&blank));
        // One from before buffers were tabs, blank or holding a saved query.
        assert_eq!(
            stored_buffers(Vec::new(), None, None),
            std::slice::from_ref(&blank)
        );
        assert_eq!(
            stored_buffers(Vec::new(), None, Some("daily".into())),
            [store::StoredQueryTab {
                name: Some("daily".into()),
                ..blank.clone()
            }]
        );
        // Every tab closed is a choice the next launch keeps.
        assert_eq!(stored_buffers(Vec::new(), Some(3), None), []);
        let open = vec![store::StoredQueryTab {
            id: 2,
            name: None,
            active: false,
            queued_results: 0,
        }];
        assert_eq!(stored_buffers(open.clone(), Some(3), None), open);
    }

    #[test]
    fn removing_a_profile_keeps_the_same_one_active() {
        assert_eq!(active_after_removal(2, 0, 3), 1);
        assert_eq!(active_after_removal(2, 2, 3), 2);
        assert_eq!(active_after_removal(2, 3, 3), 2);
        // The active profile was last, so there is nothing at its index now.
        assert_eq!(active_after_removal(2, 2, 2), 1);
        assert_eq!(active_after_removal(0, 0, 0), 0);
    }

    #[test]
    fn only_an_imported_key_no_other_profile_names_is_deleted() {
        let root =
            std::env::temp_dir().join(format!("dbdelve-profiles-key-test-{}", std::process::id()));
        let directory = root.join("ssh-keys");
        std::fs::create_dir_all(&directory).unwrap();
        let key = directory.join("tableplus-a");
        let key = key.to_str().unwrap();
        let outside = root.join("id_ed25519");
        let escaping = format!("{}/../id_ed25519", directory.display());
        let dotted = format!("{}/..", directory.display());

        let verdicts = [
            is_unshared_imported_key(key, &directory, ["/elsewhere/key"]),
            is_unshared_imported_key(key, &directory, [key]),
            is_unshared_imported_key(outside.to_str().unwrap(), &directory, []),
            is_unshared_imported_key(&escaping, &directory, []),
            is_unshared_imported_key(&dotted, &directory, []),
            is_unshared_imported_key("~/.ssh/id_ed25519", &directory, []),
        ];
        _ = std::fs::remove_dir_all(&root);
        assert_eq!(verdicts, [true, false, false, false, false, false]);
    }

    #[test]
    fn a_removal_says_what_went_with_the_profile() {
        assert_eq!(removal_note("Prod", 0, None), "Removed Prod.");
        assert_eq!(
            removal_note("Prod", 1, None),
            "Removed Prod and its saved query."
        );
        assert_eq!(
            removal_note("Prod", 7, None),
            "Removed Prod and its 7 saved queries."
        );
        // The count is not mentioned when the files are still there to count.
        assert_eq!(
            removal_note("Prod", 7, Some("permission denied".into())),
            "Removed Prod, but its saved queries are still on disk: permission denied"
        );
    }
}

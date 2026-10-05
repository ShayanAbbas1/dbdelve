//! Opening, saving, restoring and switching between connections.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use gpui_component::menu::PopupMenuItem;

use crate::connection_form::{ConnectionTest, duplicate_profile_name};
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
            editor_font_size: Some(self.settings.editor_font_size),
            preview_rows: Some(self.settings.preview_rows),
            opacity: Some(self.settings.opacity),
            check_for_updates: Some(self.settings.check_for_updates),
            color_titlebar: Some(self.settings.color_titlebar),
            custom_keybindings: Some(self.settings.custom_keybindings.clone()),
        };
        if let Err(message) = store::save_profiles(
            &profiles,
            active.as_deref(),
            &fonts,
            &settings,
            &self.projects,
        ) {
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
        // and Postgres is what it was. An engine this build cannot read is a
        // profile written by a build that has one this one does not, so it is
        // read as Postgres and says so rather than connecting somewhere the
        // user did not ask for without mentioning it.
        let (engine, unreadable_engine) = match stored.engine.as_deref() {
            None => (Engine::Postgres, None),
            Some(stored) => match Engine::parse(stored) {
                Ok(engine) => (engine, None),
                Err(message) => (Engine::Postgres, Some(message)),
            },
        };
        let config = match engine {
            Engine::Sqlite => ConnectionConfig::Sqlite {
                path: stored.path.unwrap_or_default(),
                statement_timeout: stored.statement_timeout.unwrap_or_default(),
            },
            Engine::Postgres
            | Engine::MySql
            | Engine::MariaDb
            | Engine::SqlServer
            | Engine::MongoDb => {
                let server = ServerConfig {
                    host: stored.host,
                    port: stored.port,
                    database: stored.database,
                    user: stored.user,
                    // Never on disk. Read from the Keychain when connecting.
                    password: String::new(),
                    sslmode,
                    root_certificate: stored.root_certificate,
                    statement_timeout: stored.statement_timeout.unwrap_or_default(),
                    ssh: stored.ssh,
                };
                match engine {
                    Engine::MySql => ConnectionConfig::MySql(server),
                    Engine::MariaDb => ConnectionConfig::MariaDb(server),
                    Engine::SqlServer => ConnectionConfig::SqlServer(server),
                    Engine::MongoDb => ConnectionConfig::MongoDb(crate::db::MongoConfig {
                        server,
                        srv: stored.srv.unwrap_or_default(),
                        options: stored.options.unwrap_or_default(),
                        login_database: stored.login_database,
                    }),
                    _ => ConnectionConfig::Postgres(server),
                }
            }
            Engine::Snowflake => ConnectionConfig::Snowflake(SnowflakeConfig {
                account: stored.account.unwrap_or_default(),
                // Blank on disk is the derived host, the same as absent.
                host: Some(stored.host).filter(|host| !host.is_empty()),
                user: stored.user,
                private_key: stored.private_key.unwrap_or_default(),
                database: stored.database,
                warehouse: stored.warehouse,
                role: stored.role,
                statement_timeout: stored.statement_timeout.unwrap_or_default(),
            }),
        };
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
        store::prune_grids(&stored.id, &live_grids);
        let mut session = Session::new(
            stored.id.clone(),
            stored_queries,
            stored.next_query_id.unwrap_or(0),
            stored.open_objects,
            config.engine(),
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
            catalog: CatalogState::Loading,
            databases: Databases::default(),
            session,
        });
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
        let existing = self
            .profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>();
        let id = store::profile_id(&name, &existing);
        let session = Session::new(
            id.clone(),
            first_buffer(None),
            0,
            Vec::new(),
            config.engine(),
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
            catalog: CatalogState::Loading,
            databases: Databases::default(),
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
        self.switcher_open = false;
        cx.notify();

        let password_task = {
            let id = id.clone();
            cx.background_executor()
                .spawn(async move { store::password(&id).ok().flatten() })
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let password = password_task.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                // Removed from the switcher while the Keychain answered --
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

    pub(crate) fn apply_connection_url(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else {
            return;
        };
        let url = form.url.read(cx).value();
        let config = match ConnectionConfig::from_url(url.trim()) {
            Ok(config) => config,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };

        // Only the fields the URL's own engine has. Blanking the others would
        // throw away a half-typed connection to a different database, which the
        // user never asked to lose by pasting a URL.
        let filled = match &config {
            ConnectionConfig::Sqlite { path, .. } => vec![
                (&form.name, default_profile_name(&config)),
                (&form.path, path.clone()),
            ],
            ConnectionConfig::Postgres(server)
            | ConnectionConfig::MySql(server)
            | ConnectionConfig::MariaDb(server)
            | ConnectionConfig::SqlServer(server)
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
            form.engine = engine;
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

    /// A dropdown on the connection form: `pick` writes the choice into it.
    pub(crate) fn form_dropdown<T: Clone + PartialEq + 'static>(
        id: &'static str,
        selected: T,
        groups: Vec<Vec<T>>,
        label: impl Fn(&T) -> gpui::SharedString + 'static,
        marker: impl Fn(&T) -> Option<AnyElement> + 'static,
        pick: impl Fn(&mut ConnectionForm, T) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Self::dropdown(
            id,
            selected,
            groups,
            label,
            marker,
            move |workspace, option| {
                if let Some(form) = &mut workspace.form {
                    pick(form, option);
                }
            },
            cx,
        )
    }

    /// A field-shaped button that opens a menu of `groups` of options, with a
    /// separator between groups: the pickers on the connection form and the
    /// import's project. `pick` makes the choice; `marker` is what is drawn
    /// before a label, if anything.
    pub(crate) fn dropdown<T: Clone + PartialEq + 'static>(
        id: &'static str,
        selected: T,
        groups: Vec<Vec<T>>,
        label: impl Fn(&T) -> gpui::SharedString + 'static,
        marker: impl Fn(&T) -> Option<AnyElement> + 'static,
        pick: impl Fn(&mut Self, T) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let label = Rc::new(label);
        let marker = Rc::new(marker);
        let pick = Rc::new(pick);
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
                    .text_size(px(layout::TEXT_MD))
                    .text_color(t.text)
                    .children(marker(&selected))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(label(&selected)),
                    )
                    .child(row_icon(t, icon::CHEVRON_DOWN)),
            )
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
                                .on_click(move |_, _, cx| {
                                    _ = workspace.update(cx, |workspace, cx| {
                                        pick(workspace, option.clone());
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

    /// The engine decides which fields the form even has, so it is the first
    /// thing on it.
    pub(crate) fn engine_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        Self::form_dropdown(
            "engine",
            form.engine,
            vec![Engine::ALL.to_vec()],
            |engine| engine.label().into(),
            |_| None,
            |form, engine| {
                // Only when the field set actually changes: Postgres and
                // MySQL show the same fields, so switching between them
                // takes nothing away and must not take focus either.
                if form.engine.fields() != engine.fields() {
                    form.needs_focus = Some(match engine.fields() {
                        Fields::Server => form.host.clone(),
                        Fields::File => form.path.clone(),
                        Fields::Account => form.account.clone(),
                    });
                }
                form.engine = engine;
                // Both belonged to the fields that just left the screen.
                form.error = None;
                form.test = None;
            },
            cx,
        )
    }

    /// Weakest first.
    ///
    /// Read only while a connection already exists: past creation,
    /// `Workspace::set_mode` is the one door a mode changes through, and it
    /// pushes the change into that connection's live grids -- something a
    /// profile still being typed into has none of yet. `render_connection_form`
    /// draws this only when there is no `editing` id, for exactly that reason.
    pub(crate) fn mode_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        Self::form_dropdown(
            "mode",
            form.mode,
            vec![Mode::ALL.to_vec()],
            |mode| mode.label().into(),
            |_| None,
            |form, mode| {
                form.mode = mode;
                // A pass without the Read-only hold did not test it.
                form.test = None;
            },
            cx,
        )
    }

    /// The colour only ever labels a connection, so nothing here can make the
    /// form invalid and nothing has to move focus.
    pub(crate) fn color_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        Self::form_dropdown(
            "color",
            form.color,
            vec![
                std::iter::once(None)
                    .chain(ConnectionColor::ALL.map(Some))
                    .collect(),
            ],
            |color| color.map_or("None", ConnectionColor::label).into(),
            |color| {
                color.map(|color| {
                    div()
                        .size(px(layout::SPACE_SM))
                        .rounded_full()
                        .bg(color.swatch())
                        .into_any_element()
                })
            },
            |form, color| form.color = color,
            cx,
        )
    }

    /// The project is made when the form is saved, so "New project…" only
    /// swaps the list for a field to name it in.
    pub(crate) fn project_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let form = self.form.as_ref().expect("drawn only on the form");
        if form.naming_project {
            return div()
                .flex()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&form.project_name).w_full()),
                )
                .child(
                    icon_button(
                        "choose-project",
                        icon::CLOSE,
                        Tone::Quiet,
                        Control::Standard,
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

    pub(crate) fn sslmode_chip(&self, mode: SslMode, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.sslmode == mode);
        div()
            .id(mode.as_str())
            .flex()
            .items_center()
            .h(px(24.))
            .px(px(layout::SPACE_SM))
            .rounded(px(layout::RADIUS_CONTROL))
            .text_size(px(layout::TEXT_SM))
            .whitespace_nowrap()
            .map(|chip| {
                if selected {
                    chip.bg(t.element_active).text_color(t.text)
                } else {
                    chip.text_color(t.text_muted)
                        .hover(|style| style.bg(t.element_hover))
                }
            })
            .child(mode.label())
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    // Stepping down from a verifying mode unmounts the
                    // certificate field, which may be the one holding focus.
                    if form.sslmode.checks_certificate() && !mode.checks_certificate() {
                        form.needs_focus = Some(form.password.clone());
                    }
                    form.sslmode = mode;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    pub(crate) fn connect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
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
        let ((name, config), project) = match form
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
        // Removed from the switcher while the form sat open: there is nothing
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
            .spawn(async move { connection.databases() });
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

    /// Move the profile in front onto another database on its server.
    ///
    /// Object tabs go, and their grid snapshots with them: both are keyed by
    /// schema and name within the profile, with no database in the key, so
    /// one left open would name a relation that may not exist here and a
    /// relation of the same name would read the old one's rows back. Query
    /// tabs keep their SQL, which is the user's to run wherever they like, but
    /// drop their results: those rows belong to the database left behind, and
    /// an edit staged on them would be applied to this one.
    pub(crate) fn set_database(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(server) = profile.config.server() else {
            return;
        };
        if already_on(
            &server.database,
            profile.databases.current.as_deref(),
            &name,
        ) {
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

        let active = session.active;
        let mut closing = session.objects.iter().map(|tab| tab.id).collect::<Vec<_>>();
        // The tab in front closes last, so the one it hands the front to is a
        // query tab that stays rather than a sibling about to close too.
        closing.sort_by_key(|id| Tab::Object(*id) == active);
        let queries = session.queries.iter().map(|tab| tab.id).collect::<Vec<_>>();
        for id in closing {
            self.close_object(id, window, cx);
        }
        // Before the reconnect: the cancel goes out over the old connection.
        for id in queries {
            self.stop_queue_on(Tab::Query(id), cx);
            self.stop_run(Tab::Query(id), cx);
        }

        let index = self.active;
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
            tab.queued_results = 0;
            tab.last_query = None;
            tab.sent_from = None;
            tab.ran_from = None;
            tab.plan = None;
            tab.showing_plan = false;
        }
        profile.session.clear_prompts();
        profile.session.apply_review = None;
        profile.databases = Databases::default();
        profile.config.set_database(name);
        self.remember_profiles(cx);
        self.reconnect(index, cx);
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
        self.switcher_open = false;
        // The form branch of `Render` returns before painting the modal, so a
        // flag left set would reappear the moment the form closes.
        self.settings_open = false;
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
                workspace.switcher_open = false;
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
        profile.catalog = CatalogState::Loading;

        let id = profile.id.clone();
        let mut config = profile.config.clone();
        let mode = profile.mode;
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
                    profile.state = match result {
                        Ok(connection) => ProfileState::Connected(Box::new(connection)),
                        Err(message) => ProfileState::Failed(message),
                    };
                    workspace.load_catalog(&id, generation, cx);
                    // The tab in front is already being looked at, so a
                    // reconnect has no later visit to re-run it on.
                    let active = workspace
                        .profile()
                        .filter(|profile| profile.id == id && profile.connection().is_some());
                    if let Some(Tab::Object(object)) = active.map(|profile| profile.session.active)
                    {
                        workspace.load_relation(object, cx);
                    }
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
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let provider = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => Some(Rc::new(SchemaCompletions::new(
                profile.config.engine().syntax(),
                Arc::new(catalog.clone()),
                profile.session.completion_columns.clone(),
                cx.weak_entity(),
            )) as Rc<dyn CompletionProvider>),
            _ => None,
        };

        // Every buffer, not just the one in front: a tab switch must not be a
        // moment where completion quietly stops working.
        for tab in &profile.session.queries {
            tab.editor.update(cx, |editor, _| {
                editor.lsp_mut().completion_provider = provider.clone();
            });
        }
    }

    pub(crate) fn refresh_explorer(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let filter = profile.session.explorer_filter.read(cx).value();
        let explorer = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => build_explorer_tree(catalog, &filter),
            _ => explorer::ExplorerTree {
                items: Vec::new(),
                leaves: HashMap::new(),
            },
        };
        let tree = profile.session.explorer_tree.clone();

        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) {
            profile.session.explorer_leaves = Arc::new(explorer.leaves);
        }
        tree.update(cx, |tree, cx| tree.set_items(explorer.items, cx));
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
        self.switcher_open = false;
        self.pending_removal = None;
        // Written here rather than at quit, so the profile in front survives a
        // crash as well as a close.
        self.remember_profiles(cx);
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
            profile.session.clear_prompts();
        }
        self.connect_active(cx);
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

    pub(crate) fn remove_profile(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let id = profile.id.clone();
        let name = profile.name.clone();
        if self.pending_removal.as_deref() != Some(&id) {
            self.pending_removal = Some(id);
            cx.notify();
            return;
        }
        if index == self.active
            && let Err(message) = self.persist_buffer(cx)
        {
            self.note(message, cx);
            return;
        }

        // Counted before the directory goes, because afterwards there is
        // nothing left to count and the number is what the note reports.
        let queries = store::saved_queries(&id).len();

        let removed = self.profiles.remove(index);
        if let Some(key) = identity_file(&removed.config)
            && let Ok(directory) = store::ssh_key_directory()
            && is_unshared_imported_key(
                key,
                &directory,
                self.profiles
                    .iter()
                    .filter_map(|profile| identity_file(&profile.config)),
            )
        {
            let _ = std::fs::remove_file(key);
        }
        store::delete_password(&id);
        let removed_queries = store::delete_queries(&id);
        let _ = store::delete_grids(&id);
        self.pending_removal = None;
        for project in &mut self.projects {
            project.connections.retain(|member| member != &id);
        }
        self.active = active_after_removal(self.active, index, self.profiles.len());
        self.remember_profiles(cx);
        // The switcher lives in a titlebar the welcome surface does not have.
        if self.profiles.is_empty() {
            self.switcher_open = false;
            self.refocus_front();
        }
        self.connect_active(cx);
        self.note(removal_note(&name, queries, removed_queries.err()), cx);
    }
}

/// An entry in the connection form's project list.
#[derive(Clone, PartialEq)]
enum ProjectChoice {
    /// An existing project, or `None` for No project.
    In(Option<String>),
    New,
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

#[cfg(test)]
mod tests {
    use super::*;

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

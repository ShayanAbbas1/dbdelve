//! Opening, saving, restoring and switching between connections.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;
use crate::sql::{Destructive, Mode};

impl Workspace {
    pub(crate) fn remember_profiles(&mut self, cx: &mut Context<Self>) {
        // The file we could not read at startup is still the user's, and an
        // empty list is not what they have -- so a session that never managed
        // to read it must not flatten it the moment nothing has been loaded
        // into `profiles` yet. Once a profile exists (including the last one
        // being deliberately removed) this no longer applies.
        if self.store_unreadable && self.profiles.is_empty() {
            return;
        }
        let profiles = self
            .profiles
            .iter()
            .map(|profile| profile.stored(cx))
            .collect::<Vec<_>>();
        let active = self.profile().map(|profile| profile.id.clone());
        let picked = fonts(cx);
        let fonts = store::StoredFonts {
            chrome: Some(picked.chrome.to_string()),
            editor: Some(picked.editor.to_string()),
            grid: Some(picked.grid.to_string()),
        };
        let settings = store::StoredSettings {
            theme: Some(theme(cx).name.to_string()),
            editor_font_size: Some(self.settings.editor_font_size),
            preview_rows: Some(self.settings.preview_rows),
            opacity: Some(self.settings.opacity),
            custom_keybindings: Some(self.settings.custom_keybindings.clone()),
        };
        if let Err(message) = store::save_profiles(&profiles, active.as_deref(), &fonts, &settings)
        {
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
            Engine::Postgres | Engine::MySql => {
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
                };
                match engine {
                    Engine::MySql => ConnectionConfig::MySql(server),
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
        // A profile written before a buffer was a tab carries one buffer, whose
        // name is in the legacy scalar and whose text `read_scratch` migrates.
        let stored_queries = if stored.open_queries.is_empty() {
            vec![store::StoredQueryTab {
                id: 0,
                name: stored.open_query.clone(),
                active: true,
            }]
        } else {
            stored.open_queries
        };
        // Snapshots whose tab is gone -- a renamed table strands its file
        // under the old name, and nothing else will ever remove it.
        let live_grids =
            stored_queries
                .iter()
                .map(|tab| store::query_grid_key(tab.id))
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
            generation: 0,
            state: ProfileState::Idle,
            catalog: CatalogState::Loading,
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
        let session = Session::new(id.clone(), Vec::new(), 0, Vec::new(), window, cx);
        let password = password_to_persist(&config, origin).map(str::to_string);
        self.profiles.push(Profile {
            id: id.clone(),
            name,
            config,
            color,
            mode,
            confirmed: Vec::new(),
            generation: 0,
            state: ProfileState::Idle,
            catalog: CatalogState::Loading,
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
            ConnectionConfig::Postgres(server) | ConnectionConfig::MySql(server) => vec![
                (&form.name, server.database.clone()),
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
        for (input, value) in filled {
            let input = input.clone();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }

        let engine = config.engine();
        let sslmode = config.server().map(|server| server.sslmode);
        if let Some(form) = &mut self.form {
            form.engine = engine;
            if let Some(sslmode) = sslmode {
                // The URL's own mode, so pasting one that demands verification
                // cannot land in a form still set to `prefer`.
                form.sslmode = sslmode;
            }
            form.error = None;
        }
        cx.notify();
    }

    /// One chip per engine, in the same shape as the `sslmode` row below it.
    /// The engine decides which fields the form even has, so it is the first
    /// thing on it and not a dropdown two clicks away.
    pub(crate) fn engine_chip(&self, engine: Engine, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.engine == engine);
        div()
            .id(engine.as_str())
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
            .child(engine.label())
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
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
                    // The error belonged to the fields that just left the
                    // screen, so it would be reporting something invisible.
                    form.error = None;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// One chip per mode, weakest first. A row of three words rather than a
    /// dropdown: the choice is the security of the connection, and it should
    /// be legible without opening anything.
    ///
    /// Read only while a connection already exists: past creation,
    /// `Workspace::set_mode` is the one door a mode changes through, and it
    /// pushes the change into that connection's live grids -- something a
    /// profile still being typed into has none of yet. `render_connection_form`
    /// draws this row only when there is no `editing` id, for exactly that
    /// reason.
    pub(crate) fn mode_chip(&self, mode: Mode, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.mode == mode);
        div()
            .id(mode.label())
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
                    form.mode = mode;
                    cx.notify();
                }
            }))
            .into_any_element()
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

    /// One chip per swatch, plus the no-colour option the row opens on. The
    /// colour only ever labels a connection, so nothing here can make the form
    /// invalid and nothing has to move focus.
    pub(crate) fn color_chip(
        &self,
        color: Option<ConnectionColor>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.color == color);
        div()
            .id(color.map_or("none", ConnectionColor::slug))
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
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
            .children(color.map(|color| {
                div()
                    .size(px(layout::SPACE_SM))
                    .rounded_full()
                    .bg(color.swatch())
            }))
            .child(color.map_or("None", ConnectionColor::label))
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    form.color = color;
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
        let (name, config) = match form.config(cx) {
            Ok(profile) => profile,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };

        self.form = None;
        match editing {
            Some(id) => self.save_profile(&id, name, config, color, cx),
            None => {
                let index =
                    self.create_profile(name, config, color, mode, Origin::Form, window, cx);
                self.activate(index, cx);
            }
        }
    }

    pub(crate) fn save_profile(
        &mut self,
        id: &str,
        name: String,
        config: ConnectionConfig,
        color: Option<ConnectionColor>,
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
            // The rows on an object tab are the old database's. Their statement
            // is dbdelve's own, so it re-runs the moment the tab is looked at
            // again -- a query tab holds SQL the user wrote and is theirs to
            // re-run.
            for tab in &mut self.profiles[index].session.objects {
                if let ObjectBody::Relation { stale, .. } = &mut tab.body {
                    *stale = true;
                }
            }
            self.begin_connect(index, cx);
        }
        cx.notify();
    }

    pub(crate) fn open_connection_form(
        &mut self,
        _: &NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = Some(ConnectionForm::new(None, window, cx));
        self.switcher_open = false;
        // The form branch of `Render` returns before painting the modal, so a
        // flag left set would reappear the moment the form closes.
        self.settings_open = false;
        cx.notify();
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
                        Ok(connection) => ProfileState::Connected(connection),
                        Err(message) => ProfileState::Failed(message),
                    };
                    workspace.load_catalog(&id, generation, cx);
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
                        Ok(catalog) => CatalogState::Loaded(catalog),
                        Err(error) => CatalogState::Failed(error.message),
                    };
                    // Relations restore before the catalog arrives, wearing
                    // whatever kind was on disk -- a default, for a profile an
                    // older build wrote. This is the first moment there is
                    // anything to correct it from.
                    if let CatalogState::Loaded(catalog) = &profile.catalog {
                        for tab in &mut profile.session.objects {
                            if let ObjectKind::Relation(kind) = &mut tab.kind
                                && let Some(actual) = relation_kind(catalog, &tab.schema, &tab.name)
                            {
                                *kind = actual;
                            }
                        }
                    }
                    workspace.install_completions(&id, cx);
                    workspace.refresh_explorer(&id, cx);
                    cx.notify();
                    workspace.load_routines(&id, generation, cx);
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
                    match result {
                        // Only onto a catalog that loaded. One that failed, or
                        // that a reconnect has already replaced, is not this
                        // half's to complete.
                        Ok(routines) => {
                            if let CatalogState::Loaded(catalog) = &mut profile.catalog {
                                catalog.merge(routines);
                            }
                        }
                        Err(error) => workspace.note(error.message, cx),
                    }
                    workspace.install_completions(&id, cx);
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
        // The catalog is new, so what was known about any relation's columns
        // describes a schema that may no longer exist.
        profile.session.completion_columns.borrow_mut().clear();

        let provider = match &profile.catalog {
            CatalogState::Loaded(catalog) => Some(Rc::new(SchemaCompletions::new(
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
            CatalogState::Loaded(catalog) => build_explorer_tree(catalog, &filter),
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
        if self.profiles.len() < 2 || self.form.is_some() {
            return;
        }
        let count = self.profiles.len() as isize;
        let index = (self.active as isize + step).rem_euclid(count) as usize;
        self.activate(index, cx);
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

    pub(crate) fn remove_profile(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

        self.profiles.remove(index);
        store::delete_password(&id);
        let removed_queries = store::delete_queries(&id);
        let _ = store::delete_grids(&id);
        self.pending_removal = None;
        self.active = active_after_removal(self.active, index, self.profiles.len());
        self.remember_profiles(cx);
        if self.profiles.is_empty() {
            self.form = Some(ConnectionForm::new(None, window, cx));
        } else {
            self.connect_active(cx);
            self.note(removal_note(&name, queries, removed_queries.err()), cx);
        }
        cx.notify();
    }
}

/// Removing an entry below the active one shifts the vector under the index,
/// so clamping to the new length alone silently activates the wrong profile.
pub(crate) fn active_after_removal(active: usize, removed: usize, remaining: usize) -> usize {
    let shifted = if removed < active { active - 1 } else { active };
    shifted.min(remaining.saturating_sub(1))
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

#[cfg(test)]
mod tests {
    use super::*;

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

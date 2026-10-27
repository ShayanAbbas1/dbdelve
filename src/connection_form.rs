//! The connection form: the fields a profile is created and edited through.
//!
//! The form holds inputs rather than a config, because a half-typed connection
//! is not one yet -- `config` is where the fields become something that can be
//! opened.
//!
//! This was a plain type at the crate root. It moved out whole; nothing changed
//! but its visibility.

use std::time::Instant;

use gpui::{App, AppContext, Context, Entity, SharedString, Subscription, Task, TextAlign, Window};
use gpui_component::input::{InputEvent, InputState};

use crate::{
    Workspace,
    db::{
        ConnectionConfig, Engine, Fields, MongoConfig, ServerConfig, SnowflakeConfig, SshTunnel,
        SslMode,
    },
    session::Profile,
    sql::Mode,
    theme::ConnectionColor,
};

pub(crate) struct ConnectionForm {
    pub(crate) url: Entity<InputState>,
    /// Which set of fields below is the connection. Every input is built once
    /// and kept; the engine decides which are drawn and which are read, so
    /// switching engine and switching back does not lose what was typed.
    pub(crate) engine: Engine,
    pub(crate) name: Entity<InputState>,
    pub(crate) color: Option<ConnectionColor>,
    /// What the connection will be allowed to do once it exists. Read only on
    /// creation -- `Workspace::set_mode` is the one place it changes once a
    /// profile is connecting, and it pushes into that profile's live grids,
    /// which a profile still being typed into does not have.
    pub(crate) mode: Mode,
    /// SQLite's entire connection. No host, no credentials, no transport.
    pub(crate) path: Entity<InputState>,
    pub(crate) host: Entity<InputState>,
    pub(crate) port: Entity<InputState>,
    pub(crate) database: Entity<InputState>,
    pub(crate) user: Entity<InputState>,
    pub(crate) password: Entity<InputState>,
    pub(crate) sslmode: SslMode,
    /// Only reachable while the mode consults one, so the field cannot sit
    /// there filled in and doing nothing.
    pub(crate) root_certificate: Entity<InputState>,
    /// Off, the SSH fields are kept but not read, the way another engine's
    /// fields are, so turning it back on finds them as they were typed.
    pub(crate) ssh: bool,
    pub(crate) ssh_host: Entity<InputState>,
    pub(crate) ssh_port: Entity<InputState>,
    pub(crate) ssh_user: Entity<InputState>,
    pub(crate) ssh_identity_file: Entity<InputState>,
    /// What an account has that a server does not. `host`, `database` and
    /// `user` are shared with the server fields: they mean the same thing, and
    /// sharing them is what keeps a value typed under one engine there under the
    /// next.
    pub(crate) account: Entity<InputState>,
    pub(crate) private_key: Entity<InputState>,
    pub(crate) warehouse: Entity<InputState>,
    pub(crate) role: Entity<InputState>,
    /// Driver options passed through as typed, where `Engine::takes_options`.
    pub(crate) options: Entity<InputState>,
    /// The host is a DNS name listing the servers, where
    /// `Engine::resolves_srv`.
    pub(crate) srv: bool,
    /// The profile's login database, which the form does not show and keeps
    /// as it was.
    pub(crate) login_database: Option<String>,
    /// Seconds, and blank is the same as 0: no limit. Every engine has one, so
    /// unlike the credential fields it is drawn whichever engine is selected.
    pub(crate) statement_timeout: Entity<InputState>,
    /// An input to focus once it has been mounted.
    ///
    /// A picker can unmount the field the user was typing in, and a window with
    /// nothing focused has no dispatch path — every keybinding in the app goes
    /// dead until something is clicked. So whichever picker takes a field away
    /// names the one that replaces it, and `Workspace::render` hands focus over
    /// on the next frame, once it exists to receive it.
    pub(crate) needs_focus: Option<Entity<InputState>>,
    pub(crate) error: Option<String>,
    pub(crate) test: Option<ConnectionTest>,
    /// The id of the profile being edited, or `None` for a new connection.
    pub(crate) editing: Option<String>,
    /// The project the connection is saved into, or `None` for No project.
    pub(crate) project: Option<String>,
    /// Whether the project control is the name field for a new project, made
    /// when the form is saved, rather than the list of existing ones.
    pub(crate) naming_project: bool,
    pub(crate) project_name: Entity<InputState>,
    /// When the form came up, for its card's entrance.
    pub(crate) opened: Instant,
    /// The welcome surface's contours were already on screen, so the form's,
    /// the same ones, are there from the start rather than fading in again.
    pub(crate) backdrop_shown: bool,
    /// The engine the engine-dependent placeholders were last written for.
    pub(crate) placeholders_for: Option<Engine>,
    /// The URL field as of its last change, to tell a URL pasted whole from
    /// one being typed.
    pub(crate) last_url: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl ConnectionForm {
    /// The first field the form draws for its engine.
    pub(crate) fn lead_field(&self) -> Entity<InputState> {
        match self.engine.fields() {
            Fields::Server => self.url.clone(),
            Fields::File => self.path.clone(),
            Fields::Account => self.account.clone(),
        }
    }

    pub(crate) fn set_engine(&mut self, engine: Engine) {
        // Only when the field set actually changes: Postgres and MySQL show
        // the same fields, so a switch between them leaves focus where the
        // palette hands it back (the lead field). Only a change of field set
        // moves it, to the new set's first field.
        if self.engine.fields() != engine.fields() {
            self.needs_focus = Some(match engine.fields() {
                Fields::Server => self.host.clone(),
                Fields::File => self.path.clone(),
                Fields::Account => self.account.clone(),
            });
        }
        self.engine = engine;
        // Both belonged to the fields that just left the screen.
        self.error = None;
        self.test = None;
    }

    pub(crate) fn new(
        config: Option<&ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let value = |value: Option<&str>| value.unwrap_or_default().to_string();
        let server = config.and_then(ConnectionConfig::server);
        let account = match config {
            Some(ConnectionConfig::Snowflake(account)) => Some(account),
            _ => None,
        };
        let file = match config {
            Some(
                ConnectionConfig::Sqlite { path, .. }
                | ConnectionConfig::Turso { path, .. }
                | ConnectionConfig::DuckDb { path, .. },
            ) => Some(path.as_str()),
            _ => None,
        };
        let mongo = match config {
            Some(ConnectionConfig::MongoDb(mongo)) => Some(mongo),
            _ => None,
        };

        let url = cx.new(|cx| InputState::new(window, cx).placeholder("Paste a connection URL"));
        let name = cx.new(|cx| {
            InputState::new(window, cx).default_value(value(
                server
                    .map(|server| server.database.as_str())
                    .or_else(|| account.map(|account| account.database.as_str()))
                    .or_else(|| file.map(file_stem)),
            ))
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/to/app.sqlite")
                .default_value(value(file))
        });
        let host = cx.new(|cx| {
            InputState::new(window, cx).default_value(value(
                server
                    .map(|server| server.host.as_str())
                    .or_else(|| account.and_then(|account| account.host.as_deref())),
            ))
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                server
                    .and_then(|server| server.port)
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            )
        });
        let database = cx.new(|cx| {
            InputState::new(window, cx).default_value(value(
                server
                    .map(|server| server.database.as_str())
                    .or_else(|| account.map(|account| account.database.as_str())),
            ))
        });
        let user = cx.new(|cx| {
            InputState::new(window, cx).default_value(value(
                server
                    .map(|server| server.user.as_str())
                    .or_else(|| account.map(|account| account.user.as_str())),
            ))
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional")
                .default_value(value(server.map(|server| server.password.as_str())))
                .masked(true)
        });

        let root_certificate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("System trust store")
                .default_value(value(
                    server.and_then(|server| server.root_certificate.as_deref()),
                ))
        });
        let ssh = server.and_then(|server| server.ssh.as_ref());
        let [ssh_host, ssh_port, ssh_user, ssh_identity_file] = ssh_fields(ssh);
        let mut input = |placeholder: &'static str, value: String| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let ssh_host = input("Host or ~/.ssh/config alias", ssh_host);
        let ssh_port = input("22", ssh_port);
        let ssh_user = input("Optional", ssh_user);
        let ssh_identity_file = input("SSH agent", ssh_identity_file);
        let account_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Account identifier")
                .default_value(value(account.map(|account| account.account.as_str())))
        });
        let private_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/absolute/path/to/key.p8")
                .default_value(value(account.map(|account| account.private_key.as_str())))
        });
        let warehouse = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional")
                .default_value(value(
                    account.and_then(|account| account.warehouse.as_deref()),
                ))
        });
        let role = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional")
                .default_value(value(account.and_then(|account| account.role.as_deref())))
        });
        let options = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("authSource=admin&replicaSet=rs0")
                .default_value(value(mongo.map(|mongo| mongo.options.as_str())))
        });
        let project_name = cx.new(|cx| InputState::new(window, cx).placeholder("Project name"));
        let statement_timeout = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("No limit")
                .default_value(
                    config
                        .map(ConnectionConfig::statement_timeout)
                        .filter(|seconds| *seconds > 0)
                        .map(|seconds| seconds.to_string())
                        .unwrap_or_default(),
                )
        });

        statement_timeout.update(cx, |input, cx| input.set_text_align(TextAlign::Right, cx));

        let mut subscriptions = vec![cx.subscribe_in(
            &url,
            window,
            |workspace, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => workspace.url_changed(window, cx),
                InputEvent::PressEnter { .. } => workspace.fill_from_url(window, cx),
                _ => {}
            },
        )];
        // What the header reads: its title and the line saying where the
        // form points.
        for input in [
            &name,
            &path,
            &host,
            &port,
            &database,
            &user,
            &ssh_host,
            &account_name,
        ] {
            subscriptions.push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }

        let mut form = Self {
            needs_focus: None,
            url,
            engine: config.map(ConnectionConfig::engine).unwrap_or_default(),
            name,
            color: None,
            mode: starting_mode(None),
            path,
            host,
            port,
            database,
            user,
            password,
            sslmode: server.map(|server| server.sslmode).unwrap_or_default(),
            root_certificate,
            ssh: ssh.is_some(),
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_identity_file,
            account: account_name,
            private_key,
            warehouse,
            role,
            options,
            srv: mongo.is_some_and(|mongo| mongo.srv),
            login_database: mongo.and_then(|mongo| mongo.login_database.clone()),
            statement_timeout,
            error: None,
            test: None,
            editing: None,
            project: None,
            naming_project: false,
            project_name,
            opened: Instant::now(),
            backdrop_shown: false,
            placeholders_for: None,
            last_url: SharedString::default(),
            _subscriptions: subscriptions,
        };
        // The form is the whole window while it is open, and a window with
        // nothing focused has no dispatch path -- every binding is dead until
        // a field is clicked. So the field the user is meant to start in asks
        // for focus the moment it is mounted.
        form.needs_focus = Some(form.lead_field());
        form
    }

    /// The same form, pointed at a profile that already exists.
    ///
    /// Its name is the profile's own rather than the database name a fresh form
    /// falls back to, and the password starts blank: what the Keychain holds is
    /// never read back onto the screen, so leaving it alone keeps it.
    pub(crate) fn editing(
        profile: &Profile,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut form = Self::new(Some(&profile.config), window, cx);
        form.editing = Some(profile.id.clone());
        form.color = profile.color;
        // No `form.mode` here: the mode dropdown draws only on a fresh form and
        // `save_profile` has no mode to take, so copying it in was a write
        // nothing ever read. The titlebar picker is where an existing
        // connection's mode changes.
        form.name.update(cx, |name, cx| {
            name.set_value(profile.name.clone(), window, cx);
        });
        form.password.update(cx, |password, cx| {
            password.set_value("", window, cx);
            password.set_placeholder("Unchanged", window, cx);
        });
        form
    }

    /// Seeded from `profile` but never bound to it: no `editing` id, so saving
    /// creates a new profile. Unlike `editing`, the password is prefilled,
    /// because a duplicate has no Keychain entry of its own to fall back to.
    pub(crate) fn duplicating(
        profile: &Profile,
        name: String,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut config = profile.config.clone();
        if let Some(password) = password
            && let Some(server) = config.server_mut()
        {
            server.password = password;
        }
        let mut form = Self::new(Some(&config), window, cx);
        form.color = profile.color;
        form.mode = starting_mode(Some(profile.mode));
        form.name.update(cx, |input, cx| {
            input.set_value(name, window, cx);
        });
        form
    }

    pub(crate) fn config(&self, cx: &App) -> Result<(String, ConnectionConfig), String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let statement_timeout = self.statement_timeout(cx)?;
        let file = || match read(&self.path) {
            path if path.is_empty() => Err("Database file is required.".to_string()),
            path => Ok(path),
        };
        let config = match self.engine {
            Engine::Sqlite => ConnectionConfig::Sqlite {
                path: file()?,
                statement_timeout,
            },
            Engine::Turso => ConnectionConfig::Turso {
                path: file()?,
                statement_timeout,
            },
            // No file is a database in memory, which is what a profile for
            // reading CSV and Parquet files wants.
            Engine::DuckDb => ConnectionConfig::duckdb(
                match read(&self.path) {
                    path if path.is_empty() => crate::db::DUCKDB_IN_MEMORY.to_string(),
                    path => path,
                },
                statement_timeout,
            ),
            Engine::Postgres => ConnectionConfig::Postgres(self.server(cx)?),
            Engine::MySql => ConnectionConfig::MySql(self.server(cx)?),
            Engine::MariaDb => ConnectionConfig::MariaDb(self.server(cx)?),
            Engine::SqlServer => ConnectionConfig::SqlServer(self.server(cx)?),
            // A blank username sends none, and the server signs in `default`.
            Engine::ClickHouse => ConnectionConfig::ClickHouse(self.server_requiring(&[], cx)?),
            Engine::Snowflake => ConnectionConfig::Snowflake(self.account(cx)?),
            Engine::MongoDb => {
                let mut server = self.server_requiring(&[], cx)?;
                // Hidden while SRV is on, so whatever it held is not asked for.
                if self.srv {
                    server.port = None;
                }
                ConnectionConfig::MongoDb(MongoConfig {
                    server,
                    srv: self.srv,
                    login_database: self.login_database.clone(),
                    options: read(&self.options),
                })
            }
            // An ACL user and a password are both optional: a server with
            // neither configured takes any connection.
            Engine::Redis => ConnectionConfig::Redis(self.server_requiring(&[], cx)?),
        };

        let name = match read(&self.name) {
            name if name.is_empty() => default_profile_name(&config),
            name => name,
        };
        if name.is_empty() {
            return Err("Display name is required.".into());
        }
        Ok((name, config))
    }

    /// The placeholders that say what a blank field means, which is the
    /// engine's to say: its port, whether it asks for a user, what it names
    /// a connection after. Written whenever the engine has changed since.
    pub(crate) fn sync_placeholders(&mut self, window: &mut Window, cx: &mut App) {
        if self.placeholders_for == Some(self.engine) {
            return;
        }
        self.placeholders_for = Some(self.engine);
        let blank = placeholders(self.engine);
        for (input, placeholder) in [
            (&self.name, SharedString::from(blank.name)),
            (&self.host, blank.host.into()),
            (&self.port, blank.port.into()),
            (&self.database, blank.database.into()),
            (&self.user, blank.user.into()),
            (&self.path, blank.path.into()),
        ] {
            input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
    }

    /// Where the form points, for the line under its title.
    pub(crate) fn summary(&self, cx: &App) -> Option<String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let tunnel = read(&self.ssh_host);
        summary(
            self.engine,
            self.srv,
            &Typed {
                host: &read(&self.host),
                port: &read(&self.port),
                database: &read(&self.database),
                user: &read(&self.user),
                path: &read(&self.path),
                account: &read(&self.account),
                tunnel: self.ssh.then_some(tunnel.as_str()),
            },
        )
    }

    /// Blank is 0 is no limit, so a user who never had an opinion about it is
    /// not made to have one.
    pub(crate) fn statement_timeout(&self, cx: &App) -> Result<u32, String> {
        let value = self.statement_timeout.read(cx).value().trim().to_string();
        if value.is_empty() {
            return Ok(0);
        }
        value
            .parse()
            .map_err(|_| "Statement timeout must be a whole number of seconds.".to_string())
    }

    pub(crate) fn account(&self, cx: &App) -> Result<SnowflakeConfig, String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let optional =
            |input: &Entity<InputState>| Some(read(input)).filter(|value| !value.is_empty());
        let account = crate::db::account_identifier(&read(&self.account));
        let user = read(&self.user);
        let private_key = read(&self.private_key);
        let database = read(&self.database);

        for (label, value) in [
            ("Account", &account),
            ("Username", &user),
            ("Private key", &private_key),
            ("Database", &database),
        ] {
            if value.is_empty() {
                return Err(format!("{label} is required."));
            }
        }
        // Absolute, because a relative one resolves against wherever the app
        // was launched from -- `/` for one opened from Finder -- and `~` is the
        // shell's to expand, not the file system's.
        if !private_key.starts_with('/') {
            return Err("Private key must be an absolute path to the key file.".into());
        }

        Ok(SnowflakeConfig {
            account,
            // Blank is the host the account implies, which is nearly always
            // the right one.
            host: optional(&self.host).map(|host| crate::db::normalize_host(&host)),
            user,
            private_key,
            database,
            warehouse: optional(&self.warehouse),
            role: optional(&self.role),
            statement_timeout: self.statement_timeout(cx)?,
            schema: None,
        })
    }

    pub(crate) fn server(&self, cx: &App) -> Result<ServerConfig, String> {
        self.server_requiring(&["Username"], cx)
    }

    /// The server fields, with the host and whichever of `required` always
    /// required: MongoDB asks for no username, since a server without
    /// authentication is an ordinary one there.
    fn server_requiring(&self, required: &[&str], cx: &App) -> Result<ServerConfig, String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let host = read(&self.host);
        let database = read(&self.database);
        let user = read(&self.user);
        let port = read(&self.port);

        // Blank database is the one the server signs the login into, which is
        // what a profile that moves between databases starts on.
        for (label, value) in [("Host", &host), ("Username", &user)] {
            if value.is_empty() && (label == "Host" || required.contains(&label)) {
                return Err(format!("{label} is required."));
            }
        }

        let port = parse_port("Port", &port)?;

        // Kept only where it is consulted. A path left behind by switching down
        // to `require` would be stored and shown as though it were in force.
        let root_certificate = self
            .sslmode
            .checks_certificate()
            .then(|| read(&self.root_certificate))
            .filter(|path| !path.is_empty());

        Ok(ServerConfig {
            host,
            port,
            database,
            user,
            password: self.password.read(cx).unmask_value().to_string(),
            sslmode: self.sslmode,
            root_certificate,
            statement_timeout: self.statement_timeout(cx)?,
            ssh: ssh_tunnel(
                self.ssh,
                [
                    read(&self.ssh_host),
                    read(&self.ssh_port),
                    read(&self.ssh_user),
                    read(&self.ssh_identity_file),
                ],
            )?,
            schema: None,
        })
    }
}

/// The active schema `current` holds, picked in the titlebar rather than the
/// form, onto `config`, only while it is on the same engine and database:
/// schemas belong to one database. Read from the profile at save time, so one
/// picked while the form was open stays. The database is compared as the
/// engine spells it, so `analytics` is still Snowflake's `ANALYTICS`.
///
/// DuckDB's database is picked in the titlebar too, among those attached to
/// the file, and the form has no field for it: it stays while the file does.
pub(crate) fn keep_schema(config: &mut ConnectionConfig, current: &ConnectionConfig) {
    if let (
        ConnectionConfig::DuckDb { path, database, .. },
        ConnectionConfig::DuckDb {
            path: was,
            database: picked,
            ..
        },
    ) = (&mut *config, current)
        && path == was
    {
        *database = picked.clone();
    }
    if let Some(schema) = current.schema()
        && config.engine() == current.engine()
        && config.database_name() == current.database_name()
    {
        config.set_schema(Some(schema.to_string()));
    }
}

/// What a new connection may do until told otherwise: read. A copy keeps the
/// mode of the connection it was copied from.
pub(crate) fn starting_mode(copied_from: Option<Mode>) -> Mode {
    copied_from.unwrap_or(Mode::ReadOnly)
}

/// What a blank field means, field by field.
pub(crate) struct Placeholders {
    pub(crate) name: &'static str,
    pub(crate) host: &'static str,
    pub(crate) port: String,
    pub(crate) database: &'static str,
    pub(crate) user: &'static str,
    pub(crate) path: &'static str,
}

/// Read against `ConnectionConfig::server`, `account` and
/// `default_profile_name`, which decide what blank actually does.
pub(crate) fn placeholders(engine: Engine) -> Placeholders {
    let hints = engine.spec().hints;
    Placeholders {
        name: match engine.fields() {
            Fields::File => "Named after the file",
            Fields::Server | Fields::Account => hints.name,
        },
        host: hints.host,
        port: engine
            .default_port()
            .map(|port| port.to_string())
            .unwrap_or_default(),
        database: hints.database,
        user: hints.user,
        path: hints.path,
    }
}

/// The fields `summary` reads, as typed and trimmed.
#[derive(Clone, Copy, Default)]
pub(crate) struct Typed<'a> {
    pub(crate) host: &'a str,
    pub(crate) port: &'a str,
    pub(crate) database: &'a str,
    pub(crate) user: &'a str,
    pub(crate) path: &'a str,
    pub(crate) account: &'a str,
    /// The SSH host, while the tunnel is on.
    pub(crate) tunnel: Option<&'a str>,
}

/// Where a form points, in one line: `user@host:port/database` and the SSH
/// host it goes through, the file for a file, the account for an account.
/// `None` until there is somewhere: a user on its own points nowhere.
pub(crate) fn summary(engine: Engine, srv: bool, typed: &Typed) -> Option<String> {
    let signed_in = |place: String| match typed.user {
        "" => place,
        user => format!("{user}@{place}"),
    };
    let database = |place: String| match typed.database {
        "" => place,
        database => format!("{place}/{database}"),
    };
    match engine.fields() {
        Fields::File => Some(typed.path.to_string()).filter(|path| !path.is_empty()),
        Fields::Account => {
            (!typed.account.is_empty()).then(|| signed_in(database(typed.account.to_string())))
        }
        Fields::Server => {
            if typed.host.is_empty() {
                return None;
            }
            // An SRV name takes no port, so whatever the hidden field holds
            // is not where it points.
            let place = match typed.port {
                port if !port.is_empty() && !(srv && engine.resolves_srv()) => {
                    format!("{}:{port}", typed.host)
                }
                _ => typed.host.to_string(),
            };
            let line = signed_in(database(place));
            Some(match typed.tunnel {
                Some(tunnel) if !tunnel.is_empty() => format!("{line} via {tunnel}"),
                _ => line,
            })
        }
    }
}

/// The SSH fields' text for `ssh`, in the order `ssh_tunnel` reads it back.
fn ssh_fields(ssh: Option<&SshTunnel>) -> [String; 4] {
    let ssh = ssh.cloned().unwrap_or_default();
    [
        ssh.host,
        ssh.port.map(|port| port.to_string()).unwrap_or_default(),
        ssh.user,
        ssh.identity_file.unwrap_or_default(),
    ]
}

/// Blank port, user and identity file defer to `~/.ssh/config`, which is what
/// lets a config alias carry the whole of it.
fn ssh_tunnel(
    on: bool,
    [host, port, user, identity_file]: [String; 4],
) -> Result<Option<SshTunnel>, String> {
    if !on {
        return Ok(None);
    }
    if host.is_empty() {
        return Err("SSH host is required.".into());
    }
    if host.starts_with('-') {
        return Err("SSH host must not start with '-'.".into());
    }
    let identity_file = Some(identity_file).filter(|path| !path.is_empty());
    if let Some(error) = identity_file
        .as_deref()
        .and_then(SshTunnel::identity_file_error)
    {
        return Err(error);
    }
    Ok(Some(SshTunnel {
        host,
        port: parse_port("SSH port", &port)?,
        user,
        identity_file,
    }))
}

/// Blank is the default port.
fn parse_port(label: &str, value: &str) -> Result<Option<u16>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse() {
        Ok(0) | Err(_) => Err(format!("{label} must be a number from 1 to 65535.")),
        Ok(port) => Ok(Some(port)),
    }
}

/// The Test button's last answer. Dropping `Running` drops its task, so a
/// form closed or retested mid-connect never hears back from the stale one.
pub(crate) enum ConnectionTest {
    Running(Task<()>),
    Passed,
    Failed(String),
}

/// What a profile is called when nobody has named it: the database for an
/// engine that has one, the host when the database was left to the server, and
/// the file for an engine that is one.
pub(crate) fn default_profile_name(config: &ConnectionConfig) -> String {
    match config {
        ConnectionConfig::Postgres(server)
        | ConnectionConfig::MySql(server)
        | ConnectionConfig::MariaDb(server)
        | ConnectionConfig::SqlServer(server)
        | ConnectionConfig::ClickHouse(server)
        | ConnectionConfig::MongoDb(MongoConfig { server, .. }) => if server.database.is_empty() {
            &server.host
        } else {
            &server.database
        }
        .clone(),
        ConnectionConfig::DuckDb { path, .. } if path == crate::db::DUCKDB_IN_MEMORY => {
            "DuckDB".to_string()
        }
        ConnectionConfig::Sqlite { path, .. }
        | ConnectionConfig::Turso { path, .. }
        | ConnectionConfig::DuckDb { path, .. } => file_stem(path).to_string(),
        ConnectionConfig::Snowflake(account) => account.database.clone(),
        // A database there is a number, which names nothing.
        ConnectionConfig::Redis(server) => server.host.clone(),
    }
}

/// Checks display names rather than ids: `store::profile_id` already dedupes
/// the id, but two rows with the same name in the palette are ambiguous.
pub(crate) fn duplicate_profile_name(name: &str, existing: &[String]) -> String {
    (1..)
        .map(|n| match n {
            1 => format!("{name} copy"),
            n => format!("{name} copy {n}"),
        })
        .find(|candidate| !existing.contains(candidate))
        .expect("an unbounded range always finds a free name")
}

/// A database file's name without its directory or extension.
pub(crate) fn file_stem(path: &str) -> &str {
    std::path::Path::new(path)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(path)
}

/// Where a profile's connection details came from, which is what decides
/// whether its password is a saved credential.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    Environment,
    Form,
}

/// The password that earns a Keychain entry, if any.
///
/// A file engine has none. A blank one is valid and never warned about, but an
/// empty Keychain item records nothing and is not written. And a password read
/// out of the environment is ephemeral by the convention that put it there --
/// copying it into the login Keychain would outlive the shell that set it, and
/// the session it belongs to already holds it in the config.
pub(crate) fn password_to_persist(config: &ConnectionConfig, origin: Origin) -> Option<&str> {
    if origin == Origin::Environment {
        return None;
    }
    config
        .server()
        .map(|server| server.password.as_str())
        .filter(|password| !password.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ConnectionConfig, ServerConfig, SslMode};

    fn fields(host: &str, port: &str) -> [String; 4] {
        [host.into(), port.into(), String::new(), String::new()]
    }

    #[test]
    fn the_active_schema_stays_only_on_the_engine_and_database_it_was_picked_on() {
        let postgres = |database: &str| {
            ConnectionConfig::Postgres(ServerConfig {
                database: database.into(),
                ..ServerConfig::default()
            })
        };
        let snowflake = |database: &str| {
            ConnectionConfig::Snowflake(SnowflakeConfig {
                database: database.into(),
                ..SnowflakeConfig::default()
            })
        };
        let kept = |picked_on: &ConnectionConfig, mut saved: ConnectionConfig| {
            let mut current = picked_on.clone();
            current.set_schema(Some("sales".into()));
            keep_schema(&mut saved, &current);
            saved.schema().map(str::to_string)
        };
        let sales = Some("sales".to_string());
        assert_eq!(kept(&postgres("app"), postgres("app")), sales);
        assert_eq!(kept(&postgres("app"), postgres("other")), None);
        // Blank is the `postgres` it connects to.
        assert_eq!(kept(&postgres(""), postgres("postgres")), sales);
        assert_eq!(kept(&postgres("app"), snowflake("app")), None);
        assert_eq!(kept(&snowflake("ANALYTICS"), snowflake("analytics")), sales);
        assert_eq!(
            kept(&snowflake("ANALYTICS"), snowflake(r#""analytics""#)),
            None
        );

        let duckdb = |path: &str| ConnectionConfig::duckdb(path.into(), 0);
        let mut picked = duckdb("/data/dev.duckdb");
        picked.set_database("other".into());
        picked.set_schema(Some("sales".into()));
        let saved = |mut saved: ConnectionConfig| {
            keep_schema(&mut saved, &picked);
            (saved.database_name(), saved.schema().map(str::to_string))
        };
        assert_eq!(
            saved(duckdb("/data/dev.duckdb")),
            (Some("other".into()), sales.clone())
        );
        assert_eq!(saved(duckdb("/data/prod.duckdb")), (None, None));
    }

    #[test]
    fn the_ssh_fields_are_a_tunnel_only_while_it_is_on() {
        assert_eq!(ssh_tunnel(false, fields("bastion", "nope")), Ok(None));
        assert_eq!(
            ssh_tunnel(true, fields("bastion", "")),
            Ok(Some(SshTunnel {
                host: "bastion".into(),
                port: None,
                user: String::new(),
                identity_file: None,
            }))
        );
        assert_eq!(
            ssh_tunnel(true, fields("", "")),
            Err("SSH host is required.".into())
        );
        assert_eq!(
            ssh_tunnel(true, fields("-oProxyCommand=x", "")),
            Err("SSH host must not start with '-'.".into())
        );
        for port in ["0", "65536", "22a"] {
            assert_eq!(
                ssh_tunnel(true, fields("bastion", port)),
                Err("SSH port must be a number from 1 to 65535.".into())
            );
        }
        assert_eq!(
            ssh_tunnel(
                true,
                [
                    "bastion".into(),
                    String::new(),
                    String::new(),
                    "id_ed25519".into()
                ]
            ),
            Err("Identity file must be an absolute path to the key file.".into())
        );
    }

    #[test]
    fn an_edited_tunnel_reads_back_as_it_was_saved() {
        let saved = SshTunnel {
            host: "bastion.example".into(),
            port: Some(2222),
            user: "deploy".into(),
            identity_file: Some("~/.ssh/id_ed25519".into()),
        };
        assert_eq!(ssh_tunnel(true, ssh_fields(Some(&saved))), Ok(Some(saved)));
    }

    #[test]
    fn the_summary_says_where_the_form_points_once_it_points_somewhere() {
        let at = |engine, srv, typed: &Typed| summary(engine, srv, typed);
        let server = Typed {
            host: "db.internal",
            port: "5432",
            database: "orders",
            user: "ada",
            ..Typed::default()
        };
        assert_eq!(
            at(Engine::Postgres, false, &server).as_deref(),
            Some("ada@db.internal:5432/orders")
        );
        assert_eq!(
            at(
                Engine::Postgres,
                false,
                &Typed {
                    tunnel: Some("bastion.internal"),
                    ..server
                }
            )
            .as_deref(),
            Some("ada@db.internal:5432/orders via bastion.internal")
        );
        // Switched on before its host is typed, the tunnel names nothing yet.
        assert_eq!(
            at(
                Engine::Postgres,
                false,
                &Typed {
                    tunnel: Some(""),
                    ..server
                }
            )
            .as_deref(),
            Some("ada@db.internal:5432/orders")
        );
        // Only what was typed: a blank port is the default, not "".
        assert_eq!(
            at(
                Engine::MySql,
                false,
                &Typed {
                    host: "localhost",
                    ..Typed::default()
                }
            )
            .as_deref(),
            Some("localhost")
        );
        // The port an SRV name hides is not where it points.
        assert_eq!(
            at(
                Engine::MongoDb,
                true,
                &Typed {
                    host: "cluster0.example.net",
                    port: "27017",
                    ..Typed::default()
                }
            )
            .as_deref(),
            Some("cluster0.example.net")
        );
        assert_eq!(
            at(
                Engine::Sqlite,
                false,
                &Typed {
                    path: "~/Projects/app/data.sqlite",
                    ..server
                }
            )
            .as_deref(),
            Some("~/Projects/app/data.sqlite")
        );
        assert_eq!(
            at(
                Engine::Snowflake,
                false,
                &Typed {
                    account: "acme-prod",
                    user: "ada",
                    database: "ANALYTICS",
                    ..Typed::default()
                }
            )
            .as_deref(),
            Some("ada@acme-prod/ANALYTICS")
        );
        for engine in Engine::ALL {
            assert_eq!(at(engine, false, &Typed::default()), None, "{engine:?}");
        }
        // A user on its own points nowhere.
        assert_eq!(
            at(
                Engine::Postgres,
                false,
                &Typed {
                    user: "ada",
                    ..Typed::default()
                }
            ),
            None
        );
    }

    #[test]
    fn a_new_connection_starts_read_only_and_a_copy_keeps_its_mode() {
        assert_eq!(starting_mode(None), Mode::ReadOnly);
        for mode in Mode::ALL {
            assert_eq!(starting_mode(Some(mode)), mode);
        }
    }

    #[test]
    fn a_blank_field_says_what_blank_means_for_the_engine() {
        let postgres = placeholders(Engine::Postgres);
        assert_eq!(postgres.port, "5432");
        assert_eq!(postgres.database, "postgres");
        // `server` asks for a user here, and `server_requiring(&[])` there.
        assert_eq!(postgres.user, "Required");
        assert_eq!(placeholders(Engine::MongoDb).user, "Optional");
        assert_eq!(placeholders(Engine::Redis).name, "Named after the host");
        assert_eq!(placeholders(Engine::Turso).name, "Named after the file");
        assert_eq!(
            placeholders(Engine::DuckDb).path,
            "Blank for a database in memory"
        );
        assert_eq!(placeholders(Engine::Snowflake).port, "");
    }

    #[test]
    fn a_duplicate_name_does_not_collide_with_one_already_there() {
        assert_eq!(duplicate_profile_name("Prod", &[]), "Prod copy");
        assert_eq!(
            duplicate_profile_name("Prod", &["Prod copy".to_string()]),
            "Prod copy 2"
        );
        assert_eq!(
            duplicate_profile_name(
                "Prod",
                &["Prod copy".to_string(), "Prod copy 2".to_string()]
            ),
            "Prod copy 3"
        );
        // Unrelated names are not what it is dodging.
        assert_eq!(
            duplicate_profile_name("Prod", &["Staging".to_string()]),
            "Prod copy"
        );
    }

    #[test]
    fn the_environment_password_is_never_copied_into_the_keychain() {
        let server = |password: &str| ServerConfig {
            host: "db.example".to_string(),
            port: None,
            database: "app".to_string(),
            user: "dbdelve".to_string(),
            password: password.to_string(),
            sslmode: SslMode::default(),
            root_certificate: None,
            statement_timeout: 0,
            ssh: None,
            schema: None,
        };
        let typed = ConnectionConfig::Postgres(server("hunter2"));
        assert_eq!(password_to_persist(&typed, Origin::Form), Some("hunter2"));
        // `PGPASSWORD` belongs to the shell that set it.
        assert_eq!(password_to_persist(&typed, Origin::Environment), None);
        // Blank is a valid password; an empty keychain item is not how one is
        // recorded.
        assert_eq!(
            password_to_persist(&ConnectionConfig::MySql(server("")), Origin::Form),
            None
        );
        assert_eq!(
            password_to_persist(
                &ConnectionConfig::Sqlite {
                    path: "/tmp/dbdelve.db".to_string(),
                    statement_timeout: 0
                },
                Origin::Form
            ),
            None
        );
        // An account signs in with a key file the profile only points at, so
        // there is no secret for the Keychain to hold.
        let account = ConnectionConfig::Snowflake(crate::db::SnowflakeConfig {
            database: "ANALYTICS".to_string(),
            private_key: "/Users/dev/.ssh/snowflake.p8".to_string(),
            ..Default::default()
        });
        assert_eq!(password_to_persist(&account, Origin::Form), None);
        assert_eq!(default_profile_name(&account), "ANALYTICS");
    }

    #[test]
    fn a_server_left_to_pick_the_database_is_named_after_its_host() {
        let mut server = ServerConfig {
            host: "db.example".to_string(),
            database: "app".to_string(),
            ..Default::default()
        };
        assert_eq!(
            default_profile_name(&ConnectionConfig::Postgres(server.clone())),
            "app"
        );
        server.database.clear();
        assert_eq!(
            default_profile_name(&ConnectionConfig::SqlServer(server)),
            "db.example"
        );
    }
}

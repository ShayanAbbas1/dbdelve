//! The connection form: the fields a profile is created and edited through.
//!
//! The form holds inputs rather than a config, because a half-typed connection
//! is not one yet -- `config` is where the fields become something that can be
//! opened.
//!
//! This was a plain type at the crate root. It moved out whole; nothing changed
//! but its visibility.

use gpui::{App, AppContext, Context, Entity, Window};
use gpui_component::input::InputState;

use crate::{
    Workspace,
    db::{ConnectionConfig, Engine, ServerConfig, SnowflakeConfig, SslMode},
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
    /// What an account has that a server does not. `host`, `database` and
    /// `user` are shared with the server fields: they mean the same thing, and
    /// sharing them is what keeps a value typed under one chip there under the
    /// next.
    pub(crate) account: Entity<InputState>,
    pub(crate) private_key: Entity<InputState>,
    pub(crate) warehouse: Entity<InputState>,
    pub(crate) role: Entity<InputState>,
    /// Seconds, and blank is the same as 0: no limit. Every engine has one, so
    /// unlike the credential fields it is drawn whichever chip is selected.
    pub(crate) statement_timeout: Entity<InputState>,
    /// An input to focus once it has been mounted.
    ///
    /// A chip can unmount the field the user was typing in, and a window with
    /// nothing focused has no dispatch path — every keybinding in the app goes
    /// dead until something is clicked. So whichever chip takes a field away
    /// names the one that replaces it, and `Workspace::render` hands focus over
    /// on the next frame, once it exists to receive it.
    pub(crate) needs_focus: Option<Entity<InputState>>,
    pub(crate) error: Option<String>,
    /// The id of the profile being edited, or `None` for a new connection.
    pub(crate) editing: Option<String>,
}

impl ConnectionForm {
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
            Some(ConnectionConfig::Sqlite { path, .. }) => Some(path.as_str()),
            _ => None,
        };

        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("postgresql://…  or  sqlite://…"));
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Display name")
                .default_value(value(
                    server
                        .map(|server| server.database.as_str())
                        .or_else(|| account.map(|account| account.database.as_str()))
                        .or_else(|| file.map(file_stem)),
                ))
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Database file")
                .default_value(value(file))
        });
        let host = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Host")
                .default_value(value(
                    server
                        .map(|server| server.host.as_str())
                        .or_else(|| account.and_then(|account| account.host.as_deref())),
                ))
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Port (optional)")
                .default_value(
                    server
                        .and_then(|server| server.port)
                        .map(|port| port.to_string())
                        .unwrap_or_default(),
                )
        });
        let database = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Database")
                .default_value(value(
                    server
                        .map(|server| server.database.as_str())
                        .or_else(|| account.map(|account| account.database.as_str())),
                ))
        });
        let user = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Username")
                .default_value(value(
                    server
                        .map(|server| server.user.as_str())
                        .or_else(|| account.map(|account| account.user.as_str())),
                ))
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password (optional)")
                .default_value(value(server.map(|server| server.password.as_str())))
                .masked(true)
        });

        let root_certificate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Root certificate file (optional)")
                .default_value(value(
                    server.and_then(|server| server.root_certificate.as_deref()),
                ))
        });
        let account_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Account identifier")
                .default_value(value(account.map(|account| account.account.as_str())))
        });
        let private_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Absolute path to the private key file")
                .default_value(value(account.map(|account| account.private_key.as_str())))
        });
        let warehouse = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Warehouse (optional)")
                .default_value(value(
                    account.and_then(|account| account.warehouse.as_deref()),
                ))
        });
        let role = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Role (optional)")
                .default_value(value(account.and_then(|account| account.role.as_deref())))
        });
        let statement_timeout = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Seconds (0 for no limit)")
                .default_value(
                    config
                        .map(ConnectionConfig::statement_timeout)
                        .filter(|seconds| *seconds > 0)
                        .map(|seconds| seconds.to_string())
                        .unwrap_or_default(),
                )
        });

        Self {
            // The form is the whole window on a first launch, and a window
            // with nothing focused has no dispatch path -- every binding is
            // dead until a field is clicked. So the field the user is meant to
            // start in asks for focus the moment it is mounted.
            needs_focus: Some(url.clone()),
            url,
            engine: config.map(ConnectionConfig::engine).unwrap_or_default(),
            name,
            color: None,
            mode: Mode::default(),
            path,
            host,
            port,
            database,
            user,
            password,
            sslmode: server.map(|server| server.sslmode).unwrap_or_default(),
            root_certificate,
            account: account_name,
            private_key,
            warehouse,
            role,
            statement_timeout,
            error: None,
            editing: None,
        }
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
        // No `form.mode` here: the chip row draws only on a fresh form and
        // `save_profile` has no mode to take, so copying it in was a write
        // nothing ever read. The titlebar picker is where an existing
        // connection's mode changes.
        form.name.update(cx, |name, cx| {
            name.set_value(profile.name.clone(), window, cx);
        });
        form.password
            .update(cx, |password, cx| password.set_value("", window, cx));
        form
    }

    pub(crate) fn config(&self, cx: &App) -> Result<(String, ConnectionConfig), String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = read(&self.name);
        if name.is_empty() {
            return Err("Display name is required.".into());
        }

        let statement_timeout = self.statement_timeout(cx)?;
        let config = match self.engine {
            Engine::Sqlite => {
                let path = read(&self.path);
                if path.is_empty() {
                    return Err("Database file is required.".into());
                }
                ConnectionConfig::Sqlite {
                    path,
                    statement_timeout,
                }
            }
            Engine::Postgres => ConnectionConfig::Postgres(self.server(cx)?),
            Engine::MySql => ConnectionConfig::MySql(self.server(cx)?),
            Engine::Snowflake => ConnectionConfig::Snowflake(self.account(cx)?),
        };

        Ok((name, config))
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
            host: optional(&self.host),
            user,
            private_key,
            database,
            warehouse: optional(&self.warehouse),
            role: optional(&self.role),
            statement_timeout: self.statement_timeout(cx)?,
        })
    }

    pub(crate) fn server(&self, cx: &App) -> Result<ServerConfig, String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let host = read(&self.host);
        let database = read(&self.database);
        let user = read(&self.user);
        let port = read(&self.port);

        for (label, value) in [
            ("Host", &host),
            ("Database", &database),
            ("Username", &user),
        ] {
            if value.is_empty() {
                return Err(format!("{label} is required."));
            }
        }

        let port = if port.is_empty() {
            None
        } else {
            Some(
                port.parse()
                    .map_err(|_| "Port must be a number from 1 to 65535.".to_string())?,
            )
        };

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
        })
    }
}

/// What a profile is called when nobody has named it: the database for an
/// engine that has one, and the file for an engine that is one.
pub(crate) fn default_profile_name(config: &ConnectionConfig) -> String {
    match config {
        ConnectionConfig::Postgres(server) | ConnectionConfig::MySql(server) => {
            server.database.clone()
        }
        ConnectionConfig::Sqlite { path, .. } => file_stem(path).to_string(),
        ConnectionConfig::Snowflake(account) => account.database.clone(),
    }
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
}

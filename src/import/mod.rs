//! Bringing connections in from another database client.
//!
//! Each source reads its own files into a [`Report`] and never touches the
//! workspace: what is already here, what the names collide with, and where the
//! passwords go are decided by `Workspace::add_imported`, once, for
//! every source.

mod dbeaver;
mod tableplus;

use serde::Deserialize;

use crate::{
    db::{ConnectionConfig, Engine, SslMode},
    theme::ConnectionColor,
};

/// A connection this build can open, with whatever it could not carry over.
#[derive(Debug, PartialEq)]
pub(crate) struct Imported {
    pub(crate) name: String,
    pub(crate) config: ConnectionConfig,
    pub(crate) color: Option<ConnectionColor>,
    /// Settings left behind, said the way the summary line says them: "SSH
    /// tunnel left off: it logs in with a password".
    pub(crate) notes: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Skipped {
    pub(crate) name: String,
    pub(crate) reason: String,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Report {
    pub(crate) imported: Vec<Imported>,
    pub(crate) skipped: Vec<Skipped>,
    /// What went wrong with the source as a whole rather than one connection,
    /// such as a credentials file that would not decrypt.
    pub(crate) notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub(crate) enum Source {
    DBeaver,
    TablePlus,
}

impl Source {
    pub(crate) const ALL: [Self; 2] = [Self::DBeaver, Self::TablePlus];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::DBeaver => "DBeaver",
            Self::TablePlus => "TablePlus",
        }
    }

    /// A directory check and nothing more, so the form can ask on open.
    pub(crate) fn found(self) -> bool {
        match self {
            Self::DBeaver => dbeaver::workspaces().iter().any(|root| root.is_dir()),
            Self::TablePlus => tableplus::files().iter().any(|file| file.is_file()),
        }
    }

    /// Blocking file reads; run it on the background executor.
    pub(crate) fn read(self) -> Result<Report, String> {
        match self {
            Self::DBeaver => dbeaver::read(),
            Self::TablePlus => tableplus::read(),
        }
    }
}

/// Blank is the default port. Anything else that isn't one is left blank too,
/// and said.
pub(super) fn port(label: &str, value: Option<String>, notes: &mut Vec<String>) -> Option<u16> {
    let value = value?;
    match value.parse() {
        Ok(0) | Err(_) => {
            notes.push(format!(
                "{label} {value} isn't a port number, so it was left blank"
            ));
            None
        }
        Ok(port) => Some(port),
    }
}

/// Whether `candidate` points where an existing profile already does, so that
/// running an import twice adds nothing the second time. A blank port is the
/// default one, since DBeaver writes 5432 where the form leaves it blank.
pub(crate) fn already_have<'a>(
    existing: impl IntoIterator<Item = &'a ConnectionConfig>,
    candidate: &ConnectionConfig,
) -> bool {
    let engine = candidate.engine();
    // A MariaDB connection imported before MariaDB had its own engine was
    // stored as MySQL, and re-importing it must not add a second copy.
    let family = |engine: Engine| match engine {
        Engine::MariaDb => Engine::MySql,
        other => other,
    };
    existing.into_iter().any(|config| {
        family(config.engine()) == family(engine)
            && match (config.server(), candidate.server()) {
                (Some(a), Some(b)) => {
                    a.host.eq_ignore_ascii_case(&b.host)
                        && a.port.or(engine.default_port()) == b.port.or(engine.default_port())
                        && (&a.database, &a.user) == (&b.database, &b.user)
                }
                _ => config.endpoint() == candidate.endpoint(),
            }
    })
}

/// Kept only where the mode consults it, as the form does, and said where it
/// isn't: libpq reads a CA file under `require` as asking for verify-ca.
pub(super) fn root_certificate(
    sslmode: SslMode,
    path: Option<String>,
    notes: &mut Vec<String>,
) -> Option<String> {
    let path = path?;
    if sslmode.checks_certificate() {
        return Some(path);
    }
    notes.push(format!(
        "CA certificate left off: SSL mode {} doesn't check one",
        sslmode.as_str()
    ));
    None
}

impl Report {
    /// One line, because the status bar shows one.
    pub(crate) fn summary(&self, source: Source) -> String {
        let label = source.label();
        let mut parts = Vec::new();
        match self.imported.len() {
            0 if self.skipped.is_empty() => parts.push(format!("No {label} connections found.")),
            0 => parts.push(format!("Imported nothing from {label}.")),
            1 => parts.push(format!("Imported 1 connection from {label}, read-only.")),
            count => parts.push(format!(
                "Imported {count} connections from {label}, all read-only."
            )),
        }
        let dropped = self
            .imported
            .iter()
            .filter(|imported| !imported.notes.is_empty())
            .map(|imported| format!("{} ({})", imported.name, imported.notes.join("; ")))
            .collect::<Vec<_>>();
        if !dropped.is_empty() {
            parts.push(format!(
                "{} had settings dropped: {}.",
                dropped.len(),
                dropped.join(", ")
            ));
        }
        if !self.skipped.is_empty() {
            parts.push(format!(
                "Skipped {}: {}.",
                self.skipped.len(),
                self.skipped
                    .iter()
                    .map(|skipped| format!("{} ({})", skipped.name, skipped.reason))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.extend(self.notes.iter().cloned());
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ServerConfig;

    fn postgres(host: &str, port: Option<u16>, database: &str, user: &str) -> ConnectionConfig {
        ConnectionConfig::Postgres(ServerConfig {
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            ..ServerConfig::default()
        })
    }

    #[test]
    fn already_have_treats_mysql_and_mariadb_as_one_family() {
        let server = || ServerConfig {
            host: "h".into(),
            user: "u".into(),
            ..ServerConfig::default()
        };
        assert!(already_have(
            &[ConnectionConfig::MySql(server())],
            &ConnectionConfig::MariaDb(server())
        ));
        assert!(!already_have(
            &[ConnectionConfig::Postgres(server())],
            &ConnectionConfig::MariaDb(server())
        ));
    }

    #[test]
    fn already_have_matches_engine_host_port_database_and_user() {
        let existing = [
            postgres("db.example.com", Some(5432), "app", "alice"),
            ConnectionConfig::Sqlite {
                path: "/data/app.db".into(),
                statement_timeout: 0,
            },
        ];
        let mut different_password = postgres("db.example.com", Some(5432), "app", "alice");
        if let Some(server) = different_password.server_mut() {
            server.password = "secret".into();
        }
        assert!(already_have(&existing, &different_password));
        assert!(already_have(
            &existing,
            &postgres("DB.example.com", None, "app", "alice")
        ));
        assert!(already_have(
            &[postgres("db.example.com", None, "app", "alice")],
            &postgres("db.example.com", Some(5432), "app", "alice")
        ));
        assert!(!already_have(
            &[postgres("db.example.com", None, "app", "alice")],
            &postgres("db.example.com", Some(5433), "app", "alice")
        ));
        assert!(already_have(
            &existing,
            &ConnectionConfig::Sqlite {
                path: "/data/app.db".into(),
                statement_timeout: 30,
            }
        ));

        assert!(!already_have(
            &existing,
            &postgres("db.example.com", Some(5433), "app", "alice")
        ));
        assert!(!already_have(
            &existing,
            &postgres("db.example.com", Some(5432), "app", "bob")
        ));
        assert!(!already_have(
            &existing,
            &postgres("db.example.com", Some(5432), "other", "alice")
        ));
        let ConnectionConfig::Postgres(server) =
            postgres("db.example.com", Some(5432), "app", "alice")
        else {
            unreachable!()
        };
        assert!(!already_have(&existing, &ConnectionConfig::MySql(server)));
        assert!(!already_have(
            &existing,
            &ConnectionConfig::Sqlite {
                path: "/data/other.db".into(),
                statement_timeout: 0,
            }
        ));
    }

    #[test]
    fn the_summary_is_one_line_naming_what_was_dropped_and_skipped() {
        let imported = |name: &str, notes: &[&str]| Imported {
            name: name.into(),
            config: postgres("h", None, "d", "u"),
            color: None,
            notes: notes.iter().map(|note| note.to_string()).collect(),
        };
        let report = Report {
            imported: vec![
                imported("one", &[]),
                imported("two", &["SSH tunnel left off: it logs in with a password"]),
            ],
            skipped: vec![Skipped {
                name: "Oracle prod".into(),
                reason: "Oracle isn't supported".into(),
            }],
            notes: vec!["DBeaver's saved credentials could not be read.".into()],
        };
        assert_eq!(
            report.summary(Source::DBeaver),
            "Imported 2 connections from DBeaver, all read-only. \
             1 had settings dropped: two (SSH tunnel left off: it logs in with a password). \
             Skipped 1: Oracle prod (Oracle isn't supported). \
             DBeaver's saved credentials could not be read."
        );

        assert_eq!(
            Report::default().summary(Source::DBeaver),
            "No DBeaver connections found."
        );
        assert_eq!(
            Report {
                imported: vec![imported("one", &[])],
                ..Report::default()
            }
            .summary(Source::DBeaver),
            "Imported 1 connection from DBeaver, read-only."
        );
    }
}

//! libpq's connection service file, which tokio-postgres does not read:
//! `PGSERVICEFILE`, else `~/.pg_service.conf`
//! (`%APPDATA%\postgresql\.pg_service.conf` on Windows), then
//! `$PGSYSCONFDIR/pg_service.conf`. Read each time a profile connects rather
//! than once, so the file stays the service's definition and a password in it
//! never reaches the profile or the Keychain. That connection's side sessions
//! keep what the read found.
//!
//! ponytail: libpq's last file sits in a directory compiled into libpq, which
//! dbdelve has no equivalent of, so without `PGSYSCONFDIR` there is no system
//! file. libpq's last resorts for a host and a user, its socket directory and
//! the OS user, are not taken either: the connection says which is missing.
//! A database nothing names stays dbdelve's `postgres`, where libpq would take
//! the user's name.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::PostgresConfig;
use crate::db::ServerConfig;
use crate::tls::SslMode;

/// The server with what it leaves blank filled from its service, then from the
/// `PG*` variables, libpq's order. One without a service comes back as it went
/// in: those variables are only read for one at launch.
pub(super) fn resolved(config: &PostgresConfig) -> Result<ServerConfig, String> {
    resolved_from(config, &|name| std::env::var_os(name))
}

/// [`resolved`] with its variables handed in, since the tests cannot set them
/// without racing every other thread that reads the environment.
fn resolved_from(
    config: &PostgresConfig,
    variable: &dyn Fn(&str) -> Option<OsString>,
) -> Result<ServerConfig, String> {
    let Some(name) = config.service.as_deref() else {
        return Ok(config.server.clone());
    };
    let (file, mut settings) = first_defining(files(variable)?, name)?;
    settings.extend(environment(variable));
    let filled =
        filled(&config.server, &settings).map_err(|error| format!("Service {name}: {error}"))?;
    for (key, value) in [("host", &filled.host), ("user", &filled.user)] {
        if value.is_empty() {
            return Err(format!(
                "Service {name} in {} does not set {key}.",
                file.display()
            ));
        }
    }
    Ok(filled)
}

/// The first file defining `name` and its settings, in libpq's order: a
/// system-wide definition is only reached when the user's file has none.
fn first_defining(
    files: Vec<PathBuf>,
    name: &str,
) -> Result<(PathBuf, Vec<(String, String)>), String> {
    for file in files {
        let contents = match std::fs::read(&file) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Could not read {}: {error}", file.display())),
        };
        // libpq reads bytes, so a stray byte in another service's comment must
        // not cost this one its definition.
        let contents = String::from_utf8_lossy(&contents);
        if let Some(settings) =
            definition(&contents, name).map_err(|error| format!("{}: {error}", file.display()))?
        {
            return Ok((file, settings));
        }
    }
    Err(format!("Definition of service {name} was not found."))
}

/// The variables with a setting a service can name, spelled as a service would.
fn environment(variable: &dyn Fn(&str) -> Option<OsString>) -> Vec<(String, String)> {
    [
        ("PGHOST", "host"),
        ("PGPORT", "port"),
        ("PGDATABASE", "dbname"),
        ("PGUSER", "user"),
        ("PGPASSWORD", "password"),
        ("PGSSLMODE", "sslmode"),
        ("PGSSLROOTCERT", "sslrootcert"),
    ]
    .into_iter()
    .filter_map(|(name, key)| {
        let value = variable(name)?.into_string().ok()?;
        (!value.is_empty()).then(|| (key.to_string(), value))
    })
    .collect()
}

fn files(variable: &dyn Fn(&str) -> Option<OsString>) -> Result<Vec<PathBuf>, String> {
    let variable = |name: &str| variable(name).filter(|value| !value.is_empty());
    let mut files = Vec::new();
    match variable("PGSERVICEFILE") {
        Some(file) => {
            // Named outright, so its absence is a mistake to report, where
            // the default's is only a user who keeps no such file.
            let file = PathBuf::from(file);
            if !file.exists() {
                return Err(format!("Service file {} was not found.", file.display()));
            }
            files.push(file);
        }
        None => files.extend(user_file(&variable)),
    }
    files.extend(
        variable("PGSYSCONFDIR").map(|directory| Path::new(&directory).join("pg_service.conf")),
    );
    Ok(files)
}

fn user_file(variable: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let _ = variable;
        crate::store::data_root()
            .ok()
            .map(|root| root.join("postgresql").join(".pg_service.conf"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        variable("HOME").map(|home| Path::new(&home).join(".pg_service.conf"))
    }
}

/// The `key=value` lines under `[name]`, or `None` when no section has that
/// name. Only that section is read closely, as in libpq, so a line elsewhere
/// that dbdelve could not use costs nothing.
fn definition(contents: &str, name: &str) -> Result<Option<Vec<(String, String)>>, String> {
    let mut settings = None;
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            if settings.is_some() {
                break;
            }
            // libpq stops at the `]`, so a comment after it is no different name.
            if section
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(']'))
            {
                settings = Some(Vec::new());
            }
            continue;
        }
        if let Some(settings) = &mut settings {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("line {} has no =.", index + 1))?;
            settings.push((key.to_string(), value.to_string()));
        }
    }
    Ok(settings)
}

fn filled(server: &ServerConfig, settings: &[(String, String)]) -> Result<ServerConfig, String> {
    let mut filled = server.clone();
    for (index, (key, value)) in settings.iter().enumerate() {
        // The first says, as in libpq, so a variable never beats a service.
        if settings[..index].iter().any(|(earlier, _)| earlier == key) {
            continue;
        }
        match key.as_str() {
            "host" => fill(&mut filled.host, value),
            "user" => fill(&mut filled.user, value),
            "password" => fill(&mut filled.password, value),
            "dbname" => fill(&mut filled.database, value),
            "port" => {
                if filled.port.is_none() {
                    let port = value
                        .parse()
                        .map_err(|_| format!("port {value} is not a valid port."))?;
                    filled.port = Some(port);
                }
            }
            // A profile always holds a mode, so the default stands for not
            // having said, and the service decides, `disable` included. A URL
            // cannot state `prefer` beside a service: see `config_from_url`.
            "sslmode" => {
                if filled.sslmode == SslMode::default() {
                    filled.sslmode = SslMode::parse(value)?;
                }
            }
            "sslrootcert" => {
                filled.root_certificate.get_or_insert_with(|| value.clone());
            }
            "service" => return Err("a service cannot name another service.".to_string()),
            // Refused rather than dropped, like a URL parameter: a connection
            // missing a setting its service asked for is not that connection.
            other => return Err(format!("{other} is not a setting dbdelve can use.")),
        }
    }
    // libpq checks a named root certificate on any encrypted connection, so a
    // pin must not end up encrypted to anybody who answers. `prefer` cannot
    // become `verify-ca` without losing its plaintext fallback, so it is refused.
    if filled.root_certificate.is_some() {
        match filled.sslmode {
            SslMode::Require => filled.sslmode = SslMode::VerifyCa,
            SslMode::Prefer => {
                return Err(
                    "a root certificate needs an sslmode of require or stricter.".to_string(),
                );
            }
            _ => {}
        }
    }
    Ok(filled)
}

fn fill(field: &mut String, value: &str) {
    if field.is_empty() {
        value.clone_into(field);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OsString, PostgresConfig, ServerConfig, SslMode, definition, filled, first_defining,
        resolved_from,
    };
    use std::path::PathBuf;

    const FILE: &str = "\
# rotated nightly
[staging]
host=db.staging.test
dbname=staging

[harness]
host=db.harness.test
port=6432
dbname=orders
user=svc_orders
sslmode=verify-full

[harness-replica]
host=replica.harness.test
";

    /// A file of `contents` this test alone uses, and the service it names.
    fn service_file(test: &str, contents: &str) -> (PathBuf, PostgresConfig) {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-resolved-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("pg_service.conf");
        std::fs::write(&file, contents).unwrap();
        let config = PostgresConfig {
            service: Some("harness".into()),
            ..PostgresConfig::default()
        };
        (file, config)
    }

    fn settings(contents: &str, name: &str) -> Vec<(String, String)> {
        definition(contents, name).unwrap().unwrap()
    }

    #[test]
    fn a_service_is_only_the_lines_of_its_own_section() {
        let found = settings(FILE, "harness");
        assert_eq!(found.len(), 5);
        assert_eq!(found[0], ("host".into(), "db.harness.test".into()));
    }

    #[test]
    fn a_section_is_matched_whole() {
        assert_eq!(definition(FILE, "harn").unwrap(), None);
        assert_eq!(definition(FILE, "absent").unwrap(), None);
    }

    #[test]
    fn a_comment_after_the_section_name_is_still_that_section() {
        let contents = "[harness] # the replica\nhost=h\n";
        assert_eq!(settings(contents, "harness").len(), 1);
    }

    #[test]
    fn the_first_file_defining_a_service_wins_and_a_later_one_is_only_a_fallback() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-pgservice-order-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let (user, system) = (directory.join("user"), directory.join("system"));
        std::fs::write(&user, "[harness]\nhost=user\n").unwrap();
        std::fs::write(&system, "[harness]\nhost=system\n[other]\nhost=system\n").unwrap();
        let host = |name: &str| {
            let files = vec![directory.join("absent"), user.clone(), system.clone()];
            first_defining(files, name).map(|(_, settings)| settings[0].1.clone())
        };
        let (harness, other, absent) = (host("harness"), host("other"), host("absent"));
        std::fs::remove_dir_all(&directory).unwrap();

        assert_eq!(harness.as_deref(), Ok("user"));
        assert_eq!(other.as_deref(), Ok("system"));
        assert!(absent.unwrap_err().contains("absent"));
    }

    #[test]
    fn a_line_with_no_equals_sign_is_an_error_only_in_the_section_asked_for() {
        let contents = "[other]\nnonsense\n[harness]\nhost=h\n";
        assert_eq!(settings(contents, "harness").len(), 1);
        assert!(
            definition(contents, "other")
                .unwrap_err()
                .contains("line 2")
        );
    }

    #[test]
    fn a_value_keeps_its_own_equals_signs() {
        assert_eq!(settings("[harness]\npassword=a=b\n", "harness")[0].1, "a=b");
    }

    #[test]
    fn the_variables_fill_what_the_service_leaves_unset() {
        let (file, config) = service_file(
            "variables",
            "[harness]\nhost=from.service\nuser=svc\nsslmode=require\n",
        );
        let server = resolved_from(&config, &|name| match name {
            "PGSERVICEFILE" => Some(file.clone().into()),
            "PGHOST" => Some("from.variable".into()),
            "PGPORT" => Some("6543".into()),
            "PGDATABASE" => Some("orders".into()),
            "PGPASSWORD" => Some("hunter2".into()),
            "PGSSLMODE" => Some("disable".into()),
            "PGSSLROOTCERT" => Some("/ca.pem".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(server.host, "from.service");
        assert_eq!(server.port, Some(6543));
        assert_eq!(server.database, "orders");
        assert_eq!(server.user, "svc");
        assert_eq!(server.password, "hunter2");
        assert_eq!(server.root_certificate.as_deref(), Some("/ca.pem"));
        assert_eq!(server.sslmode, SslMode::VerifyCa, "the service's `require`");
    }

    #[test]
    fn a_service_leaving_a_host_or_user_to_nobody_says_which() {
        for (test, contents, missing) in [
            ("host", "[harness]\nuser=svc\n", "host"),
            ("user", "[harness]\nhost=h\n", "user"),
        ] {
            let (file, config) = service_file(test, contents);
            let failure = resolved_from(&config, &|name| {
                (name == "PGSERVICEFILE").then(|| file.clone().into())
            })
            .unwrap_err();
            assert!(
                failure.contains(&format!("does not set {missing}")),
                "{failure}"
            );
        }
    }

    #[test]
    fn a_service_file_named_outright_has_to_exist() {
        let (file, config) = service_file("named", "");
        let absent = file.with_extension("absent");
        let failure = resolved_from(&config, &|name| {
            (name == "PGSERVICEFILE").then(|| absent.clone().into())
        })
        .unwrap_err();
        assert!(failure.contains("was not found"), "{failure}");
    }

    #[test]
    fn the_system_file_answers_what_the_users_does_not_define() {
        let (user, config) = service_file("system-user", "[other]\nhost=nope\n");
        let (system, _) = service_file("system", "[harness]\nhost=h\nuser=u\n");
        let directory: OsString = system.parent().unwrap().into();
        let server = resolved_from(&config, &|name| match name {
            "PGSERVICEFILE" => Some(user.clone().into()),
            "PGSYSCONFDIR" => Some(directory.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(server.host, "h");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn with_no_file_named_the_one_in_the_home_directory_is_read() {
        let (file, config) = service_file("home", "[harness]\nhost=h\nuser=u\n");
        let home = file.parent().unwrap();
        std::fs::rename(&file, home.join(".pg_service.conf")).unwrap();
        let server = resolved_from(&config, &|name| {
            (name == "HOME").then(|| home.as_os_str().to_owned())
        })
        .unwrap();
        assert_eq!(server.host, "h");
    }

    #[test]
    fn a_stray_byte_outside_the_section_costs_the_service_nothing() {
        let (file, _) = service_file("bytes", "");
        std::fs::write(&file, b"# caf\xe9\n[harness]\nhost=h\n").unwrap();
        let (_, found) = first_defining(vec![file], "harness").unwrap();
        assert_eq!(found, [("host".to_string(), "h".to_string())]);
    }

    #[test]
    fn a_profiles_root_certificate_beats_its_services() {
        let stated = ServerConfig {
            root_certificate: Some("/mine.pem".into()),
            sslmode: SslMode::VerifyFull,
            ..ServerConfig::default()
        };
        let service = settings("[harness]\nsslrootcert=/theirs.pem\n", "harness");
        let server = filled(&stated, &service).unwrap();
        assert_eq!(server.root_certificate.as_deref(), Some("/mine.pem"));
    }

    #[test]
    fn a_service_fills_what_the_profile_leaves_blank() {
        let server = filled(&ServerConfig::default(), &settings(FILE, "harness")).unwrap();
        assert_eq!(server.host, "db.harness.test");
        assert_eq!(server.port, Some(6432));
        assert_eq!(server.database, "orders");
        assert_eq!(server.user, "svc_orders");
        assert_eq!(server.sslmode, SslMode::VerifyFull);
    }

    #[test]
    fn what_the_profile_says_wins() {
        let stated = ServerConfig {
            host: "elsewhere.test".into(),
            port: Some(5433),
            database: "billing".into(),
            user: "ada".into(),
            sslmode: SslMode::Disable,
            ..ServerConfig::default()
        };
        let server = filled(&stated, &settings(FILE, "harness")).unwrap();
        assert_eq!(server, stated);
    }

    /// The default stands for "not said", so a URL with no `sslmode` cannot
    /// quietly undo the verification its service demands.
    #[test]
    fn a_profile_that_states_no_mode_takes_the_services() {
        for mode in ["disable", "require", "verify-ca", "verify-full"] {
            let contents = format!("[harness]\nsslmode={mode}\n");
            let server = filled(&ServerConfig::default(), &settings(&contents, "harness")).unwrap();
            assert_eq!(server.sslmode, SslMode::parse(mode).unwrap());
        }
    }

    #[test]
    fn a_service_ranks_above_the_variables_and_they_fill_what_it_leaves() {
        let mut settings = settings("[harness]\nhost=from-service\nsslmode=prefer\n", "harness");
        settings.extend(
            [
                ("host", "from-variable"),
                ("user", "from-variable"),
                ("sslmode", "disable"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        let server = filled(&ServerConfig::default(), &settings).unwrap();
        assert_eq!(server.host, "from-service");
        assert_eq!(server.user, "from-variable");
        assert_eq!(server.sslmode, SslMode::Prefer);
    }

    #[test]
    fn a_pinned_root_certificate_is_checked_on_require_as_in_libpq() {
        let contents = "[harness]\nsslmode=require\nsslrootcert=/ca.pem\n";
        let server = filled(&ServerConfig::default(), &settings(contents, "harness")).unwrap();
        assert_eq!(server.sslmode, SslMode::VerifyCa);
    }

    #[test]
    fn a_pinned_root_certificate_is_refused_where_it_would_go_unchecked() {
        let failure = filled(
            &ServerConfig::default(),
            &settings("[harness]\nsslrootcert=/ca.pem\n", "harness"),
        );
        assert!(failure.unwrap_err().contains("root certificate"));
    }

    #[test]
    fn a_setting_dbdelve_cannot_carry_is_refused_by_name() {
        let failure = filled(
            &ServerConfig::default(),
            &settings("[harness]\nsslcert=/c\n", "harness"),
        );
        assert!(failure.unwrap_err().contains("sslcert"));
    }

    #[test]
    fn a_service_cannot_name_another() {
        let failure = filled(
            &ServerConfig::default(),
            &settings("[harness]\nservice=b\n", "harness"),
        );
        assert!(failure.unwrap_err().contains("another service"));
    }

    #[test]
    fn a_bad_port_and_a_refused_mode_say_so() {
        let failure = filled(
            &ServerConfig::default(),
            &settings("[harness]\nport=x\n", "harness"),
        );
        assert!(failure.unwrap_err().contains("port x"));
        let failure = filled(
            &ServerConfig::default(),
            &settings("[harness]\nsslmode=allow\n", "harness"),
        );
        assert!(failure.unwrap_err().contains("allow"));
    }
}

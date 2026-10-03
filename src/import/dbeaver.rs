//! DBeaver keeps one workspace per install, a directory per project under it,
//! and each project's connections in `.dbeaver/data-sources*.json`. Users and
//! passwords sit beside each in the `credentials-config*.json` of the same
//! suffix, encrypted.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use serde_json::Value;

use super::{Imported, Report, Skipped, port, root_certificate};
use crate::{
    db::{ConnectionConfig, ServerConfig, SshTunnel, SslMode},
    store,
    theme::ConnectionColor,
};

/// Not a secret: DBeaver hard-codes it (`BaseProjectImpl`), so the file is
/// readable by anything that has read DBeaver's source.
const KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

const UNREADABLE_CREDENTIALS: &str =
    "DBeaver's saved credentials could not be read, so its connections came in without them.";

/// Every place an install keeps its workspace. Linux's Flatpak and Snap builds
/// keep theirs inside their sandboxes.
pub(super) fn workspaces() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let roots = store::home()
        .map(|home| home.join("Library"))
        .into_iter()
        .collect::<Vec<_>>();
    #[cfg(target_os = "windows")]
    let roots = store::data_root().into_iter().collect::<Vec<_>>();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let roots = store::data_root()
        .into_iter()
        .chain(store::home().into_iter().flat_map(|home| {
            [
                home.join(".var/app/io.dbeaver.DBeaverCommunity/data"),
                home.join("snap/dbeaver-ce/current/.local/share"),
            ]
        }))
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .map(|root| root.join("DBeaverData").join("workspace6"))
        .collect()
}

pub(super) fn read() -> Result<Report, String> {
    let mut report = Report::default();
    for root in workspaces().into_iter().filter(|root| root.is_dir()) {
        read_workspace(&root, &mut report)?;
    }
    Ok(report)
}

fn read_workspace(root: &Path, report: &mut Report) -> Result<(), String> {
    for project in entries(root)? {
        let directory = project.join(".dbeaver");
        if !directory.is_dir() {
            continue;
        }
        for file in entries(&directory)? {
            let Some(suffix) = file
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("data-sources"))
                .filter(|suffix| suffix.ends_with(".json"))
            else {
                continue;
            };
            let credentials = directory.join(format!("credentials-config{suffix}"));
            read_project_file(&file, &credentials, report);
        }
    }
    Ok(())
}

fn entries(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|error| format!("{} could not be read: {error}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

/// A credentials file that will not open costs the passwords, not the
/// connections: those still come in, and the report says why they have none.
fn read_project_file(data_sources: &Path, credentials: &Path, report: &mut Report) {
    let credentials = match fs::read(credentials) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Null,
        read => match read
            .map_err(|error| error.to_string())
            .and_then(|bytes| decrypt(&bytes))
        {
            Ok(credentials) => credentials,
            Err(_) => {
                if !report
                    .notes
                    .iter()
                    .any(|note| note == UNREADABLE_CREDENTIALS)
                {
                    report.notes.push(UNREADABLE_CREDENTIALS.to_string());
                }
                Value::Null
            }
        },
    };
    let document = fs::read(data_sources)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string())
        });
    match document {
        Ok(document) => read_connections(&document, &credentials, report),
        Err(error) => report.skipped.push(Skipped {
            name: data_sources.display().to_string(),
            reason: format!("could not be read: {error}"),
        }),
    }
}

/// The first block is the IV, the rest AES-128-CBC with PKCS#5 padding.
fn decrypt(bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() < 16 {
        return Err("too short to hold an IV".into());
    }
    let (iv, ciphertext) = bytes.split_at(16);
    let mut buffer = ciphertext.to_vec();
    let plaintext = cbc::Decryptor::<aes::Aes128>::new_from_slices(&KEY, iv)
        .map_err(|error| error.to_string())?
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| "does not decrypt".to_string())?;
    serde_json::from_slice(plaintext).map_err(|error| error.to_string())
}

fn read_connections(document: &Value, credentials: &Value, report: &mut Report) {
    let Some(connections) = document.get("connections").and_then(Value::as_object) else {
        return;
    };
    for (id, connection) in connections {
        let name = text(connection.get("name")).unwrap_or_else(|| id.clone());
        match import(name.clone(), connection, credentials.get(id)) {
            Ok(imported) => report.imported.push(imported),
            Err(reason) => report.skipped.push(Skipped { name, reason }),
        }
    }
}

/// What a provider becomes here. A constructor rather than an `Engine`, so
/// the one match on what DBeaver called it is the only one there is.
enum Target {
    Server(fn(ServerConfig) -> ConnectionConfig),
    File,
}

fn target(provider: &str, driver: &str) -> Result<Target, String> {
    match provider {
        // Redshift, Timescale, CockroachDB and the rest are drivers under it.
        "postgresql" => Ok(Target::Server(ConnectionConfig::Postgres)),
        "mysql" | "mariadb" => Ok(Target::Server(ConnectionConfig::MySql)),
        "sqlite" if driver == "sqlite_jdbc" => Ok(Target::File),
        "sqlite" => Err(format!("the {driver} driver isn't supported")),
        // DBeaver's own spelling of one of them.
        "mssql" if driver.starts_with("sybase") || driver.starts_with("sypase") => {
            Err("Sybase isn't supported".into())
        }
        "sqlserver" | "mssql" => Ok(Target::Server(ConnectionConfig::SqlServer)),
        "snowflake" => Err("DBDelve's Snowflake signs in with a key file only".into()),
        "oracle" => Err("Oracle isn't supported".into()),
        "generic" => Err(format!("{driver} isn't supported")),
        other => Err(format!("{other} isn't supported")),
    }
}

fn import(
    name: String,
    connection: &Value,
    credentials: Option<&Value>,
) -> Result<Imported, String> {
    // A connection made from a template names the template's driver in
    // `provider`/`driver`; the database it actually is, is the original.
    let named = |original: &str, key: &str| {
        text(connection.get(original))
            .or_else(|| text(connection.get(key)))
            .unwrap_or_default()
    };
    let driver = named("original-driver", "driver");
    let target = target(&named("original-provider", "provider"), &driver)?;
    let configuration = &connection["configuration"];
    let field = |key: &str| text(configuration.get(key));
    let url = field("url");
    let by_url = field("configurationType").as_deref() == Some("URL")
        || (field("host").is_none() && url.is_some());
    let mut notes = Vec::new();

    let config = match target {
        Target::File => ConnectionConfig::Sqlite {
            path: field("database")
                .or_else(|| {
                    url.as_deref()
                        .and_then(|url| url.strip_prefix("jdbc:sqlite:"))
                        .map(str::to_string)
                })
                .ok_or("it has no database file")?,
            statement_timeout: 0,
        },
        Target::Server(engine) => {
            let login = credentials.and_then(|credentials| credentials.get("#connection"));
            let user = text(login.and_then(|login| login.get("user")))
                .or_else(|| field("user"))
                .unwrap_or_default();
            let password = text(login.and_then(|login| login.get("password")))
                .or_else(|| field("password"))
                .unwrap_or_default();
            let mut config = if by_url {
                from_jdbc(url.as_deref().unwrap_or_default(), &user, &password)?
            } else {
                engine(ServerConfig {
                    host: field("host").ok_or("it has no host")?,
                    port: port("port", field("port"), &mut notes),
                    database: field("database").unwrap_or_default(),
                    user,
                    password,
                    ..ServerConfig::default()
                })
            };
            let microsoft = matches!(config, ConnectionConfig::SqlServer(_))
                && !driver.to_ascii_lowercase().contains("jtds");
            if let Some(server) = config.server_mut() {
                let ssh_login =
                    credentials.and_then(|credentials| credentials.get("network/ssh_tunnel"));
                server.ssh = ssh(
                    &configuration["handlers"]["ssh_tunnel"],
                    ssh_login,
                    &mut notes,
                );
                if let Some((sslmode, ca)) = ssl(configuration, microsoft, &mut notes) {
                    server.sslmode = sslmode;
                    server.root_certificate = root_certificate(sslmode, ca, &mut notes);
                }
            }
            config
        }
    };

    let color = match field("type").as_deref() {
        Some("prod") => Some(ConnectionColor::Red),
        Some("test") => Some(ConnectionColor::Yellow),
        _ => None,
    };
    Ok(Imported {
        name,
        config,
        color,
        notes,
    })
}

/// The three JDBC URLs that are the same URL as DBDelve's once `jdbc:` is
/// off. The user and password go in from the credentials file, which is where
/// DBeaver keeps them even for a URL connection.
fn from_jdbc(url: &str, user: &str, password: &str) -> Result<ConnectionConfig, String> {
    const UNREADABLE: &str = "only a JDBC URL, which DBDelve can't read for this database";
    let url = match url
        .strip_prefix("jdbc:")
        .and_then(|url| url.split_once("://"))
    {
        Some(("postgresql" | "mysql", _)) => url["jdbc:".len()..].to_string(),
        Some(("mariadb", rest)) => format!("mysql://{rest}"),
        _ => return Err(UNREADABLE.into()),
    };
    let mut url = url::Url::parse(&url).map_err(|error| error.to_string())?;
    // Set on the config rather than in the URL: `set_password` leaves a `%`
    // as it is, so the URL would decode a password like `p%41ss` as `pAss`.
    // The username goes in only so the URL parses; the config's is replaced.
    let fill_user = url.username().is_empty() && !user.is_empty();
    let fill_password = url.password().is_none() && !password.is_empty();
    if fill_user {
        _ = url.set_username("user");
    }
    let mut config = ConnectionConfig::from_url(url.as_str())?;
    if let Some(server) = config.server_mut() {
        if fill_user {
            server.user = user.to_string();
        }
        if fill_password {
            server.password = password.to_string();
        }
    }
    Ok(config)
}

fn ssh(handler: &Value, login: Option<&Value>, notes: &mut Vec<String>) -> Option<SshTunnel> {
    if !enabled(handler) {
        return None;
    }
    let properties = &handler["properties"];
    let property = |key: &str| text(properties.get(key));
    let mut left_off = |reason: &str| {
        notes.push(format!("SSH tunnel left off: {reason}"));
        None
    };
    let jumps = property("jumpServer.count")
        .and_then(|count| count.parse::<u32>().ok())
        .is_some_and(|count| count > 0)
        || property("jumpServerEnabled").as_deref() == Some("true");
    if jumps {
        return left_off("it goes through a jump server");
    }
    let identity_file = match property("authType").as_deref() {
        Some("PUBLIC_KEY") => property("keyPath"),
        Some("AGENT") => None,
        _ => return left_off("it logs in with a password"),
    };
    let Some(host) = property("host") else {
        return left_off("it has no host");
    };
    let identity_file = identity_file.filter(|path| {
        let relative = SshTunnel::identity_file_error(path).is_some();
        if relative {
            notes.push(format!(
                "SSH key {path} left off: it isn't an absolute path"
            ));
        }
        !relative
    });
    Some(SshTunnel {
        host,
        port: port("SSH port", property("port"), notes),
        user: text(login.and_then(|login| login.get("user")))
            .or_else(|| text(handler.get("user")))
            .unwrap_or_default(),
        identity_file,
    })
}

/// The SSL tab first, then the driver properties, then what Microsoft's driver
/// does unasked; `None` where nothing says, leaving a URL's own `sslmode`.
/// Postgres's tab says `sslMode`; MySQL's says only whether to require TLS and
/// whether to check the certificate's authority, which is what verify-ca
/// checks. A driver property about TLS that isn't read here is said, since it
/// may have asked for more than the mode it comes in with.
fn ssl(
    configuration: &Value,
    microsoft: bool,
    notes: &mut Vec<String>,
) -> Option<(SslMode, Option<String>)> {
    let handler = configuration["handlers"].as_object().and_then(|handlers| {
        handlers
            .iter()
            .find(|(id, handler)| id.ends_with("_ssl") && enabled(handler))
            .map(|(_, handler)| &handler["properties"])
    });
    let tab = |key: &str| handler.and_then(|properties| text(properties.get(key)));
    let properties = configuration["properties"].as_object();
    let driver = |key: &str| {
        properties?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| text(Some(value)))
    };

    if tab("ssl.client.cert").is_some() || tab("ssl.client.key").is_some() {
        notes.push("client certificate left off: DBDelve doesn't send one".into());
    }
    let read: &[&str] = if microsoft {
        &[
            "sslmode",
            "sslrootcert",
            "encrypt",
            "trustservercertificate",
        ]
    } else {
        &["sslmode", "sslrootcert"]
    };
    for name in properties
        .into_iter()
        .flat_map(|properties| properties.keys())
    {
        let lowered = name.to_ascii_lowercase();
        if ["ssl", "tls", "encrypt", "certificate"]
            .iter()
            .any(|word| lowered.contains(word))
            && !read.contains(&lowered.as_str())
        {
            notes.push(format!("driver property {name} left off"));
        }
    }

    let is = |value: Option<String>, expected: &str| {
        value.is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    let sslmode = if let Some(mode) = tab("sslMode").or_else(|| driver("sslmode")) {
        named_mode(&mode).unwrap_or_else(|| {
            notes.push(format!(
                "SSL mode {mode} isn't one DBDelve has, so it was set to verify-full"
            ));
            SslMode::VerifyFull
        })
    } else if is(tab("ssl.verify.server"), "true") {
        SslMode::VerifyCa
    } else if is(tab("ssl.require"), "true") {
        SslMode::Require
    } else if microsoft {
        // mssql-jdbc has encrypted and checked the certificate unless told
        // otherwise since 10.2, so silence is verify-full, not prefer.
        if is(driver("encrypt"), "false") || is(driver("encrypt"), "optional") {
            SslMode::Disable
        } else if is(driver("trustServerCertificate"), "true") {
            SslMode::Require
        } else {
            SslMode::VerifyFull
        }
    } else if handler.is_some() {
        SslMode::Prefer
    } else {
        return None;
    };
    Some((
        sslmode,
        tab("ssl.ca.cert").or_else(|| driver("sslrootcert")),
    ))
}

/// libpq's spellings, and Connector/J's for MySQL.
fn named_mode(mode: &str) -> Option<SslMode> {
    match mode.to_ascii_lowercase().replace('_', "-").as_str() {
        // libpq's `allow` tries plaintext first, which the driver cannot (see
        // `SslMode::parse`). `prefer` tries TLS first: never less than asked.
        "allow" | "preferred" => Some(SslMode::Prefer),
        "disabled" => Some(SslMode::Disable),
        "required" => Some(SslMode::Require),
        "verify-identity" => Some(SslMode::VerifyFull),
        other => SslMode::parse(other).ok(),
    }
}

fn enabled(handler: &Value) -> bool {
    text(handler.get("enabled")).as_deref() == Some("true")
}

/// DBeaver writes a port as a string in one place and a number in the next,
/// and a flag as either a boolean or `"true"`.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
    .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockEncryptMut;
    use serde_json::json;

    use super::*;

    fn encrypt(plaintext: &[u8]) -> Vec<u8> {
        let iv = [7u8; 16];
        let mut buffer = plaintext.to_vec();
        buffer.resize(plaintext.len() + 16, 0);
        let ciphertext = cbc::Encryptor::<aes::Aes128>::new_from_slices(&KEY, &iv)
            .expect("a 16-byte key and IV")
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .expect("room for a block of padding");
        [iv.as_slice(), ciphertext].concat()
    }

    fn connection(provider: &str, driver: &str, configuration: Value) -> Value {
        json!({ "provider": provider, "driver": driver, "name": "c", "configuration": configuration })
    }

    fn manual(host: &str, port: &str, database: &str) -> Value {
        json!({ "host": host, "port": port, "database": database, "configurationType": "MANUAL" })
    }

    fn login(user: &str, password: &str) -> Value {
        json!({ "#connection": { "user": user, "password": password } })
    }

    fn imported(connection: Value, credentials: Option<Value>) -> Result<Imported, String> {
        import("c".into(), &connection, credentials.as_ref())
    }

    fn server(imported: &Imported) -> &ServerConfig {
        imported.config.server().expect("a server engine")
    }

    fn with_handlers(handlers: Value) -> Value {
        let mut configuration = manual("db.example.com", "5432", "app");
        configuration["handlers"] = handlers;
        connection("postgresql", "postgres-jdbc", configuration)
    }

    #[test]
    fn credentials_decrypt_with_dbeavers_key() {
        let credentials = json!({ "id": login("alice", "s3cret") });
        let encrypted = encrypt(credentials.to_string().as_bytes());
        assert_eq!(decrypt(&encrypted), Ok(credentials));
        assert!(decrypt(b"short").is_err());
        assert!(decrypt(&[0u8; 48]).is_err());
    }

    #[test]
    fn providers_map_to_engines() {
        let engine = |provider: &str, driver: &str| {
            imported(
                connection(provider, driver, manual("h", "1", "d")),
                Some(login("u", "")),
            )
            .map(|imported| imported.config.engine())
        };
        use crate::db::Engine;
        assert_eq!(engine("postgresql", "postgres-jdbc"), Ok(Engine::Postgres));
        assert_eq!(engine("postgresql", "redshift"), Ok(Engine::Postgres));
        assert_eq!(engine("mysql", "mysql8"), Ok(Engine::MySql));
        assert_eq!(engine("mysql", "mariaDB"), Ok(Engine::MySql));
        assert_eq!(engine("mariadb", "mariaDB"), Ok(Engine::MySql));
        assert_eq!(engine("sqlserver", "microsoft"), Ok(Engine::SqlServer));
        assert_eq!(engine("mssql", "jtds"), Ok(Engine::SqlServer));
        assert_eq!(
            engine("mssql", "sybase-jtds"),
            Err("Sybase isn't supported".into())
        );
        assert_eq!(engine("mssql", "sypase_jconnect").ok(), None);
        assert_eq!(
            engine("snowflake", "snowflake"),
            Err("DBDelve's Snowflake signs in with a key file only".into())
        );
        assert_eq!(
            engine("oracle", "oracle_thin"),
            Err("Oracle isn't supported".into())
        );
        assert_eq!(engine("sqlite", "libsql").ok(), None);

        let mut templated = connection("generic", "custom", manual("h", "1", "d"));
        templated["original-provider"] = json!("postgresql");
        templated["original-driver"] = json!("postgres-jdbc");
        assert_eq!(
            imported(templated, None).map(|imported| imported.config.engine()),
            Ok(Engine::Postgres)
        );
    }

    #[test]
    fn a_manual_server_connection_takes_its_login_from_the_credentials() {
        let mut configuration = manual("db.example.com", "5432", "app");
        configuration["type"] = json!("dev");
        let imported = imported(
            connection("postgresql", "postgres-jdbc", configuration),
            Some(login("alice@example.com", "")),
        )
        .expect("imports");
        assert_eq!(
            imported.config,
            ConnectionConfig::Postgres(ServerConfig {
                host: "db.example.com".into(),
                port: Some(5432),
                database: "app".into(),
                user: "alice@example.com".into(),
                ..ServerConfig::default()
            })
        );
        assert_eq!(imported.color, None);
        assert!(imported.notes.is_empty());
    }

    #[test]
    fn the_plain_login_in_data_sources_is_the_fallback() {
        let mut configuration = manual("h", "3306", "app");
        configuration["user"] = json!("root");
        configuration["password"] = json!("pw");
        let imported = imported(connection("mysql", "mysql8", configuration), None).unwrap();
        assert_eq!(
            (
                server(&imported).user.as_str(),
                server(&imported).password.as_str()
            ),
            ("root", "pw")
        );
    }

    #[test]
    fn sqlite_reads_its_path_from_the_database_or_the_url() {
        let file = |configuration: Value| {
            imported(connection("sqlite", "sqlite_jdbc", configuration), None).map(|i| i.config)
        };
        let sqlite = |path: &str| ConnectionConfig::Sqlite {
            path: path.into(),
            statement_timeout: 0,
        };
        assert_eq!(
            file(json!({ "database": "/data/app.db" })),
            Ok(sqlite("/data/app.db"))
        );
        assert_eq!(
            file(json!({ "url": "jdbc:sqlite:/data/app.db", "configurationType": "URL" })),
            Ok(sqlite("/data/app.db"))
        );
    }

    #[test]
    fn url_connections_read_postgres_mysql_and_mariadb_urls() {
        let by_url = |provider: &str, url: &str| {
            imported(
                connection(
                    provider,
                    "d",
                    json!({ "url": url, "configurationType": "URL" }),
                ),
                Some(login("a@b", "p")),
            )
            .map(|imported| imported.config)
        };

        let ConnectionConfig::Postgres(postgres) =
            by_url("postgresql", "jdbc:postgresql://db.example.com:5433/app").unwrap()
        else {
            panic!("not Postgres");
        };
        assert_eq!(
            (
                postgres.host.as_str(),
                postgres.port,
                postgres.database.as_str(),
                postgres.user.as_str(),
                postgres.password.as_str()
            ),
            ("db.example.com", Some(5433), "app", "a@b", "p")
        );
        let ConnectionConfig::MySql(mysql) = by_url("mysql", "jdbc:mysql://h:3306/app").unwrap()
        else {
            panic!("not MySQL");
        };
        assert_eq!(
            (mysql.user.as_str(), mysql.database.as_str()),
            ("a@b", "app")
        );
        let encoded = imported(
            connection(
                "postgresql",
                "d",
                json!({ "url": "jdbc:postgresql://h/app", "configurationType": "URL" }),
            ),
            Some(login("a%40b", "p%41ss")),
        )
        .unwrap();
        assert_eq!(
            (
                server(&encoded).user.as_str(),
                server(&encoded).password.as_str()
            ),
            ("a%40b", "p%41ss")
        );
        assert!(matches!(
            by_url("mysql", "jdbc:mariadb://h/app"),
            Ok(ConnectionConfig::MySql(_))
        ));
        assert_eq!(
            by_url("sqlserver", "jdbc:sqlserver://h;databaseName=app"),
            Err("only a JDBC URL, which DBDelve can't read for this database".into())
        );
        // No host at all reads as a URL connection too.
        assert!(matches!(
            imported(
                connection(
                    "postgresql",
                    "d",
                    json!({ "url": "jdbc:postgresql://h/app" })
                ),
                Some(login("u", "")),
            ),
            Ok(Imported {
                config: ConnectionConfig::Postgres(_),
                ..
            })
        ));
    }

    #[test]
    fn ssh_tunnels_come_in_only_where_ssh_can_open_them_unattended() {
        let tunnel = |properties: Value, enabled: bool| {
            let mut connection = with_handlers(json!({
                "ssh_tunnel": { "enabled": enabled, "properties": properties }
            }));
            connection["name"] = json!("c");
            let credentials = json!({ "network/ssh_tunnel": { "user": "deploy" } });
            let imported = imported(connection, Some(credentials)).unwrap();
            (server(&imported).ssh.clone(), imported.notes)
        };

        assert_eq!(
            tunnel(
                json!({ "host": "bastion", "port": 2222, "authType": "PUBLIC_KEY", "keyPath": "~/.ssh/id_ed25519" }),
                true
            ),
            (
                Some(SshTunnel {
                    host: "bastion".into(),
                    port: Some(2222),
                    user: "deploy".into(),
                    identity_file: Some("~/.ssh/id_ed25519".into()),
                }),
                vec![]
            )
        );
        assert_eq!(
            tunnel(json!({ "host": "bastion", "authType": "AGENT" }), true),
            (
                Some(SshTunnel {
                    host: "bastion".into(),
                    port: None,
                    user: "deploy".into(),
                    identity_file: None,
                }),
                vec![]
            )
        );
        assert_eq!(
            tunnel(json!({ "host": "bastion", "authType": "PASSWORD" }), true),
            (
                None,
                vec!["SSH tunnel left off: it logs in with a password".to_string()]
            )
        );
        assert_eq!(
            tunnel(
                json!({ "host": "bastion", "authType": "AGENT", "jumpServer.count": "1" }),
                true
            ),
            (
                None,
                vec!["SSH tunnel left off: it goes through a jump server".to_string()]
            )
        );
        assert_eq!(
            tunnel(json!({ "host": "bastion", "authType": "PASSWORD" }), false),
            (None, vec![])
        );
        let (relative, notes) = tunnel(
            json!({ "host": "bastion", "authType": "PUBLIC_KEY", "keyPath": "keys/id_rsa" }),
            true,
        );
        assert_eq!(relative.map(|tunnel| tunnel.identity_file), Some(None));
        assert_eq!(
            notes,
            vec!["SSH key keys/id_rsa left off: it isn't an absolute path".to_string()]
        );
    }

    #[test]
    fn ssl_handlers_never_ask_for_less_than_dbeaver_did() {
        let ssl = |properties: Value| {
            let imported = imported(
                with_handlers(
                    json!({ "postgre_ssl": { "enabled": true, "properties": properties } }),
                ),
                None,
            )
            .unwrap();
            (
                server(&imported).sslmode,
                server(&imported).root_certificate.clone(),
                imported.notes,
            )
        };

        assert_eq!(
            ssl(json!({ "sslMode": "verify-full", "ssl.ca.cert": "/certs/ca.pem" })),
            (SslMode::VerifyFull, Some("/certs/ca.pem".into()), vec![])
        );
        assert_eq!(ssl(json!({ "sslMode": "allow" })).0, SslMode::Prefer);
        assert_eq!(
            ssl(json!({ "sslMode": "require", "ssl.ca.cert": "/certs/ca.pem" })),
            (
                SslMode::Require,
                None,
                vec!["CA certificate left off: SSL mode require doesn't check one".to_string()]
            )
        );
        assert_eq!(
            ssl(
                json!({ "sslMode": "verify-ca", "ssl.client.cert": "/c.pem", "ssl.client.key": "/k.pem" })
            ),
            (
                SslMode::VerifyCa,
                None,
                vec!["client certificate left off: DBDelve doesn't send one".to_string()]
            )
        );
        assert_eq!(
            ssl(json!({ "ssl.verify.server": "true" })).0,
            SslMode::VerifyCa
        );

        let disabled = imported(
            with_handlers(json!({ "postgre_ssl": { "enabled": false, "properties": { "sslMode": "disable" } } })),
            None,
        )
        .unwrap();
        assert_eq!(server(&disabled).sslmode, SslMode::default());
    }

    #[test]
    fn driver_properties_and_sql_servers_default_never_ask_for_less() {
        let ssl = |provider: &str, driver: &str, properties: Value| {
            let mut configuration = manual("db.example.com", "1433", "app");
            configuration["properties"] = properties;
            let imported = imported(connection(provider, driver, configuration), None).unwrap();
            (server(&imported).sslmode, imported.notes)
        };
        use SslMode::*;

        assert_eq!(
            ssl(
                "postgresql",
                "postgres-jdbc",
                json!({ "sslmode": "verify-full" })
            ),
            (VerifyFull, vec![])
        );
        assert_eq!(
            ssl("mysql", "mysql8", json!({ "sslMode": "VERIFY_IDENTITY" })),
            (VerifyFull, vec![])
        );
        assert_eq!(
            ssl(
                "mysql",
                "mysql8",
                json!({ "useSSL": "true", "requireSSL": "true" })
            ),
            (
                Prefer,
                vec![
                    "driver property useSSL left off".to_string(),
                    "driver property requireSSL left off".to_string(),
                ]
            )
        );
        assert_eq!(
            ssl("postgresql", "postgres-jdbc", json!({})),
            (Prefer, vec![])
        );

        assert_eq!(
            ssl("sqlserver", "microsoft", json!({})),
            (VerifyFull, vec![])
        );
        assert_eq!(
            ssl(
                "sqlserver",
                "microsoft",
                json!({ "trustServerCertificate": "true" })
            ),
            (Require, vec![])
        );
        assert_eq!(
            ssl("sqlserver", "microsoft", json!({ "encrypt": "false" })),
            (Disable, vec![])
        );
        assert_eq!(
            ssl("mssql", "jtds_sqlserver", json!({ "ssl": "require" })),
            (Prefer, vec!["driver property ssl left off".to_string()])
        );
    }

    #[test]
    fn production_is_red_and_test_is_yellow() {
        let color = |kind: &str| {
            let mut configuration = manual("h", "1", "d");
            configuration["type"] = json!(kind);
            imported(
                connection("postgresql", "postgres-jdbc", configuration),
                None,
            )
            .unwrap()
            .color
        };
        assert_eq!(color("prod"), Some(ConnectionColor::Red));
        assert_eq!(color("test"), Some(ConnectionColor::Yellow));
        assert_eq!(color("dev"), None);
    }

    #[test]
    fn a_port_that_is_not_one_is_left_blank_and_said() {
        let imported = imported(
            connection("postgresql", "postgres-jdbc", manual("h", "54x2", "d")),
            None,
        )
        .unwrap();
        assert_eq!(server(&imported).port, None);
        assert_eq!(
            imported.notes,
            vec!["port 54x2 isn't a port number, so it was left blank".to_string()]
        );
    }

    #[test]
    fn a_workspace_is_every_data_sources_file_in_every_project() {
        let root = std::env::temp_dir().join(format!(
            "dbdelve-dbeaver-workspace-test-{}",
            std::process::id()
        ));
        _ = fs::remove_dir_all(&root);
        let project = |name: &str| {
            let directory = root.join(name).join(".dbeaver");
            fs::create_dir_all(&directory).unwrap();
            directory
        };
        let sources = |id: &str, name: &str| {
            json!({ "connections": { id: {
                "provider": "postgresql", "driver": "postgres-jdbc", "name": name,
                "configuration": manual("h", "5432", name)
            } } })
            .to_string()
        };

        let general = project("General");
        fs::write(general.join("data-sources.json"), sources("a", "alpha")).unwrap();
        fs::write(
            general.join("credentials-config.json"),
            encrypt(json!({ "a": login("alice", "pw") }).to_string().as_bytes()),
        )
        .unwrap();
        fs::write(general.join("data-sources-2.json"), sources("b", "beta")).unwrap();
        fs::write(general.join("credentials-config-2.json"), b"garbled").unwrap();
        fs::write(general.join("data-sources.json.bak"), sources("x", "stale")).unwrap();

        let other = project("Other");
        fs::write(other.join("data-sources.json"), sources("c", "gamma")).unwrap();
        fs::create_dir_all(root.join("NotAProject")).unwrap();

        let mut report = Report::default();
        let read = read_workspace(&root, &mut report);
        _ = fs::remove_dir_all(&root);
        read.unwrap();

        let logins = report
            .imported
            .iter()
            .map(|imported| (imported.name.as_str(), server(imported).user.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(logins, [("beta", ""), ("alpha", "alice"), ("gamma", "")]);
        assert_eq!(server(&report.imported[1]).password, "pw");
        assert_eq!(report.notes, [UNREADABLE_CREDENTIALS]);
        assert!(report.skipped.is_empty());
    }
}

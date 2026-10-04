//! TablePlus keeps every connection in one `Connections.plist`, an array of
//! dictionaries, and each saved password and imported SSH key in the Keychain
//! under the connection's `ID`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;

use super::{Imported, Report, Skipped, mongo, mongo_url, port, root_certificate};
use crate::{
    db::{ConnectionConfig, ServerConfig, SshTunnel, SslMode},
    store,
    theme::ConnectionColor,
};

const UNREADABLE_PASSWORDS: &str = "TablePlus's saved passwords could not be read from the Keychain, so its connections came in without them.";
const UNLOCATED_KEY: &str = "SSH key left off: its file couldn't be located, so ssh falls back to your ssh config and agent";
const UNREADABLE_KEY: &str = "SSH key left off: TablePlus's stored key couldn't be read, so ssh falls back to your ssh config and agent";

/// The App Store / direct download build, then the Setapp one.
#[cfg(target_os = "macos")]
pub(super) fn files() -> Vec<PathBuf> {
    let Ok(home) = crate::store::home() else {
        return Vec::new();
    };
    ["com.tinyapp.TablePlus", "com.tinyapp.TablePlus-setapp"]
        .into_iter()
        .map(|app| {
            home.join("Library/Application Support")
                .join(app)
                .join("Data/Connections.plist")
        })
        .collect()
}

/// Where TablePlus keeps its connections on Windows and Linux is unverified,
/// so it is not looked for there.
#[cfg(not(target_os = "macos"))]
pub(super) fn files() -> Vec<PathBuf> {
    Vec::new()
}

pub(super) fn read() -> Result<Report, String> {
    let mut rows = Vec::new();
    for file in files().into_iter().filter(|file| file.is_file()) {
        rows.extend(plist_rows(&file)?);
    }
    let mut report = Report::default();
    read_rows(rows, keychain_secret, write_key, &mut report);
    Ok(report)
}

// ponytail: plutil rather than a plist parser. It cannot put a date or data
// value into JSON, so a file holding one fails whole; the `plist` crate reads
// both, and is the upgrade if that bites.
fn plist_rows(file: &Path) -> Result<Vec<Value>, String> {
    let unreadable = |why: &str| format!("TablePlus's {} could not be read: {why}", file.display());
    let output = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(file)
        .output()
        .map_err(|error| unreadable(&error.to_string()))?;
    if !output.status.success() {
        return Err(unreadable(String::from_utf8_lossy(&output.stderr).trim()));
    }
    match serde_json::from_slice(&output.stdout) {
        Ok(Value::Array(rows)) => Ok(rows),
        Ok(_) => Err(unreadable("it isn't a list of connections")),
        Err(error) => Err(unreadable(&error.to_string())),
    }
}

/// Another app's item, so macOS asks the user before handing it over.
fn keychain_secret(account: &str) -> Result<Option<Vec<u8>>, String> {
    let entry = keyring::Entry::new("com.tableplus.TablePlus", account)
        .map_err(|error| error.to_string())?;
    match entry.get_secret() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Returns the path the tunnel names, or the note saying why there is none.
fn write_key(id: &str, key: &[u8]) -> Result<String, String> {
    if let Some(reason) = store::unsafe_component(id) {
        return Err(format!("SSH key left off: its connection ID {reason}"));
    }
    let path = store::ssh_key_directory()
        .map_err(|error| format!("SSH key left off: {error}"))?
        .join(format!("tableplus-{id}"));
    let unwritten = |why: &dyn std::fmt::Display| {
        format!(
            "SSH key left off: it couldn't be written to {}: {why}",
            path.display()
        )
    };
    store::write_private_key(&path, key).map_err(|error| unwritten(&error.kind()))?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| unwritten(&"the path isn't valid UTF-8"))
}

/// TablePlus's encoding of the item is unverified, so both PEM text and
/// base64 of it are taken.
fn private_key(stored: Vec<u8>) -> Option<Vec<u8>> {
    let is_key = |bytes: &[u8]| {
        std::str::from_utf8(bytes)
            .is_ok_and(|text| text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----"))
    };
    if is_key(&stored) {
        return Some(stored);
    }
    let compact = stored
        .into_iter()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    STANDARD
        .decode(compact)
        .ok()
        .filter(|decoded| is_key(decoded))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Row {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "ConnectionName")]
    name: String,
    #[serde(rename = "Driver")]
    driver: String,
    #[serde(rename = "DatabaseHost")]
    host: String,
    #[serde(rename = "DatabasePort")]
    port: String,
    #[serde(rename = "DatabaseName")]
    database: String,
    #[serde(rename = "DatabaseUser")]
    user: String,
    #[serde(rename = "DatabasePath")]
    path: String,
    /// 0 is the Keychain, 1 ask every time, 2 none, 3 a command's output.
    #[serde(rename = "DatabasePasswordMode")]
    password_mode: i64,
    #[serde(rename = "tLSMode")]
    tls_mode: i64,
    /// Client key, client certificate, CA certificate.
    #[serde(rename = "TlsKeyPaths")]
    tls_key_paths: Vec<String>,
    /// What each path in `TlsKeyPaths` is, in order, as the driver names them:
    /// `Certificate Key,Certificate Authority` on MongoDB.
    #[serde(rename = "TlsKeyName")]
    tls_key_name: String,
    /// What the dropdown behind the driver's "Advanced" options holds. Its
    /// entries' shape is unknown, so only whether there are any is read.
    #[serde(rename = "OtherOptions")]
    other_options: Vec<Value>,
    #[serde(rename = "isOverSSH")]
    over_ssh: bool,
    #[serde(rename = "ServerAddress")]
    ssh_host: String,
    #[serde(rename = "ServerPort")]
    ssh_port: String,
    #[serde(rename = "ServerUser")]
    ssh_user: String,
    #[serde(rename = "isUsePrivateKey")]
    ssh_uses_key: bool,
    #[serde(rename = "ServerPrivateKeyName")]
    ssh_key: String,
    /// The same scale as `DatabasePasswordMode`.
    #[serde(rename = "ServerPasswordMode")]
    ssh_password_mode: i64,
    #[serde(rename = "Enviroment")]
    environment: String,
}

/// The first row with an `ID` wins, so a connection in both the direct and the
/// Setapp build comes in once. Secrets are looked up only after a row is
/// known to import, and not at all once the Keychain has refused one: a user
/// who denied the first prompt should not see one per connection.
///
/// ssh reads a key only from a file, so a key TablePlus keeps in the Keychain
/// is written to one of DBDelve's by `write_key`.
fn read_rows(
    rows: Vec<Value>,
    mut secret: impl FnMut(&str) -> Result<Option<Vec<u8>>, String>,
    mut write_key: impl FnMut(&str, &[u8]) -> Result<String, String>,
    report: &mut Report,
) {
    let mut seen = HashSet::new();
    let mut keychain_refused = false;
    for value in rows {
        let row = match Row::deserialize(&value) {
            Ok(row) => row,
            Err(error) => {
                report.skipped.push(Skipped {
                    name: value
                        .get("ConnectionName")
                        .and_then(Value::as_str)
                        .unwrap_or("A TablePlus connection")
                        .to_string(),
                    reason: format!("could not be read: {error}"),
                });
                continue;
            }
        };
        if !row.id.is_empty() && !seen.insert(row.id.clone()) {
            continue;
        }
        let name = if row.name.is_empty() {
            row.id.clone()
        } else {
            row.name.clone()
        };
        let mut imported = match import(name.clone(), &row) {
            Ok(imported) => imported,
            Err(reason) => {
                report.skipped.push(Skipped { name, reason });
                continue;
            }
        };
        if let Some(server) = imported.config.server_mut()
            && !row.id.is_empty()
        {
            let was_refused = keychain_refused;
            if row.password_mode == 0 && !keychain_refused {
                // A password that isn't text is what `get_password` refused.
                match secret(&format!("{}_database", row.id))
                    .map(|found| found.map(String::from_utf8))
                {
                    Ok(None) => {}
                    Ok(Some(Ok(password))) => server.password = password,
                    Ok(Some(Err(_))) | Err(_) => keychain_refused = true,
                }
            }
            if let Some(tunnel) = server.ssh.as_mut()
                && row.ssh_uses_key
                && tunnel.identity_file.is_none()
                && !keychain_refused
            {
                match secret(&format!("{}_private_key_data", row.id)) {
                    Ok(None) => {}
                    Ok(Some(stored)) => {
                        let written = private_key(stored)
                            .ok_or_else(|| UNREADABLE_KEY.to_string())
                            .and_then(|key| write_key(&row.id, &key));
                        imported.notes.retain(|note| note != UNLOCATED_KEY);
                        match written {
                            Ok(path) => tunnel.identity_file = Some(path),
                            Err(note) => imported.notes.push(note),
                        }
                    }
                    Err(_) => keychain_refused = true,
                }
            }
            if keychain_refused && !was_refused {
                report.notes.push(UNREADABLE_PASSWORDS.to_string());
            }
        }
        report.imported.push(imported);
    }
}

/// The engine, and what each index of TablePlus's TLS dropdown for the driver
/// means.
enum Target {
    Server(fn(ServerConfig) -> ConnectionConfig, &'static [SslMode]),
    File,
}

const POSTGRES_TLS: &[SslMode] = &[
    SslMode::Prefer,
    SslMode::Disable,
    SslMode::Require,
    // TablePlus's label for this one could be read as libpq's `allow`, which
    // the driver can't do (see `SslMode::parse`). `prefer` tries TLS first, so
    // it is never less than either reading asked for.
    SslMode::Prefer,
    SslMode::VerifyCa,
    SslMode::VerifyFull,
];
const MYSQL_TLS: &[SslMode] = &[
    SslMode::Prefer,
    SslMode::Disable,
    SslMode::Require,
    SslMode::VerifyCa,
    SslMode::VerifyFull,
];
const MARIADB_TLS: &[SslMode] = &[SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];
// Only the first entry of its dropdown is known, as with SQL Server below.
const MONGO_TLS: &[SslMode] = &[SslMode::Prefer];

fn target(driver: &str) -> Result<Target, String> {
    let lowered = driver.to_ascii_lowercase();
    match lowered.as_str() {
        "postgresql" | "cockroach" | "greenplum" | "redshift" => {
            Ok(Target::Server(ConnectionConfig::Postgres, POSTGRES_TLS))
        }
        "mysql" => Ok(Target::Server(ConnectionConfig::MySql, MYSQL_TLS)),
        "mariadb" => Ok(Target::Server(ConnectionConfig::MariaDb, MARIADB_TLS)),
        "mongo" | "mongodb" => Ok(Target::Server(mongo, MONGO_TLS)),
        "sqlite" => Ok(Target::File),
        "snowflake" => Err("DBDelve's Snowflake signs in with a key file only".into()),
        // Only the first entry of its dropdown is known, so any other comes in
        // as verify-full rather than as something it may be stronger than.
        _ if lowered.contains("sqlserver") || lowered.contains("sql server") => Ok(Target::Server(
            ConnectionConfig::SqlServer,
            &[SslMode::Prefer],
        )),
        _ => Err(format!("{driver} isn't supported")),
    }
}

fn import(name: String, row: &Row) -> Result<Imported, String> {
    let mut notes = Vec::new();
    let config = match target(&row.driver)? {
        Target::File => ConnectionConfig::Sqlite {
            path: [&row.path, &row.host]
                .into_iter()
                .find(|path| !path.is_empty())
                .ok_or("it has no database file")?
                .clone(),
            statement_timeout: 0,
        },
        Target::Server(engine, tls) => {
            if row.host.is_empty() {
                return Err("it has no host".into());
            }
            let (sslmode, ca) = ssl(row, tls, &mut notes);
            let mut config = engine(ServerConfig {
                host: row.host.clone(),
                port: port("port", filled(&row.port), &mut notes),
                database: row.database.clone(),
                user: row.user.clone(),
                sslmode,
                root_certificate: root_certificate(sslmode, ca, &mut notes),
                ssh: ssh(row, &mut notes),
                ..ServerConfig::default()
            });
            if matches!(config, ConnectionConfig::MongoDb(_)) {
                // A host that is a connection string says more than the
                // fields beside it, TLS included, so it is read as one. Only
                // the tunnel is kept from the fields: a URL has no place for it.
                if row.host.starts_with("mongodb://") || row.host.starts_with("mongodb+srv://") {
                    let mut from_url = mongo_url(&row.host, &row.user, "")?;
                    if let (Some(url), Some(fields)) = (from_url.server_mut(), config.server()) {
                        url.ssh = fields.ssh.clone();
                    }
                    config = from_url;
                }
                if !row.other_options.is_empty() {
                    notes.push("other options left off: their format isn't known".into());
                }
            }
            config
        }
    };
    let color = match row.environment.to_ascii_lowercase().as_str() {
        "production" => Some(ConnectionColor::Red),
        "staging" => Some(ConnectionColor::Yellow),
        "testing" => Some(ConnectionColor::Blue),
        "development" => Some(ConnectionColor::Green),
        _ => None,
    };
    Ok(Imported {
        name,
        config,
        color,
        notes,
    })
}

fn ssl(row: &Row, tls: &[SslMode], notes: &mut Vec<String>) -> (SslMode, Option<String>) {
    let sslmode = usize::try_from(row.tls_mode)
        .ok()
        .and_then(|index| tls.get(index).copied())
        .unwrap_or_else(|| {
            notes.push(format!(
                "SSL mode {} isn't one DBDelve has, so it was set to verify-full",
                row.tls_mode
            ));
            SslMode::VerifyFull
        });
    let key_path = |index: usize| row.tls_key_paths.get(index).and_then(|path| filled(path));
    let authority = row
        .tls_key_name
        .split(',')
        .position(|name| name.trim() == "Certificate Authority")
        .unwrap_or(2);
    if (0..row.tls_key_paths.len().max(authority))
        .any(|index| index != authority && key_path(index).is_some())
    {
        notes.push("client certificate left off: DBDelve doesn't send one".into());
    }
    (sslmode, key_path(authority))
}

fn ssh(row: &Row, notes: &mut Vec<String>) -> Option<SshTunnel> {
    if !row.over_ssh {
        return None;
    }
    let left_off = |notes: &mut Vec<String>, reason: &str| {
        notes.push(format!("SSH tunnel left off: {reason}"));
        None
    };
    // TablePlus may name only a key it imported into its own store, looked
    // up by `read_rows`, or keep its "Import a private key..." placeholder.
    let identity_file = if row.ssh_uses_key {
        let located = SshTunnel::identity_file_error(&row.ssh_key).is_none();
        if !located {
            notes.push(UNLOCATED_KEY.into());
        }
        located.then(|| row.ssh_key.clone())
    } else if row.ssh_password_mode == 2 {
        None
    } else {
        return left_off(notes, "it logs in with a password");
    };
    if row.ssh_host.is_empty() {
        return left_off(notes, "it has no host");
    }
    Some(SshTunnel {
        host: row.ssh_host.clone(),
        port: port("SSH port", filled(&row.ssh_port), notes),
        user: row.ssh_user.clone(),
        identity_file,
    })
}

fn filled(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::Engine;

    fn row(driver: &str, extra: Value) -> Value {
        let mut row = json!({
            "ID": "id", "ConnectionName": "c", "Driver": driver,
            "DatabaseHost": "db.example.com", "DatabasePort": "", "DatabaseName": "app",
            "DatabaseUser": "alice@example.com", "DatabasePasswordMode": 1,
        });
        for (key, value) in extra.as_object().expect("an object") {
            row[key] = value.clone();
        }
        row
    }

    fn imported(driver: &str, extra: Value) -> Result<Imported, String> {
        import("c".into(), &Row::deserialize(&row(driver, extra)).unwrap())
    }

    fn server(imported: &Imported) -> &ServerConfig {
        imported.config.server().expect("a server engine")
    }

    fn named(id: &str, name: &str, extra: Value) -> Value {
        let mut row = row("PostgreSQL", extra);
        row["ID"] = json!(id);
        row["ConnectionName"] = json!(name);
        row
    }

    fn read(
        rows: Vec<Value>,
        secret: impl FnMut(&str) -> Result<Option<Vec<u8>>, String>,
    ) -> Report {
        let mut report = Report::default();
        read_rows(
            rows,
            secret,
            |id, _| Ok(format!("/keys/tableplus-{id}")),
            &mut report,
        );
        report
    }

    const KEY: &str =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";

    fn over_ssh(id: &str, key: &str, password_mode: i64) -> Value {
        named(
            id,
            id,
            json!({
                "DatabasePasswordMode": password_mode, "isOverSSH": true,
                "ServerAddress": "bastion", "isUsePrivateKey": true, "ServerPrivateKeyName": key,
            }),
        )
    }

    fn identity_file(imported: &Imported) -> Option<&str> {
        server(imported).ssh.as_ref()?.identity_file.as_deref()
    }

    /// The stored item is handed over for every row, and what was written is
    /// compared here so a failure never prints key bytes.
    fn read_key(stored: Vec<u8>, written: &str) -> (Option<String>, Vec<String>) {
        let mut wrote = Vec::new();
        let mut report = Report::default();
        read_rows(
            vec![over_ssh("a", "id_ed25519", 1)],
            |_| Ok(Some(stored.clone())),
            |id, key| {
                wrote.push((id.to_string(), key == written.as_bytes()));
                Ok(format!("/keys/tableplus-{id}"))
            },
            &mut report,
        );
        let imported = &report.imported[0];
        let identity = identity_file(imported).map(str::to_string);
        assert!(
            wrote.iter().all(|(id, matches)| id == "a" && *matches),
            "the key written isn't the one expected"
        );
        assert_eq!(wrote.is_empty(), identity.is_none());
        (identity, imported.notes.clone())
    }

    /// A MongoDB row as TablePlus wrote it, trimmed to what is read.
    fn mongo_row(extra: Value) -> Value {
        let mut base = row(
            "Mongo",
            json!({
                "DatabaseHost": "127.0.0.1", "DatabasePort": "57017", "DatabaseName": "app",
                "DatabaseUser": "", "tLSMode": 0, "TlsKeyName": "Certificate Key,Certificate Authority",
                "TlsKeyPaths": ["", ""], "OtherOptions": [],
            }),
        );
        for (key, value) in extra.as_object().expect("an object") {
            base[key] = value.clone();
        }
        base
    }

    #[test]
    fn a_mongo_row_becomes_a_mongodb_profile() {
        let imported = import(
            "m".into(),
            &Row::deserialize(&mongo_row(json!({}))).unwrap(),
        )
        .unwrap();
        let ConnectionConfig::MongoDb(config) = &imported.config else {
            panic!("{:?}", imported.config)
        };
        assert_eq!(imported.config.engine(), Engine::MongoDb);
        assert_eq!(
            (config.server.host.as_str(), config.server.port),
            ("127.0.0.1", Some(57017))
        );
        assert_eq!(config.server.database, "app");
        assert_eq!(config.server.sslmode, SslMode::Prefer);
        assert!(!config.srv && config.options.is_empty());
        assert!(imported.notes.is_empty(), "{:?}", imported.notes);
    }

    #[test]
    fn mongo_reads_its_ca_from_its_own_slot_and_never_sends_a_client_certificate() {
        let imported = import(
            "m".into(),
            &Row::deserialize(&mongo_row(json!({
                "tLSMode": 9, "TlsKeyPaths": ["/c/client.pem", "/c/ca.pem"],
            })))
            .unwrap(),
        )
        .unwrap();
        let server = server(&imported);
        assert_eq!(server.sslmode, SslMode::VerifyFull);
        assert_eq!(server.root_certificate.as_deref(), Some("/c/ca.pem"));
        assert!(
            imported
                .notes
                .contains(&"client certificate left off: DBDelve doesn't send one".to_string())
        );
    }

    #[test]
    fn a_mongo_host_that_is_a_connection_string_is_read_as_one() {
        let imported = import(
            "m".into(),
            &Row::deserialize(&mongo_row(json!({
                "DatabaseHost": "mongodb+srv://cluster0.example.mongodb.net/app?authSource=admin",
                "DatabaseUser": "alice", "isOverSSH": true, "ServerAddress": "bastion",
                "ServerPasswordMode": 2,
            })))
            .unwrap(),
        )
        .unwrap();
        let ConnectionConfig::MongoDb(config) = &imported.config else {
            panic!()
        };
        assert!(config.srv);
        assert_eq!(config.server.host, "cluster0.example.mongodb.net");
        assert_eq!(config.server.user, "alice");
        assert_eq!(config.server.sslmode, SslMode::VerifyFull);
        assert_eq!(config.options, "authSource=admin");
        assert_eq!(
            config.server.ssh.as_ref().map(|ssh| ssh.host.as_str()),
            Some("bastion")
        );
    }

    #[test]
    fn mongo_options_whose_format_is_unknown_are_said_and_a_sql_row_is_not_asked() {
        let mongo = import(
            "m".into(),
            &Row::deserialize(&mongo_row(json!({ "OtherOptions": [{ "k": "v" }] }))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            mongo.notes,
            ["other options left off: their format isn't known"]
        );
        assert!(
            imported("PostgreSQL", json!({ "OtherOptions": [1] }))
                .unwrap()
                .notes
                .is_empty()
        );
    }

    #[test]
    fn a_mongo_password_comes_from_the_keychain_like_any_other() {
        let report = read(
            vec![named(
                "m",
                "m",
                mongo_row(json!({ "Driver": "Mongo", "DatabasePasswordMode": 0 })),
            )],
            |account| {
                assert_eq!(account, "m_database");
                Ok(Some(b"s3cret".to_vec()))
            },
        );
        assert_eq!(server(&report.imported[0]).password, "s3cret");
    }

    #[test]
    fn a_key_kept_in_the_keychain_is_written_whether_pem_or_base64() {
        assert_eq!(
            read_key(KEY.into(), KEY),
            (Some("/keys/tableplus-a".into()), vec![])
        );
        let wrapped = STANDARD.encode(KEY);
        assert_eq!(
            read_key(format!("{wrapped}\n").into(), KEY),
            (Some("/keys/tableplus-a".into()), vec![])
        );
        let rsa = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(
            read_key(rsa.into(), rsa),
            (Some("/keys/tableplus-a".into()), vec![])
        );
    }

    #[test]
    fn a_stored_key_that_is_not_one_is_said() {
        for stored in [
            b"not a key".to_vec(),
            vec![0xff, 0xfe, 0x00],
            STANDARD.encode("not a key either").into_bytes(),
            b"-----BEGIN CERTIFICATE-----".to_vec(),
        ] {
            assert_eq!(
                read_key(stored, ""),
                (None, vec![UNREADABLE_KEY.to_string()])
            );
        }
    }

    #[test]
    fn a_key_that_could_not_be_written_is_said_and_left_off() {
        let mut report = Report::default();
        read_rows(
            vec![over_ssh("a", "id_ed25519", 1)],
            |_| Ok(Some(KEY.into())),
            |_, _| {
                Err("SSH key left off: it couldn't be written to /keys/tableplus-a: permission denied".into())
            },
            &mut report,
        );
        assert_eq!(identity_file(&report.imported[0]), None);
        assert_eq!(
            report.imported[0].notes,
            ["SSH key left off: it couldn't be written to /keys/tableplus-a: permission denied"]
        );
    }

    #[test]
    fn a_key_path_is_kept_and_a_missing_item_left_to_ssh() {
        let absolute = std::env::temp_dir().join("id_ed25519");
        let absolute = absolute.to_str().unwrap();
        let mut asked = Vec::new();
        let report = read(
            vec![
                over_ssh("a", absolute, 2),
                over_ssh("b", "~/.ssh/id_rsa", 2),
                over_ssh("c", "id_ed25519", 2),
            ],
            |account| {
                asked.push(account.to_string());
                Ok(None)
            },
        );
        assert_eq!(asked, ["c_private_key_data"]);
        let identities = report
            .imported
            .iter()
            .map(identity_file)
            .collect::<Vec<_>>();
        assert_eq!(identities, [Some(absolute), Some("~/.ssh/id_rsa"), None]);
        assert_eq!(report.imported[2].notes, [UNLOCATED_KEY]);
        assert!(report.notes.is_empty());
    }

    #[test]
    fn a_written_key_reaches_ssh_from_dbdelves_directory() {
        let path = std::env::temp_dir()
            .join("Application Support")
            .join("dbdelve")
            .join("ssh-keys")
            .join("tableplus-a");
        assert_eq!(SshTunnel::identity_file_error(path.to_str().unwrap()), None);
    }

    #[test]
    fn drivers_map_to_engines_whatever_their_case() {
        let engine =
            |driver: &str| imported(driver, json!({})).map(|imported| imported.config.engine());
        for driver in [
            "PostgreSQL",
            "postgresql",
            "Cockroach",
            "Greenplum",
            "REDSHIFT",
        ] {
            assert_eq!(engine(driver), Ok(Engine::Postgres), "{driver}");
        }
        assert_eq!(engine("MySQL"), Ok(Engine::MySql));
        assert_eq!(engine("MariaDB"), Ok(Engine::MariaDb));
        assert_eq!(engine("sqlite"), Ok(Engine::Sqlite));
        for driver in ["MicrosoftSQLServer", "SQLServer", "Microsoft SQL Server"] {
            assert_eq!(engine(driver), Ok(Engine::SqlServer), "{driver}");
        }
        assert_eq!(
            engine("Snowflake"),
            Err("DBDelve's Snowflake signs in with a key file only".into())
        );
        assert_eq!(engine("Oracle"), Err("Oracle isn't supported".into()));
        assert_eq!(
            engine("ClickHouse"),
            Err("ClickHouse isn't supported".into())
        );
    }

    #[test]
    fn a_server_connection_keeps_its_login_and_no_tunnel() {
        let postgres = imported("PostgreSQL", json!({ "DatabasePort": "5433" })).unwrap();
        assert_eq!(
            postgres.config,
            ConnectionConfig::Postgres(ServerConfig {
                host: "db.example.com".into(),
                port: Some(5433),
                database: "app".into(),
                user: "alice@example.com".into(),
                ..ServerConfig::default()
            })
        );
        assert!(postgres.notes.is_empty());
        assert_eq!(
            imported("PostgreSQL", json!({ "DatabaseHost": "" })).map(|_| ()),
            Err("it has no host".into())
        );
    }

    #[test]
    fn tls_indexes_read_each_drivers_own_dropdown() {
        let modes = |driver: &str, count: i64| {
            (0..count)
                .map(|index| {
                    server(&imported(driver, json!({ "tLSMode": index })).unwrap()).sslmode
                })
                .collect::<Vec<_>>()
        };
        use SslMode::*;
        assert_eq!(
            modes("PostgreSQL", 6),
            [Prefer, Disable, Require, Prefer, VerifyCa, VerifyFull]
        );
        assert_eq!(modes("Redshift", 6), modes("PostgreSQL", 6));
        assert_eq!(
            modes("MySQL", 5),
            [Prefer, Disable, Require, VerifyCa, VerifyFull]
        );
        assert_eq!(modes("MariaDB", 3), [Prefer, Require, VerifyFull]);
        assert_eq!(
            modes("MicrosoftSQLServer", 2),
            [SslMode::default(), VerifyFull]
        );

        for (driver, index) in [
            ("PostgreSQL", 6),
            ("MySQL", 5),
            ("MariaDB", 3),
            ("MySQL", -1),
            ("MicrosoftSQLServer", 1),
        ] {
            let imported = imported(driver, json!({ "tLSMode": index })).unwrap();
            assert_eq!(server(&imported).sslmode, VerifyFull, "{driver} {index}");
            assert_eq!(
                imported.notes,
                [format!(
                    "SSL mode {index} isn't one DBDelve has, so it was set to verify-full"
                )]
            );
        }
    }

    #[test]
    fn the_ca_certificate_is_kept_where_checked_and_a_client_certificate_is_said() {
        let ssl = |tls_mode: i64, paths: [&str; 3]| {
            let imported = imported(
                "PostgreSQL",
                json!({ "tLSMode": tls_mode, "TlsKeyPaths": paths }),
            )
            .unwrap();
            (server(&imported).root_certificate.clone(), imported.notes)
        };
        assert_eq!(
            ssl(5, ["", "", "/certs/ca.pem"]),
            (Some("/certs/ca.pem".into()), vec![])
        );
        assert_eq!(
            ssl(2, ["", "", "/certs/ca.pem"]),
            (
                None,
                vec!["CA certificate left off: SSL mode require doesn't check one".to_string()]
            )
        );
        let client = vec!["client certificate left off: DBDelve doesn't send one".to_string()];
        assert_eq!(ssl(4, ["/k.pem", "", ""]), (None, client.clone()));
        assert_eq!(ssl(4, ["", "/c.pem", ""]), (None, client));
    }

    #[test]
    fn ssh_tunnels_come_in_only_where_ssh_can_open_them_unattended() {
        let tunnel = |uses_key: bool, key: &str, password_mode: i64| {
            let imported = imported(
                "PostgreSQL",
                json!({
                    "isOverSSH": true, "ServerAddress": "bastion", "ServerPort": "2222",
                    "ServerUser": "deploy", "isUsePrivateKey": uses_key,
                    "ServerPrivateKeyName": key, "ServerPasswordMode": password_mode,
                }),
            )
            .unwrap();
            (server(&imported).ssh.clone(), imported.notes)
        };
        let with_key = |identity_file: Option<&str>| {
            Some(SshTunnel {
                host: "bastion".into(),
                port: Some(2222),
                user: "deploy".into(),
                identity_file: identity_file.map(str::to_string),
            })
        };
        let unlocated = vec![
            "SSH key left off: its file couldn't be located, so ssh falls back to your ssh config and agent"
                .to_string(),
        ];

        assert_eq!(
            tunnel(true, "~/.ssh/id_ed25519", 0),
            (with_key(Some("~/.ssh/id_ed25519")), vec![])
        );
        assert_eq!(
            tunnel(true, "~/.ssh/id_rsa", 0),
            (with_key(Some("~/.ssh/id_rsa")), vec![])
        );
        assert_eq!(
            tunnel(true, "id_rsa", 0),
            (with_key(None), unlocated.clone())
        );
        assert_eq!(
            tunnel(true, "Import a private key...", 0),
            (with_key(None), unlocated)
        );
        assert_eq!(tunnel(false, "", 2), (with_key(None), vec![]));
        for password_mode in [0, 1] {
            assert_eq!(
                tunnel(false, "", password_mode),
                (
                    None,
                    vec!["SSH tunnel left off: it logs in with a password".to_string()]
                )
            );
        }
        let off = imported("PostgreSQL", json!({ "ServerAddress": "bastion" })).unwrap();
        assert_eq!((server(&off).ssh.clone(), off.notes), (None, vec![]));
    }

    #[test]
    fn the_environment_picks_the_color() {
        let color = |environment: &str| {
            imported("PostgreSQL", json!({ "Enviroment": environment }))
                .unwrap()
                .color
        };
        assert_eq!(color("production"), Some(ConnectionColor::Red));
        assert_eq!(color("staging"), Some(ConnectionColor::Yellow));
        assert_eq!(color("testing"), Some(ConnectionColor::Blue));
        assert_eq!(color("Development"), Some(ConnectionColor::Green));
        assert_eq!(color("local"), None);
        assert_eq!(color(""), None);
    }

    #[test]
    fn a_port_that_is_not_one_is_left_blank_and_said() {
        let imported = imported(
            "PostgreSQL",
            json!({
                "DatabasePort": "54x2", "isOverSSH": true, "ServerAddress": "bastion",
                "ServerPort": "0", "ServerPasswordMode": 2,
            }),
        )
        .unwrap();
        assert_eq!(server(&imported).port, None);
        assert_eq!(server(&imported).ssh.as_ref().unwrap().port, None);
        assert_eq!(
            imported.notes,
            [
                "port 54x2 isn't a port number, so it was left blank",
                "SSH port 0 isn't a port number, so it was left blank",
            ]
        );
    }

    #[test]
    fn sqlite_reads_its_path_and_falls_back_to_the_host() {
        let path = |extra: Value| imported("SQLite", extra).map(|imported| imported.config);
        let sqlite = |path: &str| ConnectionConfig::Sqlite {
            path: path.into(),
            statement_timeout: 0,
        };
        assert_eq!(
            path(json!({ "DatabasePath": "/data/app.db" })),
            Ok(sqlite("/data/app.db"))
        );
        assert_eq!(
            path(json!({ "DatabaseHost": "/data/host.db" })),
            Ok(sqlite("/data/host.db"))
        );
        assert_eq!(
            path(json!({ "DatabaseHost": "" })),
            Err("it has no database file".into())
        );
    }

    #[test]
    fn only_a_keychain_password_mode_looks_one_up() {
        let mut asked = Vec::new();
        let report = read(
            vec![
                named("a", "keychain", json!({ "DatabasePasswordMode": 0 })),
                named("b", "ask", json!({ "DatabasePasswordMode": 1 })),
                named("c", "none", json!({ "DatabasePasswordMode": 2 })),
                named("d", "missing", json!({ "DatabasePasswordMode": 0 })),
                named(
                    "e",
                    "file",
                    json!({ "Driver": "SQLite", "DatabasePath": "/a.db", "DatabasePasswordMode": 0 }),
                ),
            ],
            |account| {
                asked.push(account.to_string());
                Ok((account == "a_database").then(|| b"s3cret".to_vec()))
            },
        );
        assert_eq!(asked, ["a_database", "d_database"]);
        let passwords = report
            .imported
            .iter()
            .filter_map(|imported| imported.config.server())
            .map(|server| server.password.as_str())
            .collect::<Vec<_>>();
        assert_eq!(passwords, ["s3cret", "", "", ""]);
        assert!(report.notes.is_empty());
    }

    #[test]
    fn a_refused_keychain_is_asked_once_and_said_once() {
        let mut asked = 0;
        let report = read(
            ["a", "b", "c"]
                .into_iter()
                .map(|id| named(id, id, json!({ "DatabasePasswordMode": 0 })))
                .collect(),
            |_| {
                asked += 1;
                Err("denied".into())
            },
        );
        assert_eq!(asked, 1);
        assert_eq!(report.imported.len(), 3);
        assert!(
            report
                .imported
                .iter()
                .all(|imported| server(imported).password.is_empty())
        );
        assert_eq!(report.notes, [UNREADABLE_PASSWORDS]);
    }

    #[test]
    fn a_refused_key_lookup_stops_every_lookup_after_it() {
        for (first, expected) in [
            (over_ssh("a", "id_ed25519", 1), "a_private_key_data"),
            (over_ssh("a", "id_ed25519", 0), "a_database"),
        ] {
            let mut asked = Vec::new();
            let report = read(
                vec![
                    first,
                    over_ssh("b", "id_ed25519", 0),
                    over_ssh("c", "id_rsa", 1),
                ],
                |account| {
                    asked.push(account.to_string());
                    Err("denied".into())
                },
            );
            assert_eq!(asked, [expected]);
            assert_eq!(report.notes, [UNREADABLE_PASSWORDS]);
            for imported in &report.imported {
                assert_eq!(identity_file(imported), None);
                assert_eq!(imported.notes, [UNLOCATED_KEY]);
            }
        }
    }

    #[test]
    fn a_connection_in_both_builds_comes_in_once() {
        let direct = vec![
            named("a", "alpha", json!({})),
            named("b", "beta", json!({})),
        ];
        let setapp = vec![
            named("a", "alpha again", json!({})),
            named("c", "gamma", json!({})),
        ];
        let report = read(direct.into_iter().chain(setapp).collect(), |_| Ok(None));
        let names = report
            .imported
            .iter()
            .map(|imported| imported.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn one_malformed_row_is_skipped_and_the_rest_come_in() {
        let report = read(
            vec![
                named("a", "alpha", json!({})),
                json!("not a connection"),
                named("b", "odd", json!({ "isOverSSH": "maybe" })),
                named("c", "gamma", json!({})),
                named("d", "", json!({ "Driver": "Vertica" })),
            ],
            |_| Ok(None),
        );
        let names = report
            .imported
            .iter()
            .map(|imported| imported.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "gamma"]);
        let skipped = report
            .skipped
            .iter()
            .map(|skipped| skipped.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(skipped, ["A TablePlus connection", "odd", "d"]);
        assert!(report.skipped[1].reason.starts_with("could not be read: "));
        assert_eq!(report.skipped[2].reason, "Vertica isn't supported");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_reads_a_plist_and_refuses_a_date() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-tableplus-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let plist = |value: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><dict>
<key>ID</key><string>a</string><key>isOverSSH</key><false/><key>When</key>{value}
</dict></array></plist>"#
            )
        };
        let plain = directory.join("plain.plist");
        let dated = directory.join("dated.plist");
        std::fs::write(&plain, plist("<string>x</string>")).unwrap();
        std::fs::write(&dated, plist("<date>2026-01-01T00:00:00Z</date>")).unwrap();
        let rows = plist_rows(&plain);
        let refused = plist_rows(&dated);
        _ = std::fs::remove_dir_all(&directory);

        assert_eq!(
            rows,
            Ok(vec![json!({ "ID": "a", "isOverSSH": false, "When": "x" })])
        );
        let refused = refused.unwrap_err();
        assert!(
            refused.starts_with(&format!(
                "TablePlus's {} could not be read: ",
                dated.display()
            )),
            "{refused}"
        );
    }
}

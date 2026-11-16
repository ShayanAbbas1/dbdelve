//! Import from DBDelve, which only a nightly offers: a stable install's data
//! directory copied into a staging directory beside this build's and swapped
//! in whole, then its saved passwords copied across in the keychain.
//!
//! Everything here takes its directories and services as arguments, so the
//! tests run it on a stable build against directories of their own.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{PROFILES_FILE, data_root, passphrase_service, variant_name, variant_of, write_file};
use crate::channel::Channel;

/// What a data directory keeps that is the user's. Logs live elsewhere, and
/// anything else in it is a leftover, such as a `profiles.toml.broken`.
const COPIED: [&str; 7] = [
    PROFILES_FILE,
    "queries",
    "grids",
    "diagrams",
    "themes",
    "ssh-keys",
    "plugins",
];

/// Stable's data directory, whatever variant this build runs as.
pub(crate) fn source() -> Result<PathBuf, String> {
    Ok(data_root()?.join(variant_of(Channel::Stable, None)?))
}

/// Where the data an import replaces is kept.
pub(crate) fn backup_of(target: &Path) -> PathBuf {
    beside(target, ".before-import")
}

fn staging_of(target: &Path) -> PathBuf {
    beside(target, ".importing")
}

fn discard_of(target: &Path) -> PathBuf {
    beside(target, ".discarded")
}

fn beside(directory: &Path, suffix: &str) -> PathBuf {
    let mut name = directory.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    directory.with_file_name(name)
}

/// A connection a profiles file lists, by the account its keychain items are
/// filed under and the name the user knows it by.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Listed {
    pub(crate) id: String,
    #[serde(default)]
    name: String,
}

impl Listed {
    pub(crate) fn name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// Only the ids and names, so a profile on an engine this build can't open is
/// listed as well.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    profiles: Vec<Listed>,
}

/// Every connection `directory` lists. None for a profiles file that is
/// missing or doesn't read.
pub(crate) fn listed(directory: &Path) -> Vec<Listed> {
    fs::read_to_string(directory.join(PROFILES_FILE))
        .ok()
        .and_then(|text| toml::from_str::<Listing>(&text).ok())
        .map_or_else(Vec::new, |listing| listing.profiles)
}

/// Whether an import would lose anything in `directory`: a connection, a
/// profiles file it can't read, or anything else at all. A first launch writes
/// a profiles file with settings and no connection, and that is all a
/// directory with nothing to lose holds.
pub(crate) fn holds_data(directory: &Path) -> bool {
    match fs::read_to_string(directory.join(PROFILES_FILE)) {
        Ok(text) => match toml::from_str::<Listing>(&text) {
            Ok(listing) if listing.profiles.is_empty() => {}
            _ => return true,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return true,
    }
    match fs::read_dir(directory) {
        Ok(entries) => entries.flatten().any(|entry| {
            let name = entry.file_name();
            name != PROFILES_FILE && !in_flight(&name)
        }),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// A write's temporary: `write_file` names its own `<stem>.tmp-<pid>-<n>` and
/// `write_private_key` its `<name>.tmp`. Nothing reads one, and one the other
/// app is writing may be half there.
fn in_flight(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension == "tmp" || extension.starts_with("tmp-"))
}

/// An import copied and waiting beside the directory it replaces.
#[derive(Debug)]
pub(crate) struct Staged {
    directory: PathBuf,
    target: PathBuf,
    /// What the copied profiles file lists, whose saved passwords come next.
    pub(crate) connections: Vec<Listed>,
}

/// Copies what `source` keeps into a fresh directory beside `target`, with
/// every path in its profiles file that pointed into `source` pointed at the
/// same place in `target`. `source` is only read: DBDelve may be running, and
/// each of its writes replaces a file whole, so every file copied is one
/// version of it or the next.
pub(crate) fn stage(source: &Path, target: &Path) -> Result<Staged, String> {
    if source == target {
        return Err("This is DBDelve's own data directory.".into());
    }
    let staging = staging_of(target);
    // An earlier import that stopped half way leaves these behind, and nothing
    // else names them.
    for leftover in [&staging, &discard_of(target)] {
        remove_directory(leftover)?;
    }
    if let Err(message) = copy_into(source, target, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(message);
    }
    Ok(Staged {
        connections: listed(&staging),
        directory: staging,
        target: target.to_path_buf(),
    })
}

fn copy_into(source: &Path, target: &Path, staging: &Path) -> Result<(), String> {
    fs::create_dir_all(staging)
        .map_err(|error| format!("Could not create {}: {error}", staging.display()))?;
    for name in COPIED {
        let from = source.join(name);
        let to = staging.join(name);
        if name == PROFILES_FILE {
            match fs::read_to_string(&from) {
                Ok(text) => write_file(&to, &repointed(&text, source, target)?)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("Could not read {}: {error}", from.display())),
            }
        } else {
            copy_tree(&from, &to)?;
        }
    }
    Ok(())
}

/// A file or directory gone between being listed and being read is skipped:
/// a temporary renamed into place, or something DBDelve deleted. A directory
/// that is not there at all is one that install never made.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let gone = |error: &io::Error| error.kind() == io::ErrorKind::NotFound;
    for entry in walkdir::WalkDir::new(from).follow_links(true) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.io_error().is_some_and(gone) => continue,
            Err(error) => return Err(format!("Could not read DBDelve's data: {error}")),
        };
        let relative = entry
            .path()
            .strip_prefix(from)
            .expect("walkdir lists paths under its root");
        let destination = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir(&destination)
                .map_err(|error| format!("Could not create {}: {error}", destination.display()))?;
            #[cfg(unix)]
            if let Ok(metadata) = entry.metadata() {
                use std::os::unix::fs::PermissionsExt;
                // The owner keeps write access, or nothing more could be
                // copied into it; `ssh-keys` stays closed to everyone else.
                let mode = metadata.permissions().mode() | 0o700;
                let _ = fs::set_permissions(&destination, fs::Permissions::from_mode(mode));
            }
        } else if !in_flight(entry.file_name()) {
            match fs::copy(entry.path(), &destination) {
                Ok(_) => {}
                Err(error) if gone(&error) => {}
                Err(error) => {
                    return Err(format!(
                        "Could not copy {}: {error}",
                        entry.path().display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `text` with every string that is a path inside `from` moved to the same
/// place inside `to`. Spliced into the text rather than parsed and written
/// back, so whatever else is in the file comes across byte for byte.
fn repointed(text: &str, from: &Path, to: &Path) -> Result<String, String> {
    let document = toml::de::DeTable::parse(text)
        .map_err(|error| format!("Could not read DBDelve's {PROFILES_FILE}: {error}"))?;
    let mut edits = Vec::new();
    for (_, value) in document.get_ref() {
        moved_paths(value, from, to, &mut edits);
    }
    edits.sort_by_key(|(span, _)| span.start);
    let mut spliced = String::with_capacity(text.len());
    let mut at = 0;
    for (span, replacement) in edits {
        spliced.push_str(&text[at..span.start]);
        spliced.push_str(&replacement);
        at = span.end;
    }
    spliced.push_str(&text[at..]);
    Ok(spliced)
}

fn moved_paths(
    value: &toml::Spanned<toml::de::DeValue>,
    from: &Path,
    to: &Path,
    edits: &mut Vec<(Range<usize>, String)>,
) {
    match value.get_ref() {
        toml::de::DeValue::String(string) => {
            if let Ok(rest) = Path::new(string.as_ref()).strip_prefix(from) {
                let moved = match rest.as_os_str().is_empty() {
                    true => to.to_path_buf(),
                    false => to.join(rest),
                };
                let literal = toml::Value::String(moved.to_string_lossy().into_owned());
                edits.push((value.span(), literal.to_string()));
            }
        }
        toml::de::DeValue::Array(items) => {
            for item in items {
                moved_paths(item, from, to, edits);
            }
        }
        toml::de::DeValue::Table(table) => {
            for (_, item) in table {
                moved_paths(item, from, to, edits);
            }
        }
        _ => {}
    }
}

fn remove_directory(directory: &Path) -> Result<(), String> {
    match fs::remove_dir_all(directory) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("Could not remove {}: {error}", directory.display()))
        }
        _ => Ok(()),
    }
}

/// What a swap did with what was at the target.
#[derive(Debug, PartialEq)]
pub(crate) struct Swapped {
    /// Where the data that was there is kept.
    pub(crate) backup: Option<PathBuf>,
    /// Where what nobody needs went, to be deleted off the UI thread: a target
    /// with nothing to lose, or the backup an earlier import kept.
    pub(crate) discarded: Option<PathBuf>,
}

/// Puts `staged` where its target is. What was there goes aside first, kept as
/// `<target>.before-import` when it holds data (replacing an older one) and
/// put out of the way otherwise. Renames only, so the target is never half
/// there, and a rename that fails puts back the ones before it.
pub(crate) fn swap(staged: Staged) -> Result<Swapped, String> {
    let target = &staged.target;
    let backup = backup_of(target);
    let discard = discard_of(target);
    let exists = match fs::symlink_metadata(target) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("Could not read {}: {error}", target.display())),
    };
    let keep = exists && holds_data(target);
    let mut moves = Vec::new();
    if keep && fs::symlink_metadata(&backup).is_ok() {
        moves.push((backup.clone(), discard.clone()));
    }
    if exists {
        let aside = if keep { &backup } else { &discard };
        moves.push((target.clone(), aside.clone()));
    }
    moves.push((staged.directory.clone(), target.clone()));

    for (done, (from, to)) in moves.iter().enumerate() {
        if let Err(error) = fs::rename(from, to) {
            for (from, to) in moves[..done].iter().rev() {
                let _ = fs::rename(to, from);
            }
            let _ = fs::remove_dir_all(&staged.directory);
            return Err(format!(
                "Could not move {} to {}: {error}",
                from.display(),
                to.display()
            ));
        }
    }
    let discarded = moves
        .iter()
        .any(|(_, to)| *to == discard)
        .then_some(discard);
    Ok(Swapped {
        backup: keep.then_some(backup),
        discarded,
    })
}

/// The keychain as an import uses it: one item at a time, by service and
/// account.
pub(crate) trait Keychain {
    /// `None` for an item that isn't there.
    fn read(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String>;
}

/// Keychain Services, Secret Service or Credential Manager, as `keychain_entry`
/// reaches them. On macOS each read can raise a prompt, since the item belongs
/// to an app signed apart from this one: run it off the UI thread.
pub(crate) struct SystemKeychain;

impl Keychain for SystemKeychain {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(service, account).and_then(|entry| entry.get_password()) {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(service, account)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|error| error.to_string())
    }
}

/// The keychain services one build files a connection's password and its key
/// passphrase under.
#[derive(Clone, Debug)]
pub(crate) struct Services {
    password: String,
    passphrase: String,
}

impl Services {
    fn of(variant: &str) -> Self {
        Self {
            password: variant.to_string(),
            passphrase: passphrase_service(variant),
        }
    }

    pub(crate) fn stable() -> Result<Self, String> {
        Ok(Self::of(&variant_of(Channel::Stable, None)?))
    }

    pub(crate) fn this_build() -> Result<Self, String> {
        Ok(Self::of(&variant_name()?))
    }
}

/// Copies each connection's password and key passphrase from `from`'s
/// services to `to`'s, skipping any `from` doesn't have. Returns the names of
/// the connections with one that could not be copied.
pub(crate) fn copy_secrets(
    connections: &[Listed],
    from: &Services,
    to: &Services,
    keychain: &impl Keychain,
) -> Vec<String> {
    let mut failed: Vec<String> = Vec::new();
    for connection in connections {
        for (from, to) in [
            (&from.password, &to.password),
            (&from.passphrase, &to.passphrase),
        ] {
            let copied = keychain
                .read(from, &connection.id)
                .and_then(|secret| match secret {
                    Some(secret) => keychain.write(to, &connection.id, &secret),
                    None => Ok(()),
                });
            if let Err(error) = copied {
                tracing::warn!("a saved password could not be imported: {error}");
                let name = connection.name().to_string();
                if !failed.contains(&name) {
                    failed.push(name);
                }
            }
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    /// A directory of the test's own under the system's temporary one, gone
    /// with the value.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dbdelve-stable-import-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn profiles(source: &Path, ids: &[&str]) -> String {
        let key = source.join("ssh-keys").join("tableplus-a");
        let mut text = String::from("active = \"a\"\n\n[settings]\ntheme = \"Deep Sea\"\n");
        for id in ids {
            text.push_str(&format!(
                "\n[[profiles]]\nid = \"{id}\"\nname = \"Name {id}\"\nhost = \"db\"\n\
                 database = \"app\"\nuser = \"me\"\n\n[profiles.ssh]\nhost = \"bastion\"\n\
                 identity_file = {}\n",
                toml::Value::String(key.to_string_lossy().into_owned())
            ));
        }
        text
    }

    /// A stable data directory with one of everything an import copies, and
    /// the leftovers it doesn't.
    fn stable(root: &Path) -> PathBuf {
        let source = root.join("dbdelve");
        write(&source.join(PROFILES_FILE), &profiles(&source, &["a", "b"]));
        write(&source.join("queries/a/report.sql"), "select 1");
        write(&source.join("queries/a/.history.jsonl"), "{}\n");
        write(&source.join("queries/a/.scratch-0.sql"), "select 2");
        write(&source.join("queries/a/half.tmp-77-0"), "sel");
        write(&source.join("grids/a/users.json"), "{}");
        write(&source.join("diagrams/a/d-public.toml"), "x = 1");
        write(&source.join("themes/mine.json"), "{}");
        write(&source.join("ssh-keys/tableplus-a"), "KEY\n");
        write(
            &source.join("plugins/echo/dbdelve-plugin.toml"),
            "name = \"echo\"",
        );
        write(&source.join("profiles.toml.broken"), "nope");
        write(&source.join("app.log"), "log");
        source
    }

    fn file_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = walkdir::WalkDir::new(directory)
            .min_depth(1)
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                let relative = entry.path().strip_prefix(directory).unwrap();
                relative.to_string_lossy().replace('\\', "/")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_import_copies_what_dbdelve_keeps_and_leaves_its_leftovers() {
        let scratch = Scratch::new("copies");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        let before = file_names(&source);

        let staged = stage(&source, &target).unwrap();
        assert_eq!(
            staged
                .connections
                .iter()
                .map(Listed::name)
                .collect::<Vec<_>>(),
            ["Name a", "Name b"]
        );
        let swapped = swap(staged).unwrap();

        assert_eq!(
            swapped,
            Swapped {
                backup: None,
                discarded: None
            }
        );
        assert_eq!(
            file_names(&target),
            [
                "diagrams",
                "diagrams/a",
                "diagrams/a/d-public.toml",
                "grids",
                "grids/a",
                "grids/a/users.json",
                "plugins",
                "plugins/echo",
                "plugins/echo/dbdelve-plugin.toml",
                "profiles.toml",
                "queries",
                "queries/a",
                "queries/a/.history.jsonl",
                "queries/a/.scratch-0.sql",
                "queries/a/report.sql",
                "ssh-keys",
                "ssh-keys/tableplus-a",
                "themes",
                "themes/mine.json",
            ]
        );
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
        assert_eq!(read(&target.join("ssh-keys/tableplus-a")), "KEY\n");
        assert_eq!(file_names(&source), before, "the source is never written");
        assert_eq!(
            read(&source.join(PROFILES_FILE)),
            profiles(&source, &["a", "b"])
        );
        assert!(!staging_of(&target).exists());
    }

    #[test]
    fn paths_into_dbdelves_directory_point_into_this_ones_and_nothing_else_moves() {
        let from = Path::new("/home/me/.local/share/dbdelve");
        let to = Path::new("/home/me/.local/share/dbdelve-nightly");
        let text = "\
# kept as written
active = 'a'

[[profiles]]
id = \"a\"
path = '/home/me/.local/share/dbdelve/e2e.db'
root_certificate = \"/home/me/.local/share/dbdelve-dev/ca.pem\"
tags = [\"/home/me/.local/share/dbdelve\", 'x']

[profiles.ssh]
identity_file = \"/home/me/.local/share/dbdelve/ssh-keys/tableplus-a\"
port = 22
";
        assert_eq!(
            repointed(text, from, to).unwrap(),
            "\
# kept as written
active = 'a'

[[profiles]]
id = \"a\"
path = \"/home/me/.local/share/dbdelve-nightly/e2e.db\"
root_certificate = \"/home/me/.local/share/dbdelve-dev/ca.pem\"
tags = [\"/home/me/.local/share/dbdelve-nightly\", 'x']

[profiles.ssh]
identity_file = \"/home/me/.local/share/dbdelve-nightly/ssh-keys/tableplus-a\"
port = 22
"
        );
    }

    #[test]
    fn the_imported_profiles_point_at_the_keys_that_came_with_them() {
        let scratch = Scratch::new("keys");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");

        swap(stage(&source, &target).unwrap()).unwrap();

        let imported = read(&target.join(PROFILES_FILE));
        let file: toml::Table = toml::from_str(&imported).unwrap();
        let key = file["profiles"][0]["ssh"]["identity_file"]
            .as_str()
            .unwrap();
        assert_eq!(Path::new(key), target.join("ssh-keys").join("tableplus-a"));
        assert!(!imported.contains(&*source.join("ssh-keys").to_string_lossy()));
        assert_eq!(imported, profiles(&target, &["a", "b"]));
    }

    #[test]
    fn data_already_here_is_kept_beside_it_in_place_of_an_older_copy() {
        let scratch = Scratch::new("backup");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), &profiles(&target, &["mine"]));
        write(&target.join("queries/mine/q.sql"), "select 'mine'");
        let backup = backup_of(&target);
        write(&backup.join("old.txt"), "older import");

        let swapped = swap(stage(&source, &target).unwrap()).unwrap();

        assert_eq!(swapped.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(read(&backup.join("queries/mine/q.sql")), "select 'mine'");
        assert!(!backup.join("old.txt").exists());
        let discarded = swapped.discarded.expect("the older copy goes aside");
        assert_eq!(read(&discarded.join("old.txt")), "older import");
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
        assert!(!target.join("queries/mine").exists());
    }

    #[test]
    fn a_first_launchs_settings_alone_are_replaced_without_a_copy() {
        let scratch = Scratch::new("fresh");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "[settings]\nwelcomed = true\n");
        write(&target.join("profiles.tmp-9-9"), "");
        assert!(!holds_data(&target));

        let swapped = swap(stage(&source, &target).unwrap()).unwrap();

        assert_eq!(swapped.backup, None);
        let discarded = swapped.discarded.expect("the old directory goes aside");
        assert_eq!(
            read(&discarded.join(PROFILES_FILE)),
            "[settings]\nwelcomed = true\n"
        );
        assert!(!backup_of(&target).exists());
    }

    #[test]
    fn anything_but_a_first_launchs_settings_is_data_to_keep() {
        let scratch = Scratch::new("holds");
        let directory = scratch.0.join("dbdelve-nightly");
        assert!(!holds_data(&directory), "not there at all");
        fs::create_dir_all(&directory).unwrap();
        assert!(!holds_data(&directory));
        write(&directory.join(PROFILES_FILE), "not = [toml");
        assert!(holds_data(&directory), "a file that doesn't read");
        write(&directory.join(PROFILES_FILE), "[settings]\n");
        assert!(!holds_data(&directory));
        write(&directory.join("themes/mine.json"), "{}");
        assert!(holds_data(&directory), "a theme imported here");
        fs::remove_dir_all(directory.join("themes")).unwrap();
        write(
            &directory.join(PROFILES_FILE),
            &profiles(&directory, &["a"]),
        );
        assert!(holds_data(&directory), "a connection");
    }

    #[test]
    fn a_failed_import_leaves_this_directory_as_it_was() {
        let scratch = Scratch::new("failed");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        let mine = profiles(&target, &["mine"]);
        write(&target.join(PROFILES_FILE), &mine);

        write(&source.join(PROFILES_FILE), "not = [toml");
        assert!(stage(&source, &target).is_err());
        assert!(!staging_of(&target).exists());
        assert_eq!(read(&target.join(PROFILES_FILE)), mine);

        // The staged copy can't be moved in: the target's parent is gone by
        // the time it is, so the rename that puts it there fails.
        write(&source.join(PROFILES_FILE), &profiles(&source, &["a"]));
        let elsewhere = scratch.0.join("elsewhere");
        let staged = stage(&source, &target).unwrap();
        let moved = Staged {
            target: elsewhere.join("missing").join("dbdelve-nightly"),
            ..staged
        };
        assert!(swap(moved).is_err());
        assert!(!staging_of(&target).exists());
        assert_eq!(read(&target.join(PROFILES_FILE)), mine);
        assert!(!backup_of(&target).exists());
    }

    #[test]
    fn importing_a_directory_into_itself_is_refused() {
        let scratch = Scratch::new("itself");
        let source = stable(&scratch.0);
        assert!(stage(&source, &source).is_err());
        assert_eq!(read(&source.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn an_unreadable_listing_lists_nothing() {
        let scratch = Scratch::new("listing");
        assert!(listed(&scratch.0).is_empty());
        write(&scratch.0.join(PROFILES_FILE), "profiles = 3");
        assert!(listed(&scratch.0).is_empty());
    }

    #[derive(Default)]
    struct FakeKeychain {
        items: RefCell<HashMap<(String, String), String>>,
        unreadable: Vec<(String, String)>,
    }

    impl FakeKeychain {
        fn with(items: &[(&str, &str, &str)]) -> Self {
            Self {
                items: RefCell::new(
                    items
                        .iter()
                        .map(|(service, account, secret)| {
                            (
                                (service.to_string(), account.to_string()),
                                secret.to_string(),
                            )
                        })
                        .collect(),
                ),
                unreadable: Vec::new(),
            }
        }

        fn get(&self, service: &str, account: &str) -> Option<String> {
            self.items
                .borrow()
                .get(&(service.to_string(), account.to_string()))
                .cloned()
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            let key = (service.to_string(), account.to_string());
            if self.unreadable.contains(&key) {
                return Err("User canceled the operation.".into());
            }
            Ok(self.items.borrow().get(&key).cloned())
        }

        fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
            self.items.borrow_mut().insert(
                (service.to_string(), account.to_string()),
                secret.to_string(),
            );
            Ok(())
        }
    }

    fn connection(id: &str, name: &str) -> Listed {
        Listed {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn saved_passwords_and_passphrases_are_copied_to_this_builds_services() {
        let keychain = FakeKeychain::with(&[
            ("dbdelve", "a", "pw-a"),
            ("dbdelve/ssh-passphrase", "a", "pass-a"),
            ("dbdelve", "b", "pw-b"),
            ("dbdelve-dev", "a", "someone else's"),
        ]);
        let failed = copy_secrets(
            &[
                connection("a", "A"),
                connection("b", "B"),
                connection("c", "C"),
            ],
            &Services::of("dbdelve"),
            &Services::of("dbdelve-nightly"),
            &keychain,
        );

        assert!(failed.is_empty());
        assert_eq!(
            keychain.get("dbdelve-nightly", "a").as_deref(),
            Some("pw-a")
        );
        assert_eq!(
            keychain
                .get("dbdelve-nightly/ssh-passphrase", "a")
                .as_deref(),
            Some("pass-a")
        );
        assert_eq!(
            keychain.get("dbdelve-nightly", "b").as_deref(),
            Some("pw-b")
        );
        assert_eq!(keychain.get("dbdelve-nightly/ssh-passphrase", "b"), None);
        assert_eq!(keychain.get("dbdelve-nightly", "c"), None);
        assert_eq!(keychain.get("dbdelve", "a").as_deref(), Some("pw-a"));
    }

    #[test]
    fn a_password_that_cannot_be_read_names_its_connection_once_and_the_rest_still_copy() {
        let mut keychain = FakeKeychain::with(&[
            ("dbdelve", "a", "pw-a"),
            ("dbdelve/ssh-passphrase", "a", "pass-a"),
            ("dbdelve", "b", "pw-b"),
        ]);
        keychain.unreadable = vec![
            ("dbdelve".into(), "a".into()),
            ("dbdelve/ssh-passphrase".into(), "a".into()),
        ];
        let failed = copy_secrets(
            &[connection("a", "Prod"), connection("b", "")],
            &Services::of("dbdelve"),
            &Services::of("dbdelve-nightly"),
            &keychain,
        );

        assert_eq!(failed, ["Prod"]);
        assert_eq!(keychain.get("dbdelve-nightly", "a"), None);
        assert_eq!(
            keychain.get("dbdelve-nightly", "b").as_deref(),
            Some("pw-b")
        );
        assert_eq!(connection("b", "").name(), "b");
    }
}

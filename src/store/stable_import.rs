//! Import from DBDelve, which only a nightly offers, in two halves.
//!
//! The running app copies a stable install's data directory into
//! `<target>.importing`, marks the copy complete, and copies the saved
//! passwords across in the keychain. The next launch, before anything reads
//! the data directory, moves whatever is at the target to
//! `<target>.before-import` and the copy into its place. Nothing a running
//! session holds can be written over the import that way: the session saves
//! into what becomes the backup.
//!
//! Everything here takes its directories and services as arguments, so the
//! tests run it on a stable build against directories of their own.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{
    PROFILES_FILE, data_root, dbdelve_directory, passphrase_service, variant_name, variant_of,
    write_file,
};
use crate::channel::Channel;

/// The directories a data directory keeps that are the user's, beside its
/// profiles file. Logs live elsewhere, and anything else in it is a leftover,
/// such as a `profiles.toml.broken`.
const COPIED: [&str; 6] = [
    "queries", "grids", "diagrams", "themes", "ssh-keys", "plugins",
];

/// In a staging directory, says its copy is complete. Locked for as long as
/// the app that made it runs, so a launch beside it leaves it alone.
const MARKER: &str = ".import-complete";

/// How long a launch waits for the app that staged an import to finish
/// quitting. Every platform's restart already waits for the old process, so
/// this covers only an instance started by hand meanwhile.
const STAGER_EXIT_WAIT: Duration = Duration::from_secs(5);

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

/// A write's temporary: `write_file` names its own `<stem>.tmp-<pid>-<n>` and
/// `write_private_key` its `<name>.tmp`. Nothing reads one, and one the other
/// app is writing may be half there.
fn in_flight(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension == "tmp" || extension.starts_with("tmp-"))
}

/// An import copied in full beside the directory it replaces, waiting for the
/// next launch.
#[derive(Debug)]
pub(crate) struct Staged {
    /// What the copied profiles file lists, whose saved passwords come next.
    pub(crate) connections: Vec<Listed>,
    /// The marker, locked until this process exits; dropping it earlier lets
    /// another instance launched meanwhile move the import in under this one.
    _marker: File,
}

/// Copies what `source` keeps into a fresh directory beside `target`, with
/// the paths in its profiles file that pointed into a copied directory
/// pointed at the same place in `target`, and marks it complete. `source` is
/// only read: DBDelve may be running, and each of its writes replaces a file
/// whole, so every file copied is one version of it or the next.
pub(crate) fn stage(source: &Path, target: &Path) -> Result<Staged, String> {
    if source == target {
        return Err("This is DBDelve's own data directory.".into());
    }
    let staging = staging_of(target);
    // An earlier import that never finished copying leaves one behind, and a
    // complete one is replaced by this newer copy.
    remove_directory(&staging)?;
    let staged = copy_into(source, target, &staging).and_then(|()| {
        let path = staging.join(MARKER);
        File::create(&path)
            .and_then(|marker| marker.lock().map(|()| marker))
            .map_err(|error| format!("Could not create {}: {error}", path.display()))
    });
    match staged {
        Ok(marker) => Ok(Staged {
            connections: listed(&staging),
            _marker: marker,
        }),
        Err(message) => {
            let _ = fs::remove_dir_all(&staging);
            Err(message)
        }
    }
}

fn copy_into(source: &Path, target: &Path, staging: &Path) -> Result<(), String> {
    fs::create_dir_all(staging)
        .map_err(|error| format!("Could not create {}: {error}", staging.display()))?;
    let profiles = source.join(PROFILES_FILE);
    match fs::read_to_string(&profiles) {
        Ok(text) => write_file(
            &staging.join(PROFILES_FILE),
            &repointed(&text, source, target)?,
        )?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("Could not read {}: {error}", profiles.display()));
        }
    }
    for name in COPIED {
        copy_tree(&source.join(name), &staging.join(name))?;
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

/// `text` with every string that is a path inside one of the directories an
/// import copies moved to the same place in `to`. A path anywhere else, a
/// certificate or a database file kept beside them in `from` included, still
/// points where it did, since nothing copied it. Spliced into the text rather
/// than parsed and written back, so the rest of the file comes across byte
/// for byte.
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
            let inside_a_copy = Path::new(string.as_ref())
                .strip_prefix(from)
                .ok()
                .filter(|rest| {
                    rest.components().count() > 1
                        && rest.components().next().is_some_and(|first| {
                            COPIED.iter().any(|copied| first.as_os_str() == *copied)
                        })
                });
            if let Some(rest) = inside_a_copy {
                let literal = toml::Value::String(to.join(rest).to_string_lossy().into_owned());
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

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// What a launch found beside the data directory, and did about it.
#[derive(Debug, PartialEq)]
pub(crate) enum Settled {
    /// No import was waiting, and nothing was half done.
    Nothing,
    /// The import is the data directory now, and what was there is `backup`.
    Applied { backup: Option<PathBuf> },
    /// A copy that never finished was deleted.
    Abandoned,
    /// An import stopped between its two moves, with the data directory moved
    /// aside and nothing in its place; it was put back.
    RolledBack,
    /// The app that staged the import is still running.
    Waiting,
    /// A move failed. Whatever was moved went back, and the import waits for
    /// the next launch.
    Failed(String),
}

/// Finishes or undoes an import, deciding from what is on disk alone, so a
/// launch after a crash at any point picks up where it stopped:
/// - a complete staging copy moves in, the target (whatever it holds) going
///   to `<target>.before-import` first, replacing an older one;
/// - a staging copy without its marker is deleted;
/// - no target and no staging copy, but a backup, puts the backup back.
pub(crate) fn settle(target: &Path, wait: Duration) -> Settled {
    let staging = staging_of(target);
    let backup = backup_of(target);
    // Left in the data directory by a launch that stopped right after moving
    // the copy in. Nothing reads it, and a later import makes its own.
    let _ = fs::remove_file(target.join(MARKER));

    if !exists(&staging) {
        if exists(target) || !exists(&backup) {
            return Settled::Nothing;
        }
        return match fs::rename(&backup, target) {
            Ok(()) => Settled::RolledBack,
            Err(error) => Settled::Failed(format!(
                "Could not move {} back to {}: {error}",
                backup.display(),
                target.display()
            )),
        };
    }
    match stager_exited(&staging.join(MARKER), wait) {
        Ok(true) => {}
        Ok(false) => return Settled::Waiting,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return match remove_directory(&staging) {
                Ok(()) => Settled::Abandoned,
                Err(message) => Settled::Failed(message),
            };
        }
        Err(error) => {
            return Settled::Failed(format!("Could not read the import's marker: {error}"));
        }
    }

    let kept = exists(target);
    if kept {
        if let Err(message) = remove_directory(&backup) {
            return Settled::Failed(message);
        }
        if let Err(error) = fs::rename(target, &backup) {
            return Settled::Failed(format!(
                "Could not move {} to {}: {error}",
                target.display(),
                backup.display()
            ));
        }
    }
    if let Err(error) = fs::rename(&staging, target) {
        let moved = format!(
            "Could not move {} to {}: {error}",
            staging.display(),
            target.display()
        );
        if kept && let Err(error) = fs::rename(&backup, target) {
            // The next launch finds no target and a complete copy, and moves
            // it in then.
            return Settled::Failed(format!(
                "{moved}. {} could not be moved back either: {error}",
                backup.display()
            ));
        }
        return Settled::Failed(moved);
    }
    let _ = fs::remove_file(target.join(MARKER));
    // Kept by this launch, or by one that stopped between the two moves.
    Settled::Applied {
        backup: exists(&backup).then_some(backup),
    }
}

/// Whether the marker's lock is free, which it is once the app that took it
/// has exited, within `wait`. `NotFound` for a staging copy with no marker.
fn stager_exited(marker: &Path, wait: Duration) -> io::Result<bool> {
    let started = Instant::now();
    loop {
        // Closed before anything moves: Windows refuses to rename a directory
        // with a file open in it.
        let marker = fs::OpenOptions::new().read(true).write(true).open(marker)?;
        match marker.try_lock() {
            Ok(()) => return Ok(true),
            Err(fs::TryLockError::WouldBlock) if started.elapsed() < wait => {
                drop(marker);
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(false),
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// What the workspace says once it is up, for an import this launch
/// finished or undid.
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// [`settle`] for this build's data directory, at the very start of a
/// launch: before the plugins, the themes or the profiles are read from it.
/// A nightly's only, as only a nightly imports.
pub(crate) fn settle_at_launch() {
    if Channel::current() != Channel::Nightly {
        return;
    }
    let target = match dbdelve_directory() {
        Ok(target) => target,
        Err(error) => {
            tracing::warn!("no data directory to finish an import in: {error}");
            return;
        }
    };
    let notice = match settle(&target, STAGER_EXIT_WAIT) {
        Settled::Nothing => None,
        Settled::Applied { backup } => {
            tracing::info!("the import from DBDelve is in place");
            Some(match backup {
                Some(backup) => format!(
                    "Imported from DBDelve. What was here before is in {}.",
                    backup.display()
                ),
                None => "Imported from DBDelve.".to_string(),
            })
        }
        Settled::Abandoned => {
            tracing::info!("an import copy that never finished was deleted");
            None
        }
        Settled::RolledBack => {
            tracing::warn!("an import stopped half way, and the data before it was put back");
            Some(
                "An import from DBDelve didn't finish, so the data that was here before it \
                 was put back."
                    .to_string(),
            )
        }
        Settled::Waiting => {
            tracing::info!("the import waits for the app that copied it to quit");
            None
        }
        Settled::Failed(message) => {
            tracing::warn!("the import from DBDelve could not be put in place: {message}");
            Some(format!(
                "Could not finish importing from DBDelve. It is tried again at the next \
                 launch. {message}"
            ))
        }
    };
    *NOTICE.lock().unwrap_or_else(|poison| poison.into_inner()) = notice;
}

/// What [`settle_at_launch`] has to say, once.
pub(crate) fn take_notice() -> Option<String> {
    NOTICE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
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

    fn string(path: &Path) -> String {
        toml::Value::String(path.to_string_lossy().into_owned()).to_string()
    }

    fn profiles(directory: &Path, ids: &[&str]) -> String {
        let key = directory.join("ssh-keys").join("tableplus-a");
        let mut text = String::from("active = \"a\"\n\n[settings]\ntheme = \"Deep Sea\"\n");
        for id in ids {
            text.push_str(&format!(
                "\n[[profiles]]\nid = \"{id}\"\nname = \"Name {id}\"\nhost = \"db\"\n\
                 database = \"app\"\nuser = \"me\"\n\n[profiles.ssh]\nhost = \"bastion\"\n\
                 identity_file = {}\n",
                string(&key)
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

    /// A staging copy as an app that has since exited left it: complete, its
    /// marker no longer locked.
    fn staged_and_exited(source: &Path, target: &Path) {
        drop(stage(source, target).unwrap());
    }

    #[test]
    fn a_launch_moves_a_complete_copy_in_and_the_source_is_never_written() {
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
        assert!(!target.exists(), "nothing moves until the next launch");
        drop(staged);

        assert_eq!(
            settle(&target, Duration::ZERO),
            Settled::Applied { backup: None }
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
        assert_eq!(file_names(&source), before, "the source is never written");
        assert_eq!(
            read(&source.join(PROFILES_FILE)),
            profiles(&source, &["a", "b"])
        );
        assert!(!staging_of(&target).exists());
        assert_eq!(settle(&target, Duration::ZERO), Settled::Nothing);
    }

    #[test]
    fn whatever_is_here_is_kept_beside_it_in_place_of_an_older_copy() {
        let scratch = Scratch::new("backup");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        // No connection, only settings and a project: still kept.
        let mine = "[settings]\nwelcomed = true\n\n[[projects]]\nname = \"Work\"\n";
        write(&target.join(PROFILES_FILE), mine);
        let backup = backup_of(&target);
        write(&backup.join("old.txt"), "older import");

        staged_and_exited(&source, &target);
        assert_eq!(
            settle(&target, Duration::ZERO),
            Settled::Applied {
                backup: Some(backup.clone())
            }
        );

        assert_eq!(read(&backup.join(PROFILES_FILE)), mine);
        assert!(!backup.join("old.txt").exists());
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn a_copy_that_never_finished_is_deleted_and_nothing_else_moves() {
        let scratch = Scratch::new("unmarked");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        staged_and_exited(&source, &target);
        fs::remove_file(staging_of(&target).join(MARKER)).unwrap();

        assert_eq!(settle(&target, Duration::ZERO), Settled::Abandoned);
        assert!(!staging_of(&target).exists());
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
        assert!(!backup_of(&target).exists());
    }

    #[test]
    fn a_launch_beside_the_app_that_copied_the_import_leaves_it_alone() {
        let scratch = Scratch::new("waiting");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        let staged = stage(&source, &target).unwrap();

        assert_eq!(settle(&target, Duration::ZERO), Settled::Waiting);
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
        assert!(staging_of(&target).join(MARKER).exists());

        drop(staged);
        assert!(matches!(
            settle(&target, Duration::ZERO),
            Settled::Applied { .. }
        ));
    }

    #[test]
    fn a_launch_after_a_crash_between_the_moves_finishes_them() {
        let scratch = Scratch::new("between");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        staged_and_exited(&source, &target);
        fs::rename(&target, backup_of(&target)).unwrap();

        assert_eq!(
            settle(&target, Duration::ZERO),
            Settled::Applied {
                backup: Some(backup_of(&target))
            }
        );
        assert_eq!(read(&backup_of(&target).join(PROFILES_FILE)), "mine");
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn a_launch_after_a_crash_past_the_moves_drops_the_stray_marker() {
        let scratch = Scratch::new("past");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        staged_and_exited(&source, &target);
        fs::rename(staging_of(&target), &target).unwrap();

        assert_eq!(settle(&target, Duration::ZERO), Settled::Nothing);
        assert!(!target.join(MARKER).exists());
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn a_data_directory_moved_aside_with_nothing_in_its_place_is_put_back() {
        let scratch = Scratch::new("rollback");
        let target = scratch.0.join("dbdelve-nightly");
        write(&backup_of(&target).join(PROFILES_FILE), "mine");

        assert_eq!(settle(&target, Duration::ZERO), Settled::RolledBack);
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
        assert!(!backup_of(&target).exists());
    }

    #[test]
    fn a_kept_backup_beside_a_data_directory_stays_where_it_is() {
        let scratch = Scratch::new("settled");
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "imported");
        write(&backup_of(&target).join(PROFILES_FILE), "mine");

        assert_eq!(settle(&target, Duration::ZERO), Settled::Nothing);
        assert_eq!(read(&target.join(PROFILES_FILE)), "imported");
        assert_eq!(read(&backup_of(&target).join(PROFILES_FILE)), "mine");
        assert_eq!(
            settle(&scratch.0.join("never-made"), Duration::ZERO),
            Settled::Nothing
        );
    }

    #[test]
    fn only_paths_into_a_copied_directory_point_into_this_one() {
        let from = Path::new("/home/me/.local/share/dbdelve");
        let to = Path::new("/home/me/.local/share/dbdelve-nightly");
        let text = "\
# kept as written
active = 'a'

[[profiles]]
id = \"a\"
path = '/home/me/.local/share/dbdelve/local.sqlite'
root_certificate = \"/home/me/.local/share/dbdelve/ca.pem\"
other = \"/home/me/.local/share/dbdelve-dev/ssh-keys/k\"
tags = [\"/home/me/.local/share/dbdelve/themes/t.json\", \"/home/me/.local/share/dbdelve/themes\", 'x']

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
path = '/home/me/.local/share/dbdelve/local.sqlite'
root_certificate = \"/home/me/.local/share/dbdelve/ca.pem\"
other = \"/home/me/.local/share/dbdelve-dev/ssh-keys/k\"
tags = [\"/home/me/.local/share/dbdelve-nightly/themes/t.json\", \"/home/me/.local/share/dbdelve/themes\", 'x']

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

        staged_and_exited(&source, &target);
        settle(&target, Duration::ZERO);

        let imported = read(&target.join(PROFILES_FILE));
        let file: toml::Table = toml::from_str(&imported).unwrap();
        let key = file["profiles"][0]["ssh"]["identity_file"]
            .as_str()
            .unwrap();
        assert_eq!(Path::new(key), target.join("ssh-keys").join("tableplus-a"));
        assert_eq!(imported, profiles(&target, &["a", "b"]));
    }

    #[test]
    fn a_failed_copy_leaves_nothing_behind_and_this_directory_as_it_was() {
        let scratch = Scratch::new("failed");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        write(&source.join(PROFILES_FILE), "not = [toml");

        assert!(stage(&source, &target).is_err());
        assert!(!staging_of(&target).exists());
        assert_eq!(settle(&target, Duration::ZERO), Settled::Nothing);
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
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

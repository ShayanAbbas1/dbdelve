//! Import from DBDelve, which only a nightly offers, in two halves.
//!
//! The running app copies a stable install's data directory into
//! `<target>.importing` and marks the copy complete. The first launch after
//! that with no other nightly running, before anything reads the data
//! directory, moves whatever is at the target to a backup beside it and the
//! copy into its place, and once its window is up copies the saved passwords
//! across in the keychain. Nothing a running session holds can be written
//! over the import that way: a session saves into what becomes the backup.
//! And no password here is replaced for an import that never went in. No
//! backup is ever deleted here.
//!
//! Everything here takes its directories and services as arguments, so the
//! tests run it on a stable build against directories of their own.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use time::macros::format_description;

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

/// In a staging directory, says its copy is complete, and holds the name of
/// the backup the dialog promised.
const MARKER: &str = ".import-complete";

/// In a data directory, says the import now in it still has its saved
/// passwords to copy. The marker, renamed once the copy is in place.
const SECRETS_PENDING: &str = ".import-passwords-pending";

/// [`SECRETS_PENDING`], renamed by the one process that copies them, and
/// locked by it while it does.
const SECRETS_CLAIMED: &str = ".import-passwords-copying";

/// Stable's data directory, whatever variant this build runs as.
pub(crate) fn source() -> Result<PathBuf, String> {
    Ok(data_root()?.join(variant_of(Channel::Stable, None)?))
}

fn staging_of(target: &Path) -> PathBuf {
    beside(target, ".importing")
}

/// Held by whoever is copying an import. Beside the staging directory rather
/// than in it, so it is never moved or deleted with it.
fn staging_lock_of(target: &Path) -> PathBuf {
    beside(target, ".importing.lock")
}

/// Held shared by every running instance of this build, and alone by a
/// launch that settles an import. Beside the data directory, which moves.
fn instance_lock_of(target: &Path) -> PathBuf {
    beside(target, ".lock")
}

/// `<target>.before-import`, or when that is taken, the same with the time
/// after it (and a count, within one second): a backup is never replaced.
pub(crate) fn free_backup(target: &Path, now: SystemTime) -> PathBuf {
    let plain = beside(target, ".before-import");
    if !exists(&plain) {
        return plain;
    }
    let seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let stamp = time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|at| {
            at.format(format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .ok()
        })
        .unwrap_or_else(|| seconds.to_string());
    let stamped = beside(target, &format!(".before-import-{stamp}"));
    let mut candidate = stamped.clone();
    let mut count = 2;
    while exists(&candidate) {
        candidate = beside(target, &format!(".before-import-{stamp}-{count}"));
        count += 1;
    }
    candidate
}

/// The backups an import kept beside `target`, newest last: the timestamped
/// names sort after the plain one, and by time among themselves.
fn backups(target: &Path) -> Vec<PathBuf> {
    let Some(prefix) = target
        .file_name()
        .map(|name| format!("{}.before-import", name.to_string_lossy()))
    else {
        return Vec::new();
    };
    let Some(parent) = target.parent() else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
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
    listing(directory).unwrap_or_default()
}

/// Every connection `directory` lists, and an error for a profiles file that
/// is missing or doesn't read, where [`listed`] says none.
fn listing(directory: &Path) -> Result<Vec<Listed>, String> {
    let path = directory.join(PROFILES_FILE);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    toml::from_str::<Listing>(&text)
        .map(|listing| listing.profiles)
        .map_err(|_| format!("Could not read the profiles in {}", path.display()))
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
    /// Where the next launch moves the data here now, if there is any.
    pub(crate) backup: Option<PathBuf>,
    /// The staging lock, held until this process exits, so no other import
    /// starts over this one before a launch puts it in place.
    _lock: File,
}

/// Copies what `source` keeps into a fresh directory beside `target`, with
/// the paths in its profiles file that pointed into a copied directory
/// pointed at the same place in `target`, and marks it complete. `source` is
/// only read: DBDelve may be running, and each of its writes replaces a file
/// whole, so every file copied is one version of it or the next.
///
/// Refused while another import is copying or one is waiting for a restart.
pub(crate) fn stage(source: &Path, target: &Path, now: SystemTime) -> Result<Staged, String> {
    if source == target {
        return Err("This is DBDelve's own data directory.".into());
    }
    let staging = staging_of(target);
    let waiting = || exists(&staging.join(MARKER));
    if waiting() {
        return Err("An import from DBDelve is already waiting for a restart.".into());
    }
    let lock = locked(&staging_lock_of(target), try_alone).map_err(|error| match error.kind() {
        io::ErrorKind::WouldBlock => "An import from DBDelve is already copying.".to_string(),
        _ => format!("Could not start the import: {error}"),
    })?;
    // One that finished between the look above and the lock.
    if waiting() {
        return Err("An import from DBDelve is already waiting for a restart.".into());
    }
    // Copying takes the lock this process now holds, so a copy without its
    // marker belongs to no running process: one that stopped half way.
    remove_directory(&staging)?;
    let backup = exists(target).then(|| free_backup(target, now));
    let named = backup
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copied = copy_into(source, target, &staging)
        .and_then(|()| write_file(&staging.join(MARKER), &named));
    if let Err(message) = copied {
        let _ = fs::remove_dir_all(&staging);
        return Err(message);
    }
    Ok(Staged {
        connections: listed(&staging),
        backup,
        _lock: lock,
    })
}

/// An exclusive lock, or `WouldBlock` while anyone else holds one.
fn try_alone(file: &File) -> io::Result<()> {
    file.try_lock().map_err(io::Error::from)
}

/// `path` opened, created if need be, and locked by `lock`.
fn locked(path: &Path, lock: impl FnOnce(&File) -> io::Result<()>) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock(&file)?;
    Ok(file)
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
                        // `ssh-keys/../ca.pem` names a file no directory copied.
                        && !rest
                            .components()
                            .any(|component| component == Component::ParentDir)
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
    /// No data directory and no import, but a backup, which was put back.
    RolledBack { from: PathBuf },
    /// A move failed. Whatever was moved went back where it could, and the
    /// import waits for the next launch.
    Failed(String),
}

/// Finishes an import, deciding from what is on disk alone, so a launch after
/// a crash at any point picks up where it stopped. Only for a launch with no
/// other instance running (see [`settle_alone`]), so every staging copy it
/// finds belongs to a process that has exited:
/// - a copy with its marker moves in, the target (whatever it holds) moving
///   first to the backup the marker names, or to a free name beside it;
/// - a copy without its marker is deleted;
/// - no target and no copy, but a backup, puts the newest backup back.
///
/// Nothing deletes a backup, so no failure here can lose one.
pub(crate) fn settle(target: &Path, now: SystemTime) -> Settled {
    let staging = staging_of(target);
    // Left in the data directory by a launch that stopped right after moving
    // the copy in, whose passwords then still wait.
    secrets_now_pending(target);

    if !exists(&staging) {
        if exists(target) {
            return Settled::Nothing;
        }
        let Some(newest) = backups(target).pop() else {
            return Settled::Nothing;
        };
        return match fs::rename(&newest, target) {
            Ok(()) => Settled::RolledBack { from: newest },
            Err(error) => Settled::Failed(format!(
                "Could not move {} back to {}: {error}",
                newest.display(),
                target.display()
            )),
        };
    }
    let promised = match fs::read_to_string(staging.join(MARKER)) {
        Ok(name) => name,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return match remove_directory(&staging) {
                Ok(()) => Settled::Abandoned,
                Err(message) => Settled::Failed(message),
            };
        }
        Err(error) => {
            return Settled::Failed(format!("Could not read the import's marker: {error}"));
        }
    };
    // The one the dialog named, unless something has taken that name since.
    let promised = Some(promised.trim())
        .filter(|name| !name.is_empty() && Path::new(name).file_name() == Some(OsStr::new(name)))
        .map(|name| target.with_file_name(name));

    let mut backup = None;
    if exists(target) {
        let aside = promised
            .clone()
            .filter(|promised| !exists(promised))
            .unwrap_or_else(|| free_backup(target, now));
        if let Err(error) = fs::rename(target, &aside) {
            return Settled::Failed(format!(
                "Could not move {} to {}: {error}",
                target.display(),
                aside.display()
            ));
        }
        backup = Some(aside);
    }
    if let Err(error) = fs::rename(&staging, target) {
        let moved = format!(
            "Could not move {} to {}: {error}",
            staging.display(),
            target.display()
        );
        if let Some(aside) = &backup
            && let Err(error) = fs::rename(aside, target)
        {
            return Settled::Failed(format!(
                "{moved}. {} could not be moved back either: {error}",
                aside.display()
            ));
        }
        return Settled::Failed(moved);
    }
    secrets_now_pending(target);
    Settled::Applied {
        // A launch that stopped between the two moves made it.
        backup: backup.or(promised.filter(|promised| exists(promised))),
    }
}

/// The marker of an import moved in, turned into the note that its saved
/// passwords are still to copy. One that won't rename is tried again at the
/// next launch.
fn secrets_now_pending(target: &Path) {
    let marker = target.join(MARKER);
    if exists(&marker)
        && let Err(error) = fs::rename(&marker, target.join(SECRETS_PENDING))
    {
        tracing::warn!("the import's passwords could not be marked as still to copy: {error}");
    }
}

/// The saved passwords of the import in place, which this process alone
/// copies. Dropped without [`copy_pending_secrets`], it leaves them for the
/// next launch.
#[derive(Debug)]
pub(crate) struct SecretsClaim {
    /// The connections the import brought, read before the workspace can
    /// move a profiles file that doesn't read aside.
    pub(crate) connections: Vec<Listed>,
    claim: PathBuf,
    /// Held while the copy runs, which tells a later launch a claim is live.
    _lock: File,
}

/// Puts back a claim whose copy stopped with its process, then claims
/// whatever is pending. Run only while alone, so nothing else claims or puts
/// back meanwhile. A profile list that doesn't read leaves them pending.
fn claim_secrets(target: &Path) -> Option<SecretsClaim> {
    let pending = target.join(SECRETS_PENDING);
    let claim = target.join(SECRETS_CLAIMED);
    let abandoned = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&claim)
        .is_ok_and(|file| file.try_lock().is_ok());
    if abandoned {
        // The handle is closed by now: Windows renames no open file.
        if let Err(error) = fs::rename(&claim, &pending) {
            tracing::warn!("a stopped copy of the import's passwords was not put back: {error}");
        }
    }
    if !exists(&pending) {
        return None;
    }
    let connections = match listing(target) {
        Ok(connections) => connections,
        Err(message) => {
            tracing::warn!("the import's passwords wait, as its profiles don't read: {message}");
            return None;
        }
    };
    match fs::rename(&pending, &claim) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!("the import's passwords could not be claimed: {error}");
            return None;
        }
    }
    let lock = match locked(&claim, try_alone) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!("the import's passwords could not be claimed: {error}");
            let _ = fs::rename(&claim, &pending);
            return None;
        }
    };
    Some(SecretsClaim {
        connections,
        claim,
        _lock: lock,
    })
}

/// How a launch went, as far as an import is concerned.
#[derive(Debug)]
pub(crate) enum Launch {
    /// Alone, so whatever was waiting is settled, and this launch copies the
    /// saved passwords of an import in place whose passwords haven't been.
    Settled {
        settled: Settled,
        secrets: Option<SecretsClaim>,
    },
    /// Another instance is running, so nothing was looked at but whether an
    /// import is waiting for them all to close.
    Shared { waiting: bool },
}

/// Settles while no other instance is running, which the instance lock at
/// `lock` says, and touches nothing while one is. Returns the lock held
/// shared, for this process to keep while it runs.
pub(crate) fn settle_alone(
    lock: &Path,
    target: &Path,
    now: SystemTime,
) -> Result<(File, Launch), String> {
    let failed = |error: io::Error| format!("Could not lock {}: {error}", lock.display());
    match locked(lock, try_alone) {
        Ok(file) => {
            let settled = settle(target, now);
            let secrets = claim_secrets(target);
            // std leaves a lock taken over one already held unspecified, so
            // the exclusive one goes first. A launch that slips in between
            // finds this one settled and its passwords claimed, and its
            // shared lock waits for this one's.
            file.unlock().map_err(failed)?;
            file.lock_shared().map_err(failed)?;
            Ok((file, Launch::Settled { settled, secrets }))
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let file = locked(lock, File::lock_shared).map_err(failed)?;
            let waiting = exists(&staging_of(target).join(MARKER));
            Ok((file, Launch::Shared { waiting }))
        }
        Err(error) => Err(failed(error)),
    }
}

/// This process's hold on the instance lock, kept until it exits.
static INSTANCE: OnceLock<File> = OnceLock::new();

/// How long a launch tries for the instance lock before it starts without
/// one, and so without looking for an import.
const INSTANCE_LOCK_WAIT: Duration = Duration::from_secs(3);

/// The saved passwords this launch copies once its window is up.
static SECRETS_TO_COPY: Mutex<Option<SecretsClaim>> = Mutex::new(None);

/// What the workspace says once it is up, for an import this launch
/// finished, undid or had to leave waiting.
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// [`settle_alone`] for this build's data directory, at the very start of a
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
    let lock = instance_lock_of(&target);
    let started = Instant::now();
    let launch = loop {
        match settle_alone(&lock, &target, SystemTime::now()) {
            Ok((lock, launch)) => {
                _ = INSTANCE.set(lock);
                break launch;
            }
            Err(error) if started.elapsed() < INSTANCE_LOCK_WAIT => {
                tracing::debug!("the instance lock is not there yet: {error}");
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                tracing::warn!("an import was not looked for: {error}");
                return;
            }
        }
    };
    let launch = match launch {
        Launch::Settled { settled, secrets } => {
            *SECRETS_TO_COPY
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = secrets;
            Ok(settled)
        }
        Launch::Shared { waiting } => Err(waiting),
    };
    let notice = match launch {
        Err(false) | Ok(Settled::Nothing) => None,
        Err(true) => {
            tracing::info!("the import waits for every other instance to close");
            Some(format!(
                "The import from DBDelve is put in place once every {name} window is closed \
                 and it is opened again.",
                name = Channel::current().display_name()
            ))
        }
        Ok(Settled::Applied { backup }) => {
            tracing::info!("the import from DBDelve is in place");
            Some(match backup {
                Some(backup) => format!(
                    "Imported from DBDelve. What was here before is in {}.",
                    backup.display()
                ),
                None => "Imported from DBDelve.".to_string(),
            })
        }
        Ok(Settled::Abandoned) => {
            tracing::info!("an import copy that never finished was deleted");
            None
        }
        Ok(Settled::RolledBack { from }) => {
            tracing::warn!("no data directory but a backup, which was put back");
            Some(format!(
                "The data directory was missing, so the backup in {} was put back.",
                from.display()
            ))
        }
        Ok(Settled::Failed(message)) => {
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

/// A copy with no connections in it, claimed in `directory` and handed to
/// the next workspace to start, as a launch that put an import in place
/// hands one over. None of the keychain is read.
#[cfg(test)]
pub(crate) fn hand_over_an_empty_copy(directory: &Path) {
    write_file(&directory.join(PROFILES_FILE), "").unwrap();
    write_file(&directory.join(SECRETS_PENDING), "").unwrap();
    let claim = claim_secrets(directory).expect("an empty list is claimed");
    assert!(claim.connections.is_empty());
    *SECRETS_TO_COPY
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(claim);
}

/// The saved passwords this launch claimed, for [`copy_pending_secrets`],
/// once.
pub(crate) fn take_secrets_to_copy() -> Option<SecretsClaim> {
    SECRETS_TO_COPY
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
}

/// The saved passwords of the connections the import brought, copied from
/// `from`'s services to `to`'s, after which none are pending. Returns the
/// names of the connections one didn't come across for.
pub(crate) fn copy_pending_secrets(
    secrets: SecretsClaim,
    from: &Services,
    to: &Services,
    keychain: &impl Keychain,
) -> Vec<String> {
    let failed = copy_secrets(&secrets.connections, from, to, keychain);
    // Removed while still locked, so no launch takes it for a stopped copy.
    if let Err(error) = fs::remove_file(&secrets.claim) {
        tracing::warn!("the import's passwords are copied but still marked as to copy: {error}");
    }
    failed
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
            Err(error) => Err(kind_of(&error).to_string()),
        }
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(service, account)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|error| kind_of(&error).to_string())
    }
}

/// What went wrong, and nothing else: a keyring error's own text can carry
/// the account, which is named after the connection, and the `Ambiguous` one
/// the matching items themselves.
fn kind_of(error: &keyring::Error) -> &'static str {
    match error {
        keyring::Error::PlatformFailure(_) => "the keychain failed",
        keyring::Error::NoStorageAccess(_) => "the keychain could not be opened",
        keyring::Error::NoEntry => "no saved item",
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            "the saved item could not be read"
        }
        keyring::Error::BadStoreFormat(_) => "the keychain could not be read",
        keyring::Error::TooLong(..) => "a name is too long for the keychain",
        keyring::Error::Invalid(..) => "the keychain refused the item",
        keyring::Error::Ambiguous(_) => "more than one saved item matched",
        _ => "an unknown keychain error",
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

    /// A fixed moment, so a backup's name is the same on every run.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_763_300_000)
    }

    fn plain_backup(target: &Path) -> PathBuf {
        beside(target, ".before-import")
    }

    /// A staging copy as an app that has since exited left it: complete, and
    /// no longer locked.
    fn staged_and_exited(source: &Path, target: &Path) -> Option<PathBuf> {
        stage(source, target, now()).unwrap().backup
    }

    #[test]
    fn a_launch_moves_a_complete_copy_in_and_the_source_is_never_written() {
        let scratch = Scratch::new("copies");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        let before = file_names(&source);

        let staged = stage(&source, &target, now()).unwrap();
        assert_eq!(
            staged
                .connections
                .iter()
                .map(Listed::name)
                .collect::<Vec<_>>(),
            ["Name a", "Name b"]
        );
        assert_eq!(staged.backup, None, "nothing here to keep");
        assert!(!target.exists(), "nothing moves until the next launch");
        drop(staged);

        assert_eq!(settle(&target, now()), Settled::Applied { backup: None });
        assert_eq!(
            file_names(&target),
            [
                ".import-passwords-pending",
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
        assert_eq!(file_names(&source), before, "the source is never written");
        assert_eq!(
            read(&source.join(PROFILES_FILE)),
            profiles(&source, &["a", "b"])
        );
        assert!(!staging_of(&target).exists());
        assert_eq!(settle(&target, now()), Settled::Nothing);
    }

    #[test]
    fn whatever_is_here_goes_to_the_backup_the_dialog_named_and_an_older_one_stays() {
        let scratch = Scratch::new("backup");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        // No connection, only settings and a project: still kept.
        let mine = "[settings]\nwelcomed = true\n\n[[projects]]\nname = \"Work\"\n";
        write(&target.join(PROFILES_FILE), mine);
        write(&plain_backup(&target).join("old.txt"), "older import");

        let named = staged_and_exited(&source, &target).expect("there is data to keep");
        assert_ne!(named, plain_backup(&target));
        assert!(
            named
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("dbdelve-nightly.before-import-")
        );
        assert_eq!(
            settle(&target, now()),
            Settled::Applied {
                backup: Some(named.clone())
            }
        );

        assert_eq!(read(&named.join(PROFILES_FILE)), mine);
        assert_eq!(read(&plain_backup(&target).join("old.txt")), "older import");
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn backups_taken_within_one_second_each_get_a_name_of_their_own() {
        let scratch = Scratch::new("names");
        let target = scratch.0.join("dbdelve-nightly");
        let first = free_backup(&target, now());
        assert_eq!(first, plain_backup(&target));
        fs::create_dir_all(&first).unwrap();
        let second = free_backup(&target, now());
        fs::create_dir_all(&second).unwrap();
        let third = free_backup(&target, now());
        assert_eq!(
            third,
            beside(
                &target,
                &format!(
                    "{}-2",
                    second
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .trim_start_matches("dbdelve-nightly")
                )
            )
        );
        assert_eq!(backups(&target), [first, second]);
    }

    #[test]
    fn a_copy_that_never_finished_is_deleted_and_nothing_else_moves() {
        let scratch = Scratch::new("unmarked");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        staged_and_exited(&source, &target);
        fs::remove_file(staging_of(&target).join(MARKER)).unwrap();

        assert_eq!(settle(&target, now()), Settled::Abandoned);
        assert!(!staging_of(&target).exists());
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
        assert!(backups(&target).is_empty());
    }

    #[test]
    fn a_launch_beside_a_running_instance_touches_nothing() {
        let scratch = Scratch::new("instances");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        let lock = instance_lock_of(&target);
        write(&target.join(PROFILES_FILE), "mine");
        staged_and_exited(&source, &target);
        let running = locked(&lock, File::lock_shared).unwrap();

        let (beside_it, launch) = settle_alone(&lock, &target, now()).unwrap();
        assert!(matches!(launch, Launch::Shared { waiting: true }));
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
        assert!(staging_of(&target).join(MARKER).exists());

        drop((running, beside_it));
        let (_alone, launch) = settle_alone(&lock, &target, now()).unwrap();
        assert!(matches!(
            launch,
            Launch::Settled {
                settled: Settled::Applied { backup: Some(_) },
                secrets: Some(_)
            }
        ));
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
        let (_beside, launch) = settle_alone(&lock, &target, now()).unwrap();
        assert!(matches!(launch, Launch::Shared { waiting: false }));
    }

    #[test]
    fn a_launch_after_a_crash_between_the_moves_finishes_them() {
        let scratch = Scratch::new("between");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "mine");
        let named = staged_and_exited(&source, &target).unwrap();
        fs::rename(&target, &named).unwrap();

        assert_eq!(
            settle(&target, now()),
            Settled::Applied {
                backup: Some(named.clone())
            }
        );
        assert_eq!(read(&named.join(PROFILES_FILE)), "mine");
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    #[test]
    fn a_launch_after_a_crash_past_the_moves_leaves_the_passwords_to_copy() {
        let scratch = Scratch::new("past");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        staged_and_exited(&source, &target);
        fs::rename(staging_of(&target), &target).unwrap();

        assert_eq!(settle(&target, now()), Settled::Nothing);
        assert!(!target.join(MARKER).exists());
        assert!(target.join(SECRETS_PENDING).exists());
        assert_eq!(read(&target.join("queries/a/report.sql")), "select 1");
    }

    /// A launch moved the data aside and then could neither move the import
    /// in nor the data back; the app then started on an empty directory.
    #[test]
    fn data_moved_aside_by_a_launch_that_failed_outlives_every_launch_after_it() {
        let scratch = Scratch::new("failed-moves");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        write(&plain_backup(&target).join("old.txt"), "older import");
        write(&target.join(PROFILES_FILE), "mine");
        let named = staged_and_exited(&source, &target).unwrap();
        fs::rename(&target, &named).unwrap();
        write(&target.join(PROFILES_FILE), "fresh");

        let Settled::Applied {
            backup: Some(fresh),
        } = settle(&target, now())
        else {
            panic!("the import goes in");
        };
        assert_eq!(read(&fresh.join(PROFILES_FILE)), "fresh");
        assert_eq!(read(&named.join(PROFILES_FILE)), "mine");
        assert_eq!(read(&plain_backup(&target).join("old.txt")), "older import");
        assert_eq!(backups(&target).len(), 3);
    }

    #[test]
    fn a_missing_data_directory_gets_the_newest_backup_back() {
        let scratch = Scratch::new("rollback");
        let target = scratch.0.join("dbdelve-nightly");
        write(&plain_backup(&target).join(PROFILES_FILE), "older");
        let newest = free_backup(&target, now());
        write(&newest.join(PROFILES_FILE), "newest");

        assert_eq!(
            settle(&target, now()),
            Settled::RolledBack {
                from: newest.clone()
            }
        );
        assert_eq!(read(&target.join(PROFILES_FILE)), "newest");
        assert_eq!(read(&plain_backup(&target).join(PROFILES_FILE)), "older");
    }

    #[test]
    fn a_backup_beside_a_data_directory_stays_where_it_is() {
        let scratch = Scratch::new("settled");
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(PROFILES_FILE), "imported");
        write(&plain_backup(&target).join(PROFILES_FILE), "mine");

        assert_eq!(settle(&target, now()), Settled::Nothing);
        assert_eq!(read(&target.join(PROFILES_FILE)), "imported");
        assert_eq!(read(&plain_backup(&target).join(PROFILES_FILE)), "mine");
        assert_eq!(
            settle(&scratch.0.join("never-made"), now()),
            Settled::Nothing
        );
    }

    #[test]
    fn one_import_at_a_time() {
        let scratch = Scratch::new("one");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");

        let copying = locked(&staging_lock_of(&target), try_alone).unwrap();
        assert_eq!(
            stage(&source, &target, now()).unwrap_err(),
            "An import from DBDelve is already copying."
        );
        drop(copying);

        let staged = stage(&source, &target, now()).unwrap();
        assert_eq!(
            stage(&source, &target, now()).unwrap_err(),
            "An import from DBDelve is already waiting for a restart."
        );
        drop(staged);
        assert_eq!(
            stage(&source, &target, now()).unwrap_err(),
            "An import from DBDelve is already waiting for a restart.",
            "a complete copy waits for a launch, whoever made it"
        );
        assert!(staging_of(&target).join(MARKER).exists());
    }

    #[test]
    fn only_paths_into_a_copied_directory_point_into_this_one() {
        let share = std::env::temp_dir().join("home").join("share");
        let from = share.join("dbdelve");
        let to = share.join("dbdelve-nightly");
        let database = from.join("local.sqlite");
        let certificate = from.join("ca.pem");
        let climbing = from.join("ssh-keys").join("..").join("ca.pem");
        let elsewhere = share.join("dbdelve-dev").join("ssh-keys").join("k");
        let theme = from.join("themes").join("t.json");
        let themes = from.join("themes");
        let key = from.join("ssh-keys").join("tableplus-a");
        let text = format!(
            "# kept as written\nactive = 'a'\n\n[[profiles]]\nid = \"a\"\npath = '{}'\n\
             root_certificate = {}\nclient_key = {}\nother = {}\ntags = [{}, {}, 'x']\n\n\
             [profiles.ssh]\nidentity_file = {}\nport = 22\n",
            database.display(),
            string(&certificate),
            string(&climbing),
            string(&elsewhere),
            string(&theme),
            string(&themes),
            string(&key),
        );

        let rewritten = repointed(&text, &from, &to).unwrap();
        assert!(rewritten.starts_with("# kept as written\nactive = 'a'\n"));
        let file: toml::Table = toml::from_str(&rewritten).unwrap();
        let profile = &file["profiles"][0];
        let path = |value: &toml::Value| PathBuf::from(value.as_str().unwrap());
        assert_eq!(path(&profile["path"]), database);
        assert_eq!(path(&profile["root_certificate"]), certificate);
        assert_eq!(path(&profile["client_key"]), climbing);
        assert_eq!(path(&profile["other"]), elsewhere);
        assert_eq!(path(&profile["tags"][0]), to.join("themes").join("t.json"));
        assert_eq!(path(&profile["tags"][1]), themes);
        assert_eq!(profile["tags"][2].as_str(), Some("x"));
        assert_eq!(
            path(&profile["ssh"]["identity_file"]),
            to.join("ssh-keys").join("tableplus-a")
        );
        assert_eq!(profile["ssh"]["port"].as_integer(), Some(22));
    }

    #[test]
    fn the_imported_profiles_point_at_the_keys_that_came_with_them() {
        let scratch = Scratch::new("keys");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");

        staged_and_exited(&source, &target);
        settle(&target, now());

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

        assert!(stage(&source, &target, now()).is_err());
        assert!(!staging_of(&target).exists());
        assert_eq!(settle(&target, now()), Settled::Nothing);
        assert_eq!(read(&target.join(PROFILES_FILE)), "mine");
    }

    #[test]
    fn importing_a_directory_into_itself_is_refused() {
        let scratch = Scratch::new("itself");
        let source = stable(&scratch.0);
        assert!(stage(&source, &source, now()).is_err());
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
    fn saved_passwords_are_copied_once_the_import_is_in_place_and_by_one_process() {
        let scratch = Scratch::new("secrets");
        let source = stable(&scratch.0);
        let target = scratch.0.join("dbdelve-nightly");
        let lock = instance_lock_of(&target);
        write(&target.join(PROFILES_FILE), "mine");
        let keychain = FakeKeychain::with(&[
            ("dbdelve", "a", "pw-a"),
            ("dbdelve-nightly", "a", "nightly's own"),
        ]);
        let alone = || match settle_alone(&lock, &target, now()).unwrap().1 {
            Launch::Settled { secrets, .. } => secrets,
            Launch::Shared { .. } => panic!("no other instance runs"),
        };

        staged_and_exited(&source, &target);
        let running = locked(&lock, File::lock_shared).unwrap();
        let (beside_it, launch) = settle_alone(&lock, &target, now()).unwrap();
        assert!(matches!(launch, Launch::Shared { waiting: true }));
        drop((running, beside_it));
        assert_eq!(
            keychain.get("dbdelve-nightly", "a").as_deref(),
            Some("nightly's own"),
            "nothing is copied while the import waits"
        );

        let first = alone().expect("the launch that puts it in place claims them");
        assert_eq!(
            first
                .connections
                .iter()
                .map(Listed::name)
                .collect::<Vec<_>>(),
            ["Name a", "Name b"]
        );
        assert!(
            alone().is_none(),
            "a launch while the first one copies leaves them to it"
        );
        drop(first);
        let again = alone().expect("a copy that stopped is claimed again");
        assert!(alone().is_none());

        let failed = copy_pending_secrets(
            again,
            &Services::of("dbdelve"),
            &Services::of("dbdelve-nightly"),
            &keychain,
        );
        assert!(failed.is_empty());
        assert_eq!(
            keychain.get("dbdelve-nightly", "a").as_deref(),
            Some("pw-a")
        );
        assert!(alone().is_none(), "copied once");
        assert!(!target.join(SECRETS_PENDING).exists());
        assert!(!target.join(SECRETS_CLAIMED).exists());
    }

    #[test]
    fn saved_passwords_wait_while_the_profiles_dont_read() {
        let scratch = Scratch::new("unread");
        let target = scratch.0.join("dbdelve-nightly");
        write(&target.join(SECRETS_PENDING), "");

        assert!(claim_secrets(&target).is_none(), "no profiles file");
        write(&target.join(PROFILES_FILE), "profiles = [");
        assert!(claim_secrets(&target).is_none(), "one that doesn't parse");
        assert!(target.join(SECRETS_PENDING).exists());

        write(&target.join(PROFILES_FILE), "[settings]\n");
        let claim = claim_secrets(&target).expect("an empty list reads");
        assert!(claim.connections.is_empty());
        assert!(!target.join(SECRETS_PENDING).exists());
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

use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};
#[cfg(windows)]
use std::{ffi::OsString, process::Stdio, sync::OnceLock};

use minisign_verify::PublicKey;

#[cfg(windows)]
use super::Progress;
#[cfg(windows)]
use super::download::{
    ALREADY_RUNNING, COULD_NOT_DOWNLOAD, NO_SCRATCH, NOT_SIGNED, Scratch, agent, download,
    fetch_signature, hold, lock, release_key, verify_contents, writable,
};
use super::{
    Asset,
    download::{COULD_NOT_REPLACE, COULD_NOT_UNPACK, NOT_THE_APP, failed, remove, unique},
    identity::{ALREADY_UPDATED, Identity, unchanged},
};

pub(super) const CANCELLED: &str = "The update was cancelled before anything was installed.";

const EXE: &str = "dbdelve.exe";

/// The ending of what an update moves out of the way: running executables
/// cannot be overwritten or deleted on Windows, only renamed. The installer's
/// `[Code]` in `assets/windows/dbdelve.iss` uses the same one, and both clear
/// what has it.
const SET_ASIDE: &str = ".dbdelve-old";

/// In a set-aside folder for as long as what it holds may be the only copy of
/// the installed release: written before anything moves into it, removed only
/// once the update has finished or every file is back. No clearing takes a
/// folder with it, so an update that stops part way -- a failed undo, a crash,
/// a power cut -- never loses the old files. Anything that could not be put
/// back is listed in it. The installer's `[Code]` writes and honours the same
/// one.
const UNRESTORED: &str = "dbdelve-unrestored.txt";

const PROTECTION: &str = "DBDelve moved its installed files here while updating. Until \
    the update finishes, or puts them back, this may be the only copy of them, so \
    nothing clears this folder while this file is in it. Anything a failed update \
    could not put back is listed below, with where it belongs.\r\n";

/// Where a portable update unpacks: inside the install, so its files move into
/// place by rename.
const STAGING: &str = "update.dbdelve-new";

/// Replaced as a whole, so nothing an older bridge had lingers beside a newer
/// one.
const PLUGIN_TREE: [&str; 2] = ["plugins", "odbc"];

/// The installer's AppId, which names its entry under Uninstall.
#[cfg(windows)]
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}_is1";

/// Setup's exit code when it stopped before installing anything, which is
/// also what a declined elevation prompt gives.
const SETUP_CANCELLED: i32 = 2;

/// Everything an install needs, found when the release was: having one is what
/// makes this install eligible to update itself.
pub(crate) struct Installer {
    kind: Kind,
    /// The file this process runs from, which the installed path must still
    /// be when the update starts.
    running: Identity,
    version: String,
    asset: Asset,
    key: PublicKey,
}

/// Which of the installer's two install modes put DBDelve where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Scope {
    CurrentUser,
    AllUsers,
}

/// The two ways of installing DBDelve on Windows that can replace themselves.
#[derive(Debug, PartialEq)]
enum Kind {
    /// The installer put `dbdelve.exe` here; the new release's installer,
    /// run silently in the same mode, replaces it.
    Setup { exe: PathBuf, scope: Scope },
    /// The zip, unpacked where the user can write; its files are swapped.
    Portable { exe: PathBuf },
}

impl Installer {
    /// What the update replaces, and so what to relaunch.
    pub(super) fn installed(&self) -> PathBuf {
        self.kind.installed()
    }
}

impl Kind {
    fn asset_name(&self, version: &str, arch: &str) -> String {
        match self {
            Kind::Setup { .. } => format!("{}-setup.exe", release_directory(version, arch)),
            Kind::Portable { .. } => format!("{}.zip", release_directory(version, arch)),
        }
    }

    fn installed(&self) -> PathBuf {
        match self {
            Kind::Setup { exe, .. } | Kind::Portable { exe } => exe.clone(),
        }
    }
}

/// The one folder a release zip holds.
fn release_directory(version: &str, arch: &str) -> String {
    format!("dbdelve-{version}-windows-{arch}")
}

/// The executable this process was started from, as it was then.
#[cfg(windows)]
static RUNNING_AT_START: OnceLock<Identity> = OnceLock::new();

/// Records the executable this process runs from, and clears what an earlier
/// update set aside beside it. Called once at startup, before any update
/// check: recorded any later, it could be a newer release another process has
/// put there since.
#[cfg(windows)]
pub(super) fn remember_install() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return;
        }
    };
    match Identity::of(&exe) {
        Ok(identity) => {
            _ = RUNNING_AT_START.set(identity);
        }
        Err(error) => tracing::info!("the running executable could not be read: {error}"),
    }
    let Some(folder) = exe.parent() else {
        return;
    };
    // Under the lock: another DBDelve part way through an update needs what
    // it set aside, to put back if it fails.
    match lock(&exe) {
        Ok(_lock) => clear_set_aside(folder),
        Err(error) => tracing::info!("an update is running, so its leftovers stay: {error}"),
    }
}

/// Best effort: a set-aside executable still running, or one in a folder only
/// an administrator can write, stays until the next try. The installer clears
/// the latter.
fn clear_set_aside(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !(name.to_string_lossy().ends_with(SET_ASIDE) || name == STAGING) {
            continue;
        }
        let path = entry.path();
        if path.join(UNRESTORED).is_file() {
            tracing::warn!(path = %path.display(), "kept: an update did not finish or put back what it holds");
            continue;
        }
        let cleared = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match cleared {
            Ok(()) => tracing::info!(path = %path.display(), "cleared what an update set aside"),
            Err(error) => tracing::info!(path = %path.display(), "left for later: {error}"),
        }
    }
}

/// The installer for `version`, when this install may replace itself with it.
/// Each reason it may not is logged once per check.
#[cfg(windows)]
pub(super) fn installer(version: &str, asset: impl Fn(&str) -> Option<Asset>) -> Option<Installer> {
    let Some(key) = release_key() else {
        tracing::info!("this build has no release key, so it does not update itself");
        return None;
    };
    if !release_channel() {
        tracing::info!("not the release build, so no self-update");
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return None;
        }
    };
    let kind = match kind_of(&exe, registered, writable) {
        Ok(kind) => kind,
        Err(reason) => {
            tracing::info!(path = %exe.display(), "no self-update: {reason}");
            return None;
        }
    };
    let Some(&running) = RUNNING_AT_START.get() else {
        tracing::info!("the running executable was not recorded at startup");
        return None;
    };
    if let Err((_, reason)) = unchanged(&kind.installed(), running) {
        tracing::info!("no self-update: {reason}");
        return None;
    }
    let name = kind.asset_name(version, std::env::consts::ARCH);
    let Some(asset) = asset(&name) else {
        tracing::info!(
            version,
            name,
            "the release has no signed update for this install"
        );
        return None;
    };
    Some(Installer {
        kind,
        running,
        version: version.to_string(),
        asset,
        key,
    })
}

/// A dev or other variant build shares nothing with the release, and must not
/// replace itself with one. A test build may, so the whole path can be run
/// without touching the installed app.
#[cfg(windows)]
fn release_channel() -> bool {
    cfg!(feature = "update-test-hooks") || crate::store::is_release_variant()
}

/// How the executable at `exe` was installed, or why it cannot replace
/// itself. `registered` is where the installer recorded an install in each
/// mode, if it did.
fn kind_of(
    exe: &Path,
    registered: impl Fn(Scope) -> Option<PathBuf>,
    writable: impl Fn(&Path) -> bool,
) -> Result<Kind, &'static str> {
    if !exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(EXE))
    {
        return Err("it is not dbdelve.exe");
    }
    let Some(folder) = exe.parent() else {
        return Err("it has no folder");
    };
    for scope in [Scope::CurrentUser, Scope::AllUsers] {
        if registered(scope).is_some_and(|location| same_folder(&location, folder)) {
            return Ok(Kind::Setup {
                exe: exe.to_path_buf(),
                scope,
            });
        }
    }
    // What the zip has beside the executable and a build tree does not.
    let unpacked = ["LICENSE", "NOTICES.md"].iter().all(|name| {
        folder
            .join(name)
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file())
    });
    if !unpacked {
        return Err("it is neither the installer's install nor an unpacked release zip");
    }
    if !writable(folder) {
        return Err("its folder is not writable");
    }
    Ok(Kind::Portable {
        exe: exe.to_path_buf(),
    })
}

/// Windows paths compare without case, and the installer records its
/// location with a trailing backslash.
fn same_folder(first: &Path, second: &Path) -> bool {
    let normal = |path: &Path| {
        let path = path.to_string_lossy().replace('/', "\\");
        let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
        path.trim_end_matches('\\').to_lowercase()
    };
    normal(first) == normal(second)
}

/// The folder the installer recorded for an install in `scope`. The
/// installer is 64-bit, so its all-users entry is in the 64-bit view.
#[cfg(windows)]
fn registered(scope: Scope) -> Option<PathBuf> {
    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
    };

    let hive = match scope {
        Scope::CurrentUser => HKEY_CURRENT_USER,
        Scope::AllUsers => HKEY_LOCAL_MACHINE,
    };
    let key = RegKey::predef(hive)
        .open_subkey_with_flags(UNINSTALL_KEY, KEY_READ | KEY_WOW64_64KEY)
        .ok()?;
    ["Inno Setup: App Path", "InstallLocation"]
        .into_iter()
        .find_map(|name| {
            key.get_value::<OsString, _>(name)
                .ok()
                .filter(|location| !location.is_empty())
        })
        .map(PathBuf::from)
}

/// Downloads, verifies and puts the update in place, and returns the path to
/// relaunch. Blocking. On failure the installed app still runs, and the reason
/// is in words for the dropdown; the detail goes to the log.
#[cfg(windows)]
pub(crate) fn install(installer: &Installer, progress: &Progress) -> Result<PathBuf, &'static str> {
    let Installer {
        kind,
        running,
        version,
        asset,
        key,
    } = installer;
    let installed = kind.installed();
    let Some(folder) = installed.parent() else {
        return Err(failed(COULD_NOT_REPLACE)("the app has no folder".into()));
    };
    tracing::info!(version, path = %installed.display(), "update started");
    let _lock = lock(&installed).map_err(failed(ALREADY_RUNNING))?;
    unchanged(&installed, *running).map_err(|(reason, detail)| failed(reason)(detail))?;

    let scratch = Scratch::new().map_err(failed(NO_SCRATCH))?;
    let file = scratch.0.join(&asset.name);
    let agent = agent();
    // First, so a release whose signature is missing fails before the
    // download rather than after it.
    let signature =
        fetch_signature(&agent, &asset.signature_url).map_err(failed(COULD_NOT_DOWNLOAD))?;
    tracing::info!(url = asset.url, size = asset.size, "downloading the update");
    download(&agent, &asset.url, asset.size, &file, progress)
        .map_err(failed(COULD_NOT_DOWNLOAD))?;
    let held = hold(&file).map_err(failed(COULD_NOT_DOWNLOAD))?;
    verify_contents(&held, &signature, key, &asset.name).map_err(failed(NOT_SIGNED))?;
    tracing::info!("the update's signature verified");
    // Again, just before anything changes: another user, or an installer run
    // by hand, may have replaced this install while the download ran.
    unchanged(&installed, *running).map_err(|(reason, detail)| failed(reason)(detail))?;

    progress.installing();
    match kind {
        Kind::Setup { scope, .. } => {
            run_setup(&file, *scope, folder, &scratch.0.join("setup.log"))?;
        }
        Kind::Portable { .. } => put_in_place(
            &file,
            folder,
            &release_directory(version, std::env::consts::ARCH),
            &mut |from, to| fs::rename(from, to),
            &mut protect,
        )?,
    }
    drop(held);
    replaced(&installed, *running).map_err(failed(COULD_NOT_REPLACE))?;
    tracing::info!(version, path = %installed.display(), "update installed");
    Ok(installed)
}

/// Runs the release's installer silently over this install and waits for it.
/// An all-users install asks for elevation, and the unelevated installer
/// waits for its elevated self and hands back its exit code.
#[cfg(windows)]
fn run_setup(setup: &Path, scope: Scope, folder: &Path, log: &Path) -> Result<(), &'static str> {
    use std::os::windows::process::CommandExt as _;

    let mut command = std::process::Command::new(setup);
    // Raw: the installer reads quotes anywhere in an argument and no
    // backslash escapes, which is not how Rust quotes a path with spaces.
    for argument in setup_arguments(scope, folder, log) {
        command.raw_arg(argument);
    }
    tracing::info!(?scope, folder = %folder.display(), "running the release's installer");
    let status = command
        .stdin(Stdio::null())
        .status()
        .map_err(|error| format!("{}: {error}", setup.display()))
        .map_err(failed(COULD_NOT_REPLACE))?;
    #[cfg(feature = "update-test-hooks")]
    keep_setup_log(log);
    if status.success() {
        return Ok(());
    }
    if !folder.join(EXE).is_file() {
        tracing::error!(folder = %folder.display(), "the failed installer left no {EXE}");
    }
    let reason = match status.code() {
        Some(SETUP_CANCELLED) => CANCELLED,
        _ => COULD_NOT_REPLACE,
    };
    Err(failed(reason)(format!(
        "the installer exited with {status}; its log ends: {}",
        log_tail(log)
    )))
}

/// Only a test build, told to by `DBDELVE_UPDATE_SETUP_LOG`, copies the
/// installer's log there: the scratch folder holding it goes with the update,
/// and the end-to-end test reads it to see what the installer did.
#[cfg(all(windows, feature = "update-test-hooks"))]
fn keep_setup_log(log: &Path) {
    let Some(keep) = std::env::var_os("DBDELVE_UPDATE_SETUP_LOG").filter(|keep| !keep.is_empty())
    else {
        return;
    };
    if let Err(error) = fs::copy(log, &keep) {
        tracing::info!(path = %Path::new(&keep).display(), "the installer's log could not be kept: {error}");
    }
}

/// The installer's command line. `/UPDATE` is this app's own switch: the
/// installer then moves the running executables aside before it installs,
/// and puts them back if it fails.
fn setup_arguments(scope: Scope, folder: &Path, log: &Path) -> Vec<String> {
    let scope = match scope {
        Scope::CurrentUser => "/CURRENTUSER",
        Scope::AllUsers => "/ALLUSERS",
    };
    vec![
        "/VERYSILENT".into(),
        "/SUPPRESSMSGBOXES".into(),
        "/NORESTART".into(),
        // Silent, it would otherwise close this app to replace its files.
        "/NOCLOSEAPPLICATIONS".into(),
        scope.into(),
        "/UPDATE".into(),
        format!("\"/DIR={}\"", folder.display()),
        format!("\"/LOG={}\"", log.display()),
    ]
}

#[cfg(windows)]
fn log_tail(log: &Path) -> String {
    let Ok(log) = fs::read(log) else {
        return "(no log)".into();
    };
    let log = String::from_utf8_lossy(&log);
    let lines = log.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(12)..].join(" | ")
}

/// The update is in place only if another file now stands at the installed
/// path; an installer that exits cleanly without putting one there has not
/// updated anything.
fn replaced(installed: &Path, running: Identity) -> Result<(), String> {
    match unchanged(installed, running) {
        Err((reason, _)) if reason == ALREADY_UPDATED => Ok(()),
        Err((_, detail)) => Err(detail),
        Ok(()) => Err(format!(
            "{} is still the file this DBDelve started from",
            installed.display()
        )),
    }
}

/// Unpacks the release zip beside the install and swaps its files in, or
/// leaves the install as it was.
fn put_in_place(
    zip: &Path,
    folder: &Path,
    top: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    protect: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> Result<(), &'static str> {
    let staging = folder.join(STAGING);
    // What an earlier attempt left when it was killed before it could clean up.
    remove(&staging);
    let aside = folder.join(format!("{}{SET_ASIDE}", unique()));
    let swapped = unpack(zip, &staging, top)
        .map_err(|(reason, detail)| failed(reason)(detail))
        .and_then(|release| {
            swap(&release, folder, &aside, rename, protect).map_err(failed(COULD_NOT_REPLACE))
        });
    remove(&staging);
    swapped
}

/// Unpacks the zip into `into`, which must not exist yet, and returns the
/// release folder in it: `top`, alone, holding `dbdelve.exe`. The reason comes
/// with the detail for the log.
fn unpack(zip: &Path, into: &Path, top: &str) -> Result<PathBuf, (&'static str, String)> {
    let unreadable = |error: &dyn std::fmt::Display| (COULD_NOT_UNPACK, error.to_string());
    fs::create_dir(into).map_err(|error| unreadable(&error))?;
    let file = File::open(zip).map_err(|error| unreadable(&error))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| unreadable(&error))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| unreadable(&error))?;
        let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let Some(relative) = entry.enclosed_name().filter(|path| within(path, top)) else {
            return Err((NOT_THE_APP, format!("{name:?} is not inside {top}")));
        };
        if entry.is_symlink() {
            return Err((NOT_THE_APP, format!("{name:?} is a link")));
        }
        let at = into.join(&relative);
        let unpacked = if entry.is_dir() {
            fs::create_dir_all(&at)
        } else {
            at.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| File::create_new(&at))
                .and_then(|mut file| io::copy(&mut entry, &mut file).map(drop))
        };
        unpacked.map_err(|error| (COULD_NOT_UNPACK, format!("{name}: {error}")))?;
    }
    let release = into.join(top);
    if !release
        .join(EXE)
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err((NOT_THE_APP, format!("it has no {top}/{EXE}")));
    }
    Ok(release)
}

/// Under `top`, through plain names only: no root, drive, `..` or stream.
fn within(path: &Path, top: &str) -> bool {
    let mut components = path.components();
    components.next() == Some(Component::Normal(OsStr::new(top)))
        && components.all(|component| {
            matches!(component, Component::Normal(part) if !part.to_string_lossy().contains(':'))
        })
}

/// Puts the unpacked `release` over the install in `folder`, file by file:
/// each installed file the release replaces moves into `aside` first, since a
/// running one can be renamed but not overwritten, then the release's file
/// moves to its place. The bridge's folder is replaced whole. Files the
/// release does not ship stay. Any failure moves everything back. `protect`
/// writes the [`UNRESTORED`] marker; nothing moves until it has.
fn swap(
    release: &Path,
    folder: &Path,
    aside: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    protect: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> Result<(), String> {
    let tree = PLUGIN_TREE.iter().collect::<PathBuf>();
    let shipped = files_under(release).map_err(|error| format!("listing the update: {error}"))?;
    let mut displaced = Vec::new();
    let mut incoming = Vec::new();
    for file in shipped.into_iter().filter(|file| !file.starts_with(&tree)) {
        if folder.join(&file).symlink_metadata().is_ok() {
            displaced.push(file.clone());
        }
        incoming.push(file);
    }
    let old_tree = folder.join(&tree);
    let replaces_tree = release.join(&tree).is_dir();
    if replaces_tree {
        if old_tree.is_dir() {
            // A folder whose executable runs cannot be renamed; its files can.
            let files = files_under(&old_tree)
                .map_err(|error| format!("listing {}: {error}", old_tree.display()))?;
            displaced.extend(files.into_iter().map(|file| tree.join(file)));
        } else if old_tree.symlink_metadata().is_ok() {
            displaced.push(tree.clone());
        }
        incoming.push(tree);
    }

    let mut done = Vec::new();
    let applied = apply(
        &Plan {
            release,
            folder,
            aside,
            displaced: &displaced,
            incoming: &incoming,
            emptied: replaces_tree.then_some(old_tree.as_path()),
        },
        rename,
        protect,
        &mut done,
    );
    let Err(error) = applied else {
        release_aside(aside);
        return Ok(());
    };
    let mut unrestored = String::new();
    for (from, to) in done.iter().rev() {
        let back = from
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| rename(to, from));
        if let Err(undo) = back {
            tracing::error!(path = %from.display(), "could not be put back from {}: {undo}", to.display());
            unrestored.push_str(&format!("{} <- {}\r\n", from.display(), to.display()));
        }
    }
    if unrestored.is_empty() {
        release_aside(aside);
    } else {
        tracing::error!(path = %aside.display(), "kept, protected by its {UNRESTORED}");
        let listed = fs::OpenOptions::new()
            .append(true)
            .open(aside.join(UNRESTORED))
            .and_then(|mut marker| io::Write::write_all(&mut marker, unrestored.as_bytes()));
        if let Err(list) = listed {
            tracing::warn!(path = %aside.display(), "what was not put back could not be listed in it: {list}");
        }
    }
    Err(error.to_string())
}

/// The set-aside folder once nothing in it is needed: unprotected, then
/// removed. What still runs from it keeps it until the next start clears it.
/// A marker that will not go leaves it protected, and never cleared, rather
/// than failing an update that has finished.
fn release_aside(aside: &Path) {
    match fs::remove_file(aside.join(UNRESTORED)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %aside.display(), "stays protected, so nothing will clear it: {error}");
            return;
        }
    }
    if let Err(error) = fs::remove_dir_all(aside) {
        tracing::info!(path = %aside.display(), "left for the next start: {error}");
    }
}

/// The real [`UNRESTORED`] marker, which the tests replace to make it fail.
fn protect(marker: &Path) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = fs::File::create(marker)?;
    file.write_all(PROTECTION.as_bytes())?;
    // On disk before the first rename, so a power cut cannot keep the renames
    // and lose the marker.
    file.sync_all()
}

struct Plan<'a> {
    release: &'a Path,
    folder: &'a Path,
    aside: &'a Path,
    /// Installed files to move out of the way, relative to the folder.
    displaced: &'a [PathBuf],
    /// The release's files and folders to move in, relative to both.
    incoming: &'a [PathBuf],
    /// Emptied of files by the set-aside, and removed so its replacement can
    /// move in whole.
    emptied: Option<&'a Path>,
}

/// Each rename that succeeds goes into `done` as `(from, to)`.
fn apply(
    plan: &Plan,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    protect: &mut dyn FnMut(&Path) -> io::Result<()>,
    done: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let mut step = |from: PathBuf, to: PathBuf| {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        rename(&from, &to)?;
        done.push((from, to));
        Ok::<_, io::Error>(())
    };
    fs::create_dir(plan.aside)?;
    protect(&plan.aside.join(UNRESTORED))?;
    for file in plan.displaced {
        step(plan.folder.join(file), plan.aside.join(file))?;
    }
    if let Some(emptied) = plan.emptied.filter(|emptied| emptied.is_dir()) {
        fs::remove_dir_all(emptied)?;
    }
    for file in plan.incoming {
        step(plan.release.join(file), plan.folder.join(file))?;
    }
    Ok(())
}

/// Every file under `folder`, relative to it, folders left out.
fn files_under(folder: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(folder.join(&relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        io::{self, Read as _, Write as _},
        path::{Path, PathBuf},
    };

    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::{
        EXE, Kind, SET_ASIDE, STAGING, Scope, UNRESTORED, clear_set_aside, files_under, kind_of,
        protect, put_in_place, release_directory, replaced, same_folder, setup_arguments, swap,
        unpack,
    };
    use crate::update::{
        download::{COULD_NOT_REPLACE, COULD_NOT_UNPACK, NOT_THE_APP, tests::scratch},
        identity::{ALREADY_UPDATED, Identity, unchanged},
    };

    #[test]
    fn each_install_asks_for_its_own_asset() {
        let setup = Kind::Setup {
            exe: PathBuf::from(r"C:\Program Files\DBDelve\dbdelve.exe"),
            scope: Scope::AllUsers,
        };
        assert_eq!(
            setup.asset_name("0.8.0", "x86_64"),
            "dbdelve-0.8.0-windows-x86_64-setup.exe"
        );
        assert_eq!(
            setup.installed(),
            Path::new(r"C:\Program Files\DBDelve\dbdelve.exe")
        );
        let portable = Kind::Portable {
            exe: PathBuf::from(r"D:\tools\dbdelve-0.7.1-windows-x86_64\dbdelve.exe"),
        };
        assert_eq!(
            portable.asset_name("0.8.0", "x86_64"),
            "dbdelve-0.8.0-windows-x86_64.zip"
        );
        assert_eq!(
            portable.installed(),
            Path::new(r"D:\tools\dbdelve-0.7.1-windows-x86_64\dbdelve.exe")
        );
    }

    #[test]
    fn a_folder_is_the_same_however_windows_spells_it() {
        let folder = Path::new(r"C:\Users\Me\AppData\Local\Programs\DBDelve");
        for spelling in [
            r"C:\Users\Me\AppData\Local\Programs\DBDelve\",
            r"c:\users\me\appdata\local\programs\dbdelve",
            r"\\?\C:\Users\Me\AppData\Local\Programs\DBDelve",
            "C:/Users/Me/AppData/Local/Programs/DBDelve/",
        ] {
            assert!(same_folder(Path::new(spelling), folder), "{spelling}");
        }
        assert!(!same_folder(
            Path::new(r"C:\Users\Me\AppData\Local\Programs\DBDelve2"),
            folder
        ));
        assert!(!same_folder(Path::new(r"C:\Program Files\DBDelve"), folder));
    }

    #[test]
    fn the_installers_entry_decides_its_mode() {
        let mine = r"C:\Users\Me\AppData\Local\Programs\DBDelve";
        let everyone = r"C:\Program Files\DBDelve";
        let registered = |user: Option<&'static str>, machine: Option<&'static str>| {
            move |scope: Scope| {
                match scope {
                    Scope::CurrentUser => user,
                    Scope::AllUsers => machine,
                }
                .map(PathBuf::from)
            }
        };
        // Joined, so a Mac, where a backslash is no separator, reads the
        // same folder and name as Windows.
        let exe = |folder: &str| Path::new(folder).join(EXE);
        let anywhere = |_: &Path| true;

        assert_eq!(
            kind_of(
                &exe(mine),
                registered(Some(r"C:\USERS\me\AppData\Local\Programs\DBDelve\"), None),
                anywhere
            ),
            Ok(Kind::Setup {
                exe: exe(mine),
                scope: Scope::CurrentUser
            })
        );
        // Installed both ways: the one that is this folder.
        assert_eq!(
            kind_of(
                &exe(everyone),
                registered(Some(mine), Some(everyone)),
                anywhere
            ),
            Ok(Kind::Setup {
                exe: exe(everyone),
                scope: Scope::AllUsers
            })
        );
        // An install it is not part of, or none at all, and no zip's files
        // beside it.
        assert!(
            kind_of(
                &exe(r"D:\dbdelve"),
                registered(Some(mine), Some(everyone)),
                anywhere
            )
            .is_err()
        );
        assert!(kind_of(&exe(mine), registered(None, None), anywhere).is_err());
        // Another program in the install's folder.
        assert!(
            kind_of(
                &Path::new(mine).join("other.exe"),
                registered(Some(mine), None),
                anywhere
            )
            .is_err()
        );
    }

    #[test]
    fn an_unpacked_zip_is_portable_where_it_can_be_written() {
        let directory = scratch("windows-portable-kind");
        let folder = directory.join("dbdelve-0.7.1-windows-x86_64");
        fs::create_dir(&folder).unwrap();
        let exe = folder.join(EXE);
        fs::write(&exe, "").unwrap();
        let nowhere = |_| None;

        // A build tree: no licence or notices beside the executable.
        assert!(kind_of(&exe, nowhere, |_| true).is_err());
        fs::write(folder.join("LICENSE"), "").unwrap();
        assert!(kind_of(&exe, nowhere, |_| true).is_err());
        fs::write(folder.join("NOTICES.md"), "").unwrap();
        assert_eq!(
            kind_of(&exe, nowhere, |_| true),
            Ok(Kind::Portable { exe: exe.clone() })
        );
        assert_eq!(
            kind_of(&exe, nowhere, |_| false),
            Err("its folder is not writable")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            use crate::update::download::writable;
            fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
            let locked = kind_of(&exe, nowhere, writable);
            fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(locked, Err("its folder is not writable"));
            assert!(kind_of(&exe, nowhere, writable).is_ok());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_installer_runs_silently_in_the_installs_own_mode_and_folder() {
        let folder = Path::new(r"C:\Program Files\DBDelve");
        let log = Path::new(r"C:\Users\Me\AppData\Local\Temp\dbdelve-update-1\setup.log");
        assert_eq!(
            setup_arguments(Scope::AllUsers, folder, log),
            [
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/NOCLOSEAPPLICATIONS",
                "/ALLUSERS",
                "/UPDATE",
                r#""/DIR=C:\Program Files\DBDelve""#,
                r#""/LOG=C:\Users\Me\AppData\Local\Temp\dbdelve-update-1\setup.log""#,
            ]
        );
        assert!(setup_arguments(Scope::CurrentUser, folder, log).contains(&"/CURRENTUSER".into()));
        assert!(!setup_arguments(Scope::CurrentUser, folder, log).contains(&"/ALLUSERS".into()));
    }

    #[test]
    fn the_installer_script_honours_the_update_switch() {
        let script = include_str!("../../assets/windows/dbdelve.iss");
        let suffix = format!("SetAsideSuffix = '{}';", super::SET_ASIDE);
        let marker = format!("UnrestoredMarker = '{UNRESTORED}';");
        for needed in [
            "'/UPDATE'",
            "function PrepareToInstall(var NeedsRestart: Boolean): String;",
            "procedure DeinitializeSetup;",
            "if not SetAside('dbdelve.exe', Aside)",
            r"if not SetAside('plugins\odbc\bin\dbdelve-odbc.exe', Aside)",
            "if not SetAside('plugins', Aside)",
            r#"Name: "{app}\*.dbdelve-old""#,
            suffix.as_str(),
            marker.as_str(),
        ] {
            assert!(script.contains(needed), "{needed}");
        }
        let protect = script
            .find("SaveStringToFile(AddBackslash(Aside) + UnrestoredMarker")
            .unwrap();
        let first_move = script
            .find("if not SetAside('dbdelve.exe', Aside)")
            .unwrap();
        assert!(
            protect < first_move,
            "the folder is protected before anything moves into it"
        );
    }

    /// A zip of `entries`: a name ending in `/` is a folder, `->` a link.
    fn zip_of(at: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = ZipWriter::new(File::create(at).unwrap());
        let options = SimpleFileOptions::default();
        for (name, data) in entries {
            if let Some(folder) = name.strip_suffix('/') {
                zip.add_directory(folder, options).unwrap();
            } else if let Some((link, target)) = name.split_once(" -> ") {
                zip.add_symlink(link, target, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(data).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    /// Every file under `folder` and what it holds, `/` between names.
    fn contents(folder: &Path) -> BTreeMap<String, String> {
        files_under(folder)
            .unwrap()
            .into_iter()
            .map(|file| {
                let text = fs::read_to_string(folder.join(&file)).unwrap();
                let name = file
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                (name, text)
            })
            .collect()
    }

    fn release(top: &str, exe: &str) -> Vec<(String, Vec<u8>)> {
        [
            ("dbdelve.exe", exe),
            ("LICENSE", "licence"),
            ("NOTICES.md", "notices"),
            ("licenses/OFL.txt", "ofl"),
            ("licenses/new.txt", "new licence"),
            ("plugins/odbc/dbdelve-plugin.toml", "new manifest"),
            ("plugins/odbc/bin/dbdelve-odbc.exe", "new bridge"),
        ]
        .into_iter()
        .map(|(name, data)| (format!("{top}/{name}"), data.as_bytes().to_vec()))
        .collect()
    }

    fn zip_release(at: &Path, entries: &[(String, Vec<u8>)]) {
        zip_of(
            at,
            &entries
                .iter()
                .map(|(name, data)| (name.as_str(), data.as_slice()))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn a_zip_must_be_the_release_it_claims() {
        let directory = scratch("windows-unpack");
        let top = release_directory("9.9.9", "x86_64");
        let good = directory.join("good.zip");
        let mut entries = vec![(format!("{top}/"), Vec::new())];
        entries.extend(release(&top, "new"));
        zip_release(&good, &entries);
        let unpacked = unpack(&good, &directory.join("good"), &top).unwrap();
        assert_eq!(unpacked, directory.join("good").join(&top));
        assert_eq!(
            fs::read_to_string(unpacked.join("plugins/odbc/bin/dbdelve-odbc.exe")).unwrap(),
            "new bridge"
        );
        // Another version's zip under this name.
        let other = release_directory("9.9.8", "x86_64");
        assert_eq!(
            unpack(&good, &directory.join("other"), &other).map_err(|(reason, _)| reason),
            Err(NOT_THE_APP)
        );

        let file = |name: &str| (format!("{top}/{name}"), b"x".to_vec());
        for (case, entries) in [
            ("no-exe", vec![file("LICENSE")]),
            (
                "stray",
                vec![file(EXE), ("stray.txt".into(), b"x".to_vec())],
            ),
            (
                "escape",
                vec![file(EXE), (format!("{top}/../../escaped"), b"x".to_vec())],
            ),
            (
                "climb",
                vec![
                    file(EXE),
                    (format!("{top}/a/../../{top}2/x"), b"x".to_vec()),
                ],
            ),
            (
                "absolute",
                vec![file(EXE), ("/absolute".into(), b"x".to_vec())],
            ),
            (
                "drive",
                vec![file(EXE), (format!("{top}/C:evil"), b"x".to_vec())],
            ),
            (
                "stream",
                vec![
                    file(EXE),
                    (format!("{top}/dbdelve.exe:evil"), b"x".to_vec()),
                ],
            ),
            (
                "link",
                vec![
                    file(EXE),
                    (format!("{top}/link -> ../../outside"), Vec::new()),
                ],
            ),
        ] {
            let at = directory.join(format!("{case}.zip"));
            zip_release(&at, &entries);
            assert_eq!(
                unpack(&at, &directory.join(case), &top).map_err(|(reason, _)| reason),
                Err(NOT_THE_APP),
                "{case}"
            );
        }
        assert!(!directory.join("escaped").exists());

        fs::write(directory.join("garbage.zip"), "not a zip").unwrap();
        assert_eq!(
            unpack(
                &directory.join("garbage.zip"),
                &directory.join("garbage"),
                &top
            )
            .map_err(|(reason, _)| reason),
            Err(COULD_NOT_UNPACK)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// An install of the previous release, with what a user adds beside it.
    fn installed_folder(folder: &Path) {
        for (name, data) in [
            ("dbdelve.exe", "old"),
            ("LICENSE", "old licence"),
            ("NOTICES.md", "old notices"),
            ("licenses/OFL.txt", "old ofl"),
            ("licenses/gone.txt", "dropped since"),
            ("plugins/odbc/dbdelve-plugin.toml", "old manifest"),
            ("plugins/odbc/bin/dbdelve-odbc.exe", "old bridge"),
            ("plugins/odbc/stale.txt", "stale"),
            ("plugins/mine/dbdelve-plugin.toml", "mine"),
            ("notes.txt", "my notes"),
        ] {
            let at = folder.join(name);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, data).unwrap();
        }
    }

    fn set_aside_folders(folder: &Path) -> Vec<PathBuf> {
        fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().ends_with(super::SET_ASIDE))
            .collect()
    }

    #[test]
    fn the_release_replaces_its_own_files_and_leaves_the_rest() {
        let directory = scratch("windows-swap");
        let folder = directory.join("dbdelve-0.7.1-windows-x86_64");
        installed_folder(&folder);
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join(format!("{top}.zip"));
        zip_release(&zip, &release(&top, "new"));
        // Running: Windows renames these but will not overwrite them.
        let mut running = File::open(folder.join(EXE)).unwrap();
        let mut bridge = File::open(folder.join("plugins/odbc/bin/dbdelve-odbc.exe")).unwrap();
        // What a killed attempt left.
        fs::create_dir_all(folder.join(STAGING).join("half")).unwrap();

        put_in_place(
            &zip,
            &folder,
            &top,
            &mut |from, to| fs::rename(from, to),
            &mut protect,
        )
        .unwrap();

        let after = contents(&folder);
        let expected = [
            ("dbdelve.exe", "new"),
            ("LICENSE", "licence"),
            ("NOTICES.md", "notices"),
            ("licenses/OFL.txt", "ofl"),
            ("licenses/new.txt", "new licence"),
            ("licenses/gone.txt", "dropped since"),
            ("plugins/odbc/dbdelve-plugin.toml", "new manifest"),
            ("plugins/odbc/bin/dbdelve-odbc.exe", "new bridge"),
            ("plugins/mine/dbdelve-plugin.toml", "mine"),
            ("notes.txt", "my notes"),
        ];
        let mut text = String::new();
        running.read_to_string(&mut text).unwrap();
        assert_eq!(text, "old", "the running app keeps its file");
        text.clear();
        bridge.read_to_string(&mut text).unwrap();
        assert_eq!(text, "old bridge");
        // On a Mac the open files go with the set-aside folder; on Windows
        // they would keep it until they close, and the next start clears it.
        drop((running, bridge));
        clear_set_aside(&folder);
        let after = after
            .into_iter()
            .filter(|(name, _)| !name.contains(".dbdelve-old/"))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            after,
            expected
                .into_iter()
                .map(|(name, data)| (name.to_string(), data.to_string()))
                .collect()
        );
        assert!(!folder.join(STAGING).exists());
        assert!(set_aside_folders(&folder).is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_failed_swap_puts_everything_back() {
        let directory = scratch("windows-rollback");
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join(format!("{top}.zip"));
        zip_release(&zip, &release(&top, "new"));
        let tree = Path::new("plugins").join("odbc");
        for (case, fails_on) in [
            ("moving the old exe aside", PathBuf::from(EXE)),
            (
                "moving a new licence in",
                Path::new("licenses").join("new.txt"),
            ),
            ("moving the bridge's folder in", tree.clone()),
        ] {
            let folder = directory.join(case);
            installed_folder(&folder);
            let before = contents(&folder);
            let running = File::open(folder.join(EXE)).unwrap();
            let target = folder.join(&fails_on);
            let mut moves = 0;
            let mut refusing = |from: &Path, to: &Path| {
                moves += 1;
                let leaving = from == target && !to.starts_with(folder.join(STAGING));
                let arriving = to == target && from.starts_with(folder.join(STAGING));
                if (case.contains("aside") && leaving) || (!case.contains("aside") && arriving) {
                    return Err(io::Error::other("in use"));
                }
                fs::rename(from, to)
            };
            assert_eq!(
                put_in_place(&zip, &folder, &top, &mut refusing, &mut protect),
                Err(COULD_NOT_REPLACE),
                "{case}"
            );
            assert!(moves > 1, "{case}");
            assert_eq!(contents(&folder), before, "{case}");
            assert!(set_aside_folders(&folder).is_empty(), "{case}");
            assert!(!folder.join(STAGING).exists(), "{case}");
            drop(running);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn what_a_failed_swap_could_not_put_back_is_never_cleared() {
        let directory = scratch("windows-unrestored");
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join(format!("{top}.zip"));
        zip_release(&zip, &release(&top, "new"));
        let folder = directory.join("install");
        installed_folder(&folder);
        let licence = folder.join("LICENSE");
        let mut refusing = |from: &Path, to: &Path| {
            let arriving = to == folder.join("licenses").join("new.txt");
            let returning = to == licence && !from.starts_with(folder.join(STAGING));
            if arriving || returning {
                return Err(io::Error::other("in use"));
            }
            fs::rename(from, to)
        };
        assert_eq!(
            put_in_place(&zip, &folder, &top, &mut refusing, &mut protect),
            Err(COULD_NOT_REPLACE)
        );
        assert!(!licence.exists());
        let aside = set_aside_folders(&folder);
        assert_eq!(aside.len(), 1);
        let marker = fs::read_to_string(aside[0].join(UNRESTORED)).unwrap();
        assert!(marker.starts_with("DBDelve moved"), "{marker}");
        assert!(marker.contains(&licence.display().to_string()), "{marker}");

        fs::create_dir_all(folder.join("1-2.dbdelve-old")).unwrap();
        clear_set_aside(&folder);
        assert_eq!(set_aside_folders(&folder), aside);
        assert_eq!(
            fs::read_to_string(aside[0].join("LICENSE")).unwrap(),
            "old licence"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn nothing_moves_until_the_set_aside_folder_is_protected() {
        let directory = scratch("windows-unprotected");
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join(format!("{top}.zip"));
        zip_release(&zip, &release(&top, "new"));
        let folder = directory.join("install");
        installed_folder(&folder);
        let before = contents(&folder);
        let mut moves = 0;
        assert_eq!(
            put_in_place(
                &zip,
                &folder,
                &top,
                &mut |from, to| {
                    moves += 1;
                    fs::rename(from, to)
                },
                &mut |_| Err(io::Error::other("disk full")),
            ),
            Err(COULD_NOT_REPLACE)
        );
        assert_eq!(moves, 0);
        assert_eq!(contents(&folder), before);
        assert!(set_aside_folders(&folder).is_empty());
        assert!(!folder.join(STAGING).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    /// Keeps `file` from being deleted until what it returns drops, as a
    /// running executable does on Windows.
    #[cfg(unix)]
    fn pin(file: &Path) -> impl Sized + use<> {
        use std::os::unix::fs::PermissionsExt as _;

        struct Unpin(PathBuf);
        impl Drop for Unpin {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let parent = file.parent().unwrap().to_path_buf();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        Unpin(parent)
    }

    #[cfg(windows)]
    fn pin(file: &Path) -> impl Sized + use<> {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_SHARE_READ: u32 = 1;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(file)
            .unwrap()
    }

    #[test]
    fn a_finished_swap_leaves_its_set_aside_folder_to_clearing() {
        let directory = scratch("windows-finished");
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join(format!("{top}.zip"));
        zip_release(&zip, &release(&top, "new"));
        let folder = directory.join("install");
        installed_folder(&folder);
        let mut pinned = None;
        put_in_place(
            &zip,
            &folder,
            &top,
            &mut |from, to| {
                fs::rename(from, to)?;
                let aside = to.ancestors().nth(2).unwrap();
                if to.ends_with("licenses/OFL.txt") && aside.to_string_lossy().ends_with(SET_ASIDE)
                {
                    pinned = Some(pin(to));
                }
                Ok(())
            },
            &mut protect,
        )
        .unwrap();
        let aside = set_aside_folders(&folder);
        assert_eq!(aside.len(), 1, "the pinned file keeps it");
        assert!(!aside[0].join(UNRESTORED).exists());
        drop(pinned);
        clear_set_aside(&folder);
        assert!(set_aside_folders(&folder).is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_bad_zip_changes_nothing() {
        let directory = scratch("windows-bad-zip");
        let folder = directory.join("install");
        installed_folder(&folder);
        let before = contents(&folder);
        let top = release_directory("9.9.9", "x86_64");
        let zip = directory.join("other.zip");
        zip_release(&zip, &release(&release_directory("9.9.8", "x86_64"), "new"));
        assert_eq!(
            put_in_place(
                &zip,
                &folder,
                &top,
                &mut |from, to| fs::rename(from, to),
                &mut protect
            ),
            Err(NOT_THE_APP)
        );
        assert_eq!(contents(&folder), before);
        assert!(!folder.join(STAGING).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn swapping_moves_nothing_it_does_not_ship() {
        let directory = scratch("windows-no-tree");
        let folder = directory.join("install");
        installed_folder(&folder);
        let release = directory.join("release");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join(EXE), "new").unwrap();
        // A release without the bridge leaves the installed one alone.
        swap(
            &release,
            &folder,
            &folder.join("x.dbdelve-old"),
            &mut |from, to| fs::rename(from, to),
            &mut protect,
        )
        .unwrap();
        let after = contents(&folder);
        assert_eq!(after["dbdelve.exe"], "new");
        assert_eq!(after["plugins/odbc/stale.txt"], "stale");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_what_an_update_set_aside_is_cleared() {
        let directory = scratch("windows-clear");
        for name in ["1-2.dbdelve-old", STAGING, "plugins"] {
            fs::create_dir_all(directory.join(name).join("bin")).unwrap();
            fs::write(directory.join(name).join("bin/x"), "x").unwrap();
        }
        for name in [
            "dbdelve.exe.dbdelve-old",
            "dbdelve.exe",
            "notes.dbdelve-old.txt",
        ] {
            fs::write(directory.join(name), "x").unwrap();
        }
        clear_set_aside(&directory);
        let mut left = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(left, ["dbdelve.exe", "notes.dbdelve-old.txt", "plugins"]);
        clear_set_aside(&directory.join("missing"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_install_counts_as_updated_only_once_another_file_is_there() {
        let directory = scratch("windows-replaced");
        let installed = directory.join(EXE);
        fs::write(&installed, "old").unwrap();
        let started = Identity::of(&installed).unwrap();
        unchanged(&installed, started).unwrap();
        // The installer exited cleanly but put nothing new there.
        assert!(replaced(&installed, started).is_err());

        let aside = directory.join("1.dbdelve-old");
        fs::create_dir(&aside).unwrap();
        fs::rename(&installed, aside.join(EXE)).unwrap();
        // Nothing there at all.
        assert!(replaced(&installed, started).is_err());
        assert_eq!(
            unchanged(&installed, started).map_err(|(reason, _)| reason),
            Err(COULD_NOT_REPLACE)
        );
        fs::write(&installed, "new").unwrap();
        replaced(&installed, started).unwrap();
        // Another DBDelve's update, seen by this one before it starts its own.
        assert_eq!(
            unchanged(&installed, started).map_err(|(reason, _)| reason),
            Err(ALREADY_UPDATED)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn only_an_install_replaced_since_start_offers_a_restart() {
        use std::sync::Arc;

        use super::CANCELLED;
        use crate::update::{Asset, State};

        let installer = || {
            Arc::new(super::Installer {
                kind: Kind::Portable {
                    exe: PathBuf::from(r"C:\tools\dbdelve\dbdelve.exe"),
                },
                running: Identity {
                    device: 0,
                    inode: 0,
                },
                version: "9.9.9".into(),
                asset: Asset {
                    name: String::new(),
                    url: String::new(),
                    size: 0,
                    signature_url: String::new(),
                },
                key: minisign_verify::PublicKey::from_base64(
                    "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
                )
                .unwrap(),
            })
        };
        assert!(matches!(
            State::stopped(ALREADY_UPDATED, installer()),
            State::Superseded(path) if path == Path::new(r"C:\tools\dbdelve\dbdelve.exe")
        ));
        assert!(matches!(
            State::stopped(CANCELLED, installer()),
            State::Failed(CANCELLED, _)
        ));
    }
}

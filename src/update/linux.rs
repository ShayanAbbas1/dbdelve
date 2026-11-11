use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use flate2::read::GzDecoder;
use minisign_verify::PublicKey;

use super::{
    Asset, Progress,
    download::{
        ALREADY_RUNNING, COULD_NOT_DOWNLOAD, COULD_NOT_REPLACE, COULD_NOT_UNPACK, NO_SCRATCH,
        NOT_SIGNED, NOT_THE_APP, Scratch, agent, download, failed, fetch_signature, lock,
        release_key, remove, run, verify, writable,
    },
};

const ALREADY_UPDATED: &str =
    "DBDelve was updated since it started. Restart it to run that version.";

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

/// The two ways of installing DBDelve on Linux that can replace themselves.
#[derive(Debug, PartialEq)]
enum Kind {
    /// The AppImage file at this path, replaced whole.
    AppImage(PathBuf),
    /// The tarball's `install.sh` put the binary in `~/.local/bin` under this
    /// home; the new release's own `install.sh` replaces it.
    Tarball { home: PathBuf },
}

impl Kind {
    fn asset_name(&self, version: &str, arch: &str) -> String {
        match self {
            Kind::AppImage(_) => format!("dbdelve-{version}-{arch}.AppImage"),
            Kind::Tarball { .. } => format!("{}.tar.gz", release_directory(version, arch)),
        }
    }

    /// What the update replaces, and so what to relaunch.
    fn installed(&self) -> PathBuf {
        match self {
            Kind::AppImage(appimage) => appimage.clone(),
            Kind::Tarball { home } => home.join(".local/bin/dbdelve"),
        }
    }
}

/// A file, whatever path it is reached by or renamed to.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(path: &Path) -> io::Result<Self> {
        let metadata = path.symlink_metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// The AppImage file as it was when this process started: the running
/// executable is inside its mount, so it has no identity of its own to check.
static APPIMAGE_AT_START: OnceLock<Identity> = OnceLock::new();

/// Records the AppImage this process runs from, if it runs from one. Called
/// once at startup, before any update check: recorded any later, it could be
/// a newer release another process has put there since.
pub(super) fn remember_install() {
    let exe = match std::env::current_exe().and_then(fs::canonicalize) {
        Ok(exe) => exe,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return;
        }
    };
    if let Some(appimage) = appimage_of(
        &exe,
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("APPDIR").as_deref(),
    ) {
        remember(appimage, &APPIMAGE_AT_START);
    }
}

fn remember(appimage: &Path, at_start: &OnceLock<Identity>) {
    match Identity::of(appimage) {
        Ok(identity) => {
            _ = at_start.set(identity);
        }
        Err(error) => tracing::info!("the AppImage could not be read: {error}"),
    }
}

/// The file this process was started from, as it was then.
fn running(kind: &Kind, appimage_at_start: &OnceLock<Identity>) -> io::Result<Identity> {
    match kind {
        Kind::AppImage(_) => appimage_at_start
            .get()
            .copied()
            .ok_or_else(|| io::Error::other("the AppImage was not recorded at startup")),
        // Still the file that was started even once it is replaced or
        // deleted, which the path `current_exe` reads is not.
        Kind::Tarball { .. } => fs::metadata("/proc/self/exe").map(|metadata| Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }),
    }
}

/// Another process may have updated this install since it started, perhaps
/// past the release this one offers: putting that over it would go backwards.
fn unchanged(installed: &Path, running: Identity) -> Result<(), String> {
    match Identity::of(installed) {
        Ok(identity) if identity == running => Ok(()),
        Ok(_) => Err(format!(
            "{} is no longer the file this DBDelve started from",
            installed.display()
        )),
        Err(error) => Err(format!("{}: {error}", installed.display())),
    }
}

/// The one directory a release tarball holds.
fn release_directory(version: &str, arch: &str) -> String {
    format!("dbdelve-{version}-linux-{arch}")
}

/// The installer for `version`, when this install may replace itself with it.
/// Each reason it may not is logged once per check.
pub(super) fn installer(version: &str, asset: impl Fn(&str) -> Option<Asset>) -> Option<Installer> {
    let Some(key) = release_key() else {
        tracing::info!("this build has no release key, so it does not update itself");
        return None;
    };
    if !release_channel() {
        tracing::info!("not the release build, so no self-update");
        return None;
    }
    let exe = match std::env::current_exe().and_then(fs::canonicalize) {
        Ok(exe) => exe,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return None;
        }
    };
    let home = std::env::var_os("HOME");
    let kind = match kind_of(
        &exe,
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("APPDIR").as_deref(),
        home.as_deref().map(Path::new),
    ) {
        Ok(kind) => kind,
        Err(reason) => {
            tracing::info!(path = %exe.display(), "no self-update: {reason}");
            return None;
        }
    };
    let installed = kind.installed();
    let running = match running(&kind, &APPIMAGE_AT_START) {
        Ok(running) => running,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return None;
        }
    };
    if let Err(reason) = unchanged(&installed, running) {
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
fn release_channel() -> bool {
    cfg!(feature = "update-test-hooks") || crate::store::is_release_variant()
}

/// How the executable at `exe` (canonical) was installed, from the
/// environment it runs in, or why it cannot replace itself.
fn kind_of(
    exe: &Path,
    appimage: Option<&OsStr>,
    appdir: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<Kind, &'static str> {
    if let Some(appimage) = appimage_of(exe, appimage, appdir) {
        return appimage_at(appimage).map(Kind::AppImage);
    }
    let Some(home) = home.filter(|home| home.is_absolute()) else {
        return Err("HOME is not set");
    };
    let bin = home.join(".local/bin");
    let installed = bin.join("dbdelve");
    let installed_here = installed
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file())
        && fs::canonicalize(&bin).is_ok_and(|bin| bin.join("dbdelve") == exe);
    if !installed_here {
        return Err("it is neither an AppImage nor the binary install.sh put in ~/.local/bin");
    }
    if !writable(&bin) {
        return Err("~/.local/bin is not writable");
    }
    Ok(Kind::Tarball {
        home: home.to_path_buf(),
    })
}

/// The AppImage `exe` (canonical) runs from, if it runs from one.
fn appimage_of<'a>(
    exe: &Path,
    appimage: Option<&'a OsStr>,
    appdir: Option<&OsStr>,
) -> Option<&'a Path> {
    let appimage = appimage.filter(|value| !value.is_empty()).map(Path::new)?;
    let appdir = appdir.filter(|value| !value.is_empty())?;
    // An AppImage's runtime exports both to everything it starts, so a
    // terminal opened from one hands them to a DBDelve that is no AppImage at
    // all. Only an executable inside the mount APPDIR names is the AppImage.
    fs::canonicalize(appdir)
        .is_ok_and(|appdir| exe.starts_with(appdir))
        .then_some(appimage)
}

fn appimage_at(appimage: &Path) -> Result<PathBuf, &'static str> {
    // Renaming over a symlink would replace the link, not the AppImage.
    if !appimage.is_absolute()
        || !appimage
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file())
    {
        return Err("APPIMAGE does not name an AppImage file");
    }
    let Some(folder) = appimage.parent() else {
        return Err("the AppImage has no parent directory");
    };
    if !writable(folder) {
        return Err("the AppImage's folder is not writable");
    }
    Ok(appimage.to_path_buf())
}

/// Downloads, verifies and puts the update in place, and returns the path to
/// relaunch. Blocking. On failure the installed app is untouched, and the
/// reason is in words for the dropdown; the detail goes to the log.
pub(crate) fn install(installer: &Installer, progress: &Progress) -> Result<PathBuf, &'static str> {
    let Installer {
        kind,
        running,
        version,
        asset,
        key,
    } = installer;
    let installed = kind.installed();
    tracing::info!(version, path = %installed.display(), "update started");
    let _lock = lock(&installed).map_err(failed(ALREADY_RUNNING))?;
    unchanged(&installed, *running).map_err(failed(ALREADY_UPDATED))?;

    let scratch = Scratch::new().map_err(failed(NO_SCRATCH))?;
    let file = scratch.0.join(&asset.name);
    let agent = agent();
    tracing::info!(url = asset.url, size = asset.size, "downloading the update");
    download(&agent, &asset.url, asset.size, &file, progress)
        .map_err(failed(COULD_NOT_DOWNLOAD))?;
    let signature =
        fetch_signature(&agent, &asset.signature_url).map_err(failed(COULD_NOT_DOWNLOAD))?;
    verify(&file, &signature, key, &asset.name).map_err(failed(NOT_SIGNED))?;
    tracing::info!("the update's signature verified");

    progress.installing();
    match kind {
        Kind::AppImage(appimage) => swap_in(&file, appimage)?,
        Kind::Tarball { home } => {
            let release = unpack(
                &file,
                &scratch.0.join("unpacked"),
                &release_directory(version, std::env::consts::ARCH),
            )
            .map_err(|error| error.to_string())
            .map_err(failed(COULD_NOT_UNPACK))?;
            tracing::info!("running the release's install.sh");
            run(&mut bundled_installer(&release, home)).map_err(failed(COULD_NOT_REPLACE))?;
        }
    }
    tracing::info!(version, path = %installed.display(), "update installed");
    Ok(installed)
}

/// Hidden, and beside the AppImage so it is on the same filesystem as it.
fn sibling(appimage: &Path) -> PathBuf {
    let name = appimage
        .file_name()
        .map_or("dbdelve.AppImage".into(), |name| name.to_string_lossy());
    appimage.with_file_name(format!(".{name}.update-new"))
}

/// Copies the verified AppImage beside the installed one and renames it over
/// it, which is atomic: the path holds the old file or the new one, never
/// neither, and the running image keeps its own inode open until it exits.
fn swap_in(download: &Path, appimage: &Path) -> Result<(), &'static str> {
    let staged = sibling(appimage);
    // What an earlier attempt left when it was killed before it could clean up.
    remove(&staged);
    let swapped = copy_executable(download, &staged)
        .map_err(|error| format!("staging the update: {error}"))
        .map_err(failed(COULD_NOT_REPLACE))
        .and_then(|()| elf(&staged).map_err(failed(NOT_THE_APP)))
        .and_then(|()| {
            fs::rename(&staged, appimage)
                .map_err(|error| format!("renaming the update over the app: {error}"))
                .map_err(failed(COULD_NOT_REPLACE))
        });
    remove(&staged);
    swapped?;
    if let Some(folder) = appimage.parent()
        && let Err(error) = File::open(folder).and_then(|folder| folder.sync_all())
    {
        tracing::warn!("the AppImage's folder could not be synced: {error}");
    }
    Ok(())
}

fn copy_executable(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = File::open(from)?;
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(to)?;
    io::copy(&mut source, &mut staged)?;
    // The mode given at creation is narrowed by the umask.
    staged.set_permissions(fs::Permissions::from_mode(0o755))?;
    staged.sync_all()
}

fn elf(path: &Path) -> Result<(), String> {
    let mut magic = [0; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|error| error.to_string())?;
    if magic != *b"\x7fELF" {
        return Err("it is not an executable".into());
    }
    Ok(())
}

/// Unpacks the tarball into `into`, which must not exist yet, and returns the
/// release directory in it: `top`, alone, holding the binary and its
/// installer.
fn unpack(tarball: &Path, into: &Path, top: &str) -> io::Result<PathBuf> {
    fs::create_dir(into)?;
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(tarball)?));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !entry.unpack_in(into)? {
            return Err(io::Error::other(format!(
                "{} would land outside the release",
                path.display()
            )));
        }
    }
    let found = fs::read_dir(into)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    if found != [OsString::from(top)] {
        return Err(io::Error::other(format!(
            "it holds {found:?}, not the one directory {top}"
        )));
    }
    let release = into.join(top);
    for file in ["dbdelve", "install.sh"] {
        if !release
            .join(file)
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(io::Error::other(format!("it has no {top}/{file}")));
        }
    }
    Ok(release)
}

/// The release's own `install.sh`, which knows where each of its files goes:
/// the binary, the ODBC bridge, the desktop entry and the icons.
fn bundled_installer(release: &Path, home: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg(release.join("install.sh"))
        .env("HOME", home)
        .current_dir(release);
    command
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        fs::{self, File},
        io::Read as _,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use flate2::{Compression, write::GzEncoder};

    use super::{
        Identity, Kind, appimage_of, bundled_installer, kind_of, release_directory, remember, run,
        running, sibling, swap_in, unchanged, unpack,
    };
    use crate::update::download::{COULD_NOT_REPLACE, NOT_THE_APP, tests::scratch};

    /// macOS keeps its temp directory behind a symlink, and the executable
    /// path the app checks is always canonical.
    fn canonical_scratch(name: &str) -> PathBuf {
        fs::canonicalize(scratch(name)).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn each_install_asks_for_its_own_asset() {
        let appimage = Kind::AppImage(PathBuf::from("/opt/DBDelve.AppImage"));
        assert_eq!(
            appimage.asset_name("0.8.0", "x86_64"),
            "dbdelve-0.8.0-x86_64.AppImage"
        );
        assert_eq!(appimage.installed(), Path::new("/opt/DBDelve.AppImage"));
        let tarball = Kind::Tarball {
            home: PathBuf::from("/home/me"),
        };
        assert_eq!(
            tarball.asset_name("0.8.0", "aarch64"),
            "dbdelve-0.8.0-linux-aarch64.tar.gz"
        );
        assert_eq!(
            tarball.installed(),
            Path::new("/home/me/.local/bin/dbdelve")
        );
    }

    #[test]
    fn an_appimage_is_only_one_when_running_from_its_mount() {
        let directory = canonical_scratch("appimage-kind");
        let mount = directory.join("mount_abc");
        fs::create_dir_all(mount.join("usr/bin")).unwrap();
        let exe = mount.join("usr/bin/dbdelve");
        fs::write(&exe, "").unwrap();
        let apps = directory.join("Applications");
        fs::create_dir(&apps).unwrap();
        let appimage = apps.join("DBDelve.AppImage");
        fs::write(&appimage, "").unwrap();
        let (image, dir) = (Some(appimage.as_os_str()), Some(mount.as_os_str()));

        assert_eq!(
            kind_of(&exe, image, dir, None),
            Ok(Kind::AppImage(appimage.clone()))
        );
        // Leaked from another AppImage's process tree into this one's: the
        // executable is not inside the mount, so it is not that AppImage.
        let elsewhere = directory.join("dbdelve");
        fs::write(&elsewhere, "").unwrap();
        assert!(kind_of(&elsewhere, image, dir, None).is_err());
        assert_eq!(appimage_of(&exe, image, dir), Some(appimage.as_path()));
        assert_eq!(appimage_of(&elsewhere, image, dir), None);
        // APPIMAGE alone proves nothing.
        assert_eq!(appimage_of(&exe, image, None), None);
        assert!(kind_of(&exe, image, None, None).is_err());
        assert!(kind_of(&exe, image, Some(OsStr::new("")), None).is_err());

        let missing = apps.join("gone.AppImage");
        assert!(kind_of(&exe, Some(missing.as_os_str()), dir, None).is_err());
        assert!(kind_of(&exe, Some(apps.as_os_str()), dir, None).is_err());
        let link = apps.join("link.AppImage");
        std::os::unix::fs::symlink(&appimage, &link).unwrap();
        assert!(kind_of(&exe, Some(link.as_os_str()), dir, None).is_err());

        fs::set_permissions(&apps, fs::Permissions::from_mode(0o555)).unwrap();
        let locked = kind_of(&exe, image, dir, None);
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(locked, Err("the AppImage's folder is not writable"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_install_shs_binary_is_a_tarball_install() {
        let directory = canonical_scratch("tarball-kind");
        let home = directory.join("home");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("dbdelve");
        fs::write(&installed, "").unwrap();
        let tarball = Ok(Kind::Tarball { home: home.clone() });

        assert_eq!(kind_of(&installed, None, None, Some(&home)), tarball);
        // An AppImage's variables, leaked into a terminal this was run from.
        let mount = directory.join("mount_abc");
        fs::create_dir(&mount).unwrap();
        let leaked = directory.join("Other.AppImage");
        fs::write(&leaked, "").unwrap();
        assert_eq!(
            kind_of(
                &installed,
                Some(leaked.as_os_str()),
                Some(mount.as_os_str()),
                Some(&home)
            ),
            tarball
        );

        // The tarball run where it was unpacked.
        let unpacked = directory.join("dbdelve-0.8.0-linux-x86_64/dbdelve");
        fs::create_dir_all(unpacked.parent().unwrap()).unwrap();
        fs::write(&unpacked, "").unwrap();
        assert!(kind_of(&unpacked, None, None, Some(&home)).is_err());
        assert!(kind_of(&installed, None, None, None).is_err());
        assert!(kind_of(&installed, None, None, Some(Path::new("relative"))).is_err());

        // Someone else's binary, linked into ~/.local/bin.
        fs::remove_file(&installed).unwrap();
        std::os::unix::fs::symlink(&unpacked, &installed).unwrap();
        assert!(kind_of(&unpacked, None, None, Some(&home)).is_err());
        assert!(kind_of(&installed, None, None, Some(&home)).is_err());
        fs::remove_file(&installed).unwrap();
        fs::write(&installed, "").unwrap();

        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).unwrap();
        let locked = kind_of(&installed, None, None, Some(&home));
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(locked, Err("~/.local/bin is not writable"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_install_replaced_since_start_is_refused() {
        let directory = scratch("identity");
        let installed = directory.join("dbdelve");
        fs::write(&installed, "A").unwrap();
        let started = Identity::of(&installed).unwrap();
        unchanged(&installed, started).unwrap();

        // Another DBDelve's update, renamed over it.
        let newer = directory.join(".dbdelve.new");
        fs::write(&newer, "C").unwrap();
        fs::rename(&newer, &installed).unwrap();
        assert!(unchanged(&installed, started).is_err());
        fs::remove_file(&installed).unwrap();
        assert!(unchanged(&installed, started).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_appimage_is_remembered_as_it_was_at_startup() {
        let directory = scratch("appimage-identity");
        let appimage = directory.join("DBDelve.AppImage");
        fs::write(&appimage, "A").unwrap();
        let kind = Kind::AppImage(appimage.clone());
        let at_start = OnceLock::new();
        // Not recorded at startup: nothing found later stands in for it.
        assert!(running(&kind, &at_start).is_err());

        remember(&appimage, &at_start);
        let first = running(&kind, &at_start).unwrap();
        unchanged(&appimage, first).unwrap();

        // Another process's update, then this one's first check.
        let newer = directory.join(".DBDelve.AppImage.update-new");
        fs::write(&newer, "C").unwrap();
        fs::rename(&newer, &appimage).unwrap();
        remember(&appimage, &at_start);
        let later = running(&kind, &at_start).unwrap();
        assert_eq!(later, first);
        assert!(unchanged(&appimage, later).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_tarball_install_is_the_running_executable() {
        let kind = Kind::Tarball {
            home: PathBuf::from("/nonexistent"),
        };
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            running(&kind, &OnceLock::new()).unwrap(),
            Identity::of(&exe).unwrap()
        );
    }

    #[test]
    fn the_appimage_is_replaced_in_one_rename() {
        let directory = scratch("appimage-swap");
        let appimage = directory.join("DBDelve.AppImage");
        fs::write(&appimage, "\x7fELF old").unwrap();
        fs::set_permissions(&appimage, fs::Permissions::from_mode(0o700)).unwrap();
        let download = directory.join("dbdelve-9.9.9-x86_64.AppImage");
        fs::write(&download, "\x7fELF new").unwrap();
        fs::set_permissions(&download, fs::Permissions::from_mode(0o600)).unwrap();
        let staged = sibling(&appimage);
        assert_eq!(staged, directory.join(".DBDelve.AppImage.update-new"));
        // What a killed attempt left.
        fs::write(&staged, "half a copy").unwrap();
        let mut running = File::open(&appimage).unwrap();

        swap_in(&download, &appimage).unwrap();
        assert_eq!(fs::read_to_string(&appimage).unwrap(), "\x7fELF new");
        assert_eq!(mode(&appimage), 0o755);
        assert!(!staged.exists());
        let mut old = String::new();
        running.read_to_string(&mut old).unwrap();
        assert_eq!(old, "\x7fELF old", "the running image keeps its file");

        // Not an executable: refused, and the installed one stays.
        fs::write(&download, "<html>").unwrap();
        assert_eq!(swap_in(&download, &appimage), Err(NOT_THE_APP));
        assert_eq!(fs::read_to_string(&appimage).unwrap(), "\x7fELF new");
        assert!(!staged.exists());

        assert_eq!(
            swap_in(&directory.join("missing"), &appimage),
            Err(COULD_NOT_REPLACE)
        );
        assert!(!staged.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    /// A gzipped tarball of `entries`, each a path, its contents and its mode.
    fn tarball(at: &Path, entries: &[(&str, &[u8], u32)]) {
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(at).unwrap(),
            Compression::fast(),
        ));
        for (path, data, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_entry_type(tar::EntryType::Regular);
            // Written raw, past set_path's own checks, so a hostile path can
            // be made.
            header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn a_tarball_must_be_the_release_it_claims() {
        let directory = scratch("unpack");
        let top = release_directory("9.9.9", "x86_64");
        let file = |name: &str| format!("{top}/{name}");
        let good = directory.join("good.tar.gz");
        tarball(
            &good,
            &[
                (&file("dbdelve"), b"binary", 0o755),
                (&file("install.sh"), b"#!/bin/bash", 0o755),
                (&file("plugins/odbc/manifest.toml"), b"", 0o644),
            ],
        );
        let release = unpack(&good, &directory.join("good"), &top).unwrap();
        assert_eq!(release, directory.join("good").join(&top));
        assert_eq!(mode(&release.join("dbdelve")), 0o755);
        assert!(release.join("plugins/odbc/manifest.toml").is_file());
        // Another version's tarball under this name.
        assert!(
            unpack(
                &good,
                &directory.join("other"),
                &release_directory("9.9.8", "x86_64")
            )
            .is_err()
        );

        for (case, entries) in [
            ("no-binary", vec![(file("install.sh"), &b""[..], 0o755)]),
            ("no-installer", vec![(file("dbdelve"), &b""[..], 0o755)]),
            (
                "two-tops",
                vec![
                    (file("dbdelve"), &b""[..], 0o755),
                    (file("install.sh"), &b""[..], 0o755),
                    ("stray".to_string(), &b""[..], 0o644),
                ],
            ),
            (
                "escape",
                vec![
                    (file("dbdelve"), &b""[..], 0o755),
                    (file("install.sh"), &b""[..], 0o755),
                    (format!("{top}/../../escaped"), &b""[..], 0o644),
                ],
            ),
        ] {
            let at = directory.join(format!("{case}.tar.gz"));
            let entries = entries
                .iter()
                .map(|(path, data, mode)| (path.as_str(), *data, *mode))
                .collect::<Vec<_>>();
            tarball(&at, &entries);
            assert!(unpack(&at, &directory.join(case), &top).is_err(), "{case}");
        }
        assert!(!directory.join("escaped").exists());

        fs::write(directory.join("garbage.tar.gz"), "not gzip").unwrap();
        assert!(
            unpack(
                &directory.join("garbage.tar.gz"),
                &directory.join("garbage"),
                &top
            )
            .is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// The real `assets/linux/install.sh`, run as an update runs it, into a
    /// home of its own over a binary that is "running".
    #[test]
    fn the_bundled_installer_replaces_the_binary_in_place() {
        let directory = scratch("bundled-installer");
        let top = release_directory("9.9.9", "x86_64");
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/linux");
        let installer = fs::read(assets.join("install.sh")).unwrap();
        let desktop = fs::read(assets.join("dbdelve.desktop")).unwrap();
        let mut entries = vec![
            (format!("{top}/dbdelve"), b"new binary".to_vec(), 0o755),
            (format!("{top}/install.sh"), installer, 0o755),
            (format!("{top}/dbdelve.desktop"), desktop, 0o644),
            (
                format!("{top}/plugins/odbc/manifest.toml"),
                b"odbc".to_vec(),
                0o644,
            ),
            (
                format!("{top}/plugins/odbc/bin/dbdelve-odbc"),
                b"bridge".to_vec(),
                0o755,
            ),
        ];
        for size in [16, 32, 64, 128, 256, 512] {
            entries.push((
                format!("{top}/icons/dbdelve-{size}.png"),
                b"png".to_vec(),
                0o644,
            ));
        }
        let at = directory.join(format!("{top}.tar.gz"));
        tarball(
            &at,
            &entries
                .iter()
                .map(|(path, data, mode)| (path.as_str(), data.as_slice(), *mode))
                .collect::<Vec<_>>(),
        );
        let release = unpack(&at, &directory.join("unpacked"), &top).unwrap();

        let home = directory.join("home");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("dbdelve");
        fs::write(&installed, "old binary").unwrap();
        let mut running = File::open(&installed).unwrap();

        run(bundled_installer(&release, &home).env_remove("XDG_DATA_HOME")).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "new binary");
        assert_eq!(mode(&installed), 0o755);
        let mut old = String::new();
        running.read_to_string(&mut old).unwrap();
        assert_eq!(old, "old binary", "the running binary keeps its file");
        assert_eq!(
            fs::read_dir(&bin).unwrap().count(),
            1,
            "nothing but the binary is left in ~/.local/bin"
        );
        let bridge = home.join(".local/share/dbdelve/plugins/odbc/bin/dbdelve-odbc");
        assert_eq!(fs::read_to_string(&bridge).unwrap(), "bridge");
        assert_eq!(mode(&bridge), 0o755);
        let entry =
            fs::read_to_string(home.join(".local/share/applications/dbdelve.desktop")).unwrap();
        assert!(
            entry.contains(&format!("Exec={} ", installed.display())),
            "{entry}"
        );
        assert!(
            home.join(".local/share/icons/hicolor/512x512/apps/dbdelve.png")
                .is_file()
        );

        let data = home.join(".local/share/dbdelve");
        let release_with = |name: &str, bridge: Option<&[u8]>| {
            let at = directory.join(format!("{name}.tar.gz"));
            tarball(
                &at,
                &entries
                    .iter()
                    .filter(|(path, ..)| bridge.is_some() || !path.contains("/plugins/"))
                    .map(|(path, data, mode)| {
                        let data = if path.ends_with("/dbdelve") {
                            &b"newer binary"[..]
                        } else if path.ends_with("/dbdelve-odbc") {
                            bridge.unwrap()
                        } else {
                            data.as_slice()
                        };
                        (path.as_str(), data, *mode)
                    })
                    .collect::<Vec<_>>(),
            );
            unpack(&at, &directory.join(name), &top).unwrap()
        };
        let untouched = |case: &str| {
            assert_eq!(
                fs::read_to_string(&installed).unwrap(),
                "new binary",
                "{case}"
            );
            assert_eq!(fs::read_to_string(&bridge).unwrap(), "bridge", "{case}");
            assert_eq!(fs::read_dir(&bin).unwrap().count(), 1, "{case}");
            let names = |dir: &Path| {
                fs::read_dir(dir)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(&data), ["plugins"], "{case}");
            assert_eq!(names(&data.join("plugins")), ["odbc"], "{case}");
        };

        // A release missing its bridge fails before anything is replaced.
        let broken = release_with("broken", None);
        assert!(run(bundled_installer(&broken, &home).env_remove("XDG_DATA_HOME")).is_err());
        untouched("a failed copy");

        // The binary's rename fails after the bridge's: the old bridge goes
        // back beside the old binary.
        let fake = directory.join("fake-bin");
        fs::create_dir(&fake).unwrap();
        fs::write(
            fake.join("mv"),
            "#!/bin/sh\nfor arg; do case $arg in */.dbdelve.new) exit 1;; esac; done\nexec /bin/mv \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(fake.join("mv"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
        let newer = release_with("newer", Some(b"newer bridge"));
        assert!(
            run(bundled_installer(&newer, &home)
                .env_remove("XDG_DATA_HOME")
                .env("PATH", path))
            .is_err()
        );
        untouched("a failed commit");

        // Killed between moving the old bridge aside and the binary's rename:
        // the next run puts the old bridge back before anything else.
        fs::rename(data.join("plugins/odbc"), data.join(".odbc.old")).unwrap();
        fs::write(bin.join(".dbdelve.new"), "newer binary").unwrap();
        assert!(run(bundled_installer(&broken, &home).env_remove("XDG_DATA_HOME")).is_err());
        untouched("an interrupted commit");

        // Then the newer release goes in whole.
        run(bundled_installer(&newer, &home).env_remove("XDG_DATA_HOME")).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "newer binary");
        assert_eq!(fs::read_to_string(&bridge).unwrap(), "newer bridge");
        assert!(!data.join(".odbc.old").exists() && !data.join(".odbc.new").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}

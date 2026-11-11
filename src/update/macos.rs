use std::{
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    process::Command,
};

use minisign_verify::PublicKey;

use super::{
    Asset, CURRENT, Progress,
    download::{
        ALREADY_RUNNING, COULD_NOT_DOWNLOAD, COULD_NOT_REPLACE, COULD_NOT_UNPACK, NO_SCRATCH,
        NOT_SIGNED, NOT_THE_APP, Scratch, agent, download, failed, fetch_signature, lock,
        release_key, remove, run, verify, writable,
    },
    is_newer,
};

/// The release bundle's id, which `dev/bundle.sh` sets and the Keychain scopes
/// saved passwords to.
const RELEASE_IDENTIFIER: &str = "com.shayanabbas.dbdelve";

const APP_IN_DMG: &str = "DBDelve.app";

/// Everything an install needs, found when the release was: having one is what
/// makes this install eligible to update itself.
pub(crate) struct Installer {
    bundle: PathBuf,
    version: String,
    dmg: Asset,
    key: PublicKey,
}

/// The installer for `version`, when this install may replace itself with it.
/// Each reason it may not is logged once per check.
pub(super) fn installer(version: &str, dmg: Option<Asset>) -> Option<Installer> {
    let Some(dmg) = dmg else {
        tracing::info!(version, "the release has no signed macOS update");
        return None;
    };
    let Some(key) = release_key() else {
        tracing::info!("this build has no release key, so it does not update itself");
        return None;
    };
    let exe = match std::env::current_exe().and_then(fs::canonicalize) {
        Ok(exe) => exe,
        Err(error) => {
            tracing::info!("the running executable could not be found: {error}");
            return None;
        }
    };
    let Some(bundle) = bundle_of(&exe) else {
        tracing::info!(path = %exe.display(), "not running from an app bundle, so no self-update");
        return None;
    };
    if !release_channel(&bundle) {
        tracing::info!(bundle = %bundle.display(), "not the release build, so no self-update");
        return None;
    }
    if let Some(reason) = blocked(&bundle) {
        tracing::info!(bundle = %bundle.display(), "no self-update: {reason}");
        return None;
    }
    Some(Installer {
        bundle,
        version: version.to_string(),
        dmg,
        key,
    })
}

/// `X.app` for an executable at `X.app/Contents/MacOS/<name>`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// A dev or other variant build shares nothing with the release, and must not
/// replace itself with one. A test build may, so the whole path can be run
/// without touching the installed app.
fn release_channel(bundle: &Path) -> bool {
    if cfg!(feature = "update-test-hooks") {
        return true;
    }
    crate::store::is_release_variant()
        && bundle_info(bundle).is_ok_and(|info| info.identifier == RELEASE_IDENTIFIER)
}

/// Why the bundle cannot be swapped where it stands, if it cannot.
fn blocked(bundle: &Path) -> Option<&'static str> {
    // Gatekeeper runs a quarantined app from a randomized read-only copy; the
    // real one is somewhere this process cannot see.
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        return Some("it runs translocated");
    }
    let Some(parent) = bundle.parent() else {
        return Some("it has no parent directory");
    };
    if read_only_volume(bundle) {
        return Some("it is on a read-only volume");
    }
    if !writable(parent) || !writable(bundle) {
        return Some("its folder is not writable");
    }
    None
}

fn read_only_volume(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    let mut volume = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs fills the struct whenever it returns 0, and it is read
    // only then.
    unsafe {
        libc::statfs(path.as_ptr(), volume.as_mut_ptr()) != 0
            || volume.assume_init().f_flags & libc::MNT_RDONLY as u32 != 0
    }
}

/// Downloads, verifies and swaps in the update, and returns the bundle to
/// relaunch. Blocking. On failure nothing is left half-installed, and the
/// reason is in words for the dropdown; the detail goes to the log.
pub(crate) fn install(installer: &Installer, progress: &Progress) -> Result<PathBuf, &'static str> {
    let Installer {
        bundle,
        version,
        dmg,
        key,
    } = installer;
    tracing::info!(version, bundle = %bundle.display(), "update started");
    let _lock = lock(bundle).map_err(failed(ALREADY_RUNNING))?;
    let staged = sibling(bundle, "update-new");
    // What an earlier attempt left when it was killed before it could clean up.
    remove(&staged);

    let scratch = Scratch::new().map_err(failed(NO_SCRATCH))?;
    let file = scratch.0.join(&dmg.name);
    let agent = agent();
    // First, so a release whose signature is missing fails before the
    // download rather than after it.
    let signature =
        fetch_signature(&agent, &dmg.signature_url).map_err(failed(COULD_NOT_DOWNLOAD))?;
    tracing::info!(url = dmg.url, size = dmg.size, "downloading the update");
    download(&agent, &dmg.url, dmg.size, &file, progress).map_err(failed(COULD_NOT_DOWNLOAD))?;
    verify(&file, &signature, key, &dmg.name).map_err(failed(NOT_SIGNED))?;
    tracing::info!("the update's signature verified");

    progress.installing();
    let installed = stage(&file, &scratch.0.join("mnt"), &staged)
        .map_err(failed(COULD_NOT_UNPACK))
        .and_then(|()| check(&staged, bundle, version).map_err(failed(NOT_THE_APP)))
        .and_then(|()| {
            exchange(&staged, bundle)
                .map_err(|error| format!("exchanging the app and the update: {error}"))
                .map_err(failed(COULD_NOT_REPLACE))
        });
    // After the exchange `staged` holds the old app; before it, the update.
    // Either way it is what is left over.
    remove(&staged);
    installed?;
    tracing::info!(version, bundle = %bundle.display(), "update installed");
    Ok(bundle.clone())
}

/// Hidden, and beside the bundle so it is on the same volume as it.
fn sibling(bundle: &Path, tag: &str) -> PathBuf {
    let stem = bundle
        .file_stem()
        .map_or("DBDelve".into(), |stem| stem.to_string_lossy());
    bundle.with_file_name(format!(".{stem}.{tag}.app"))
}

/// Copies the app out of the DMG to `staged`. The image is detached whatever
/// happens, so a failed copy leaves no volume mounted.
fn stage(dmg: &Path, mount: &Path, staged: &Path) -> Result<(), String> {
    fs::create_dir(mount).map_err(|error| error.to_string())?;
    run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(mount)
        .arg(dmg))?;
    let mounted = Mounted(mount);
    let app = mounted.0.join(APP_IN_DMG);
    if !app.is_dir() {
        return Err(format!("the image holds no {APP_IN_DMG}"));
    }
    run(Command::new("/usr/bin/ditto").arg(&app).arg(staged)).map(drop)
}

struct Mounted<'a>(&'a Path);

impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        let detach = |force: bool| {
            let mut command = Command::new("/usr/bin/hdiutil");
            command.arg("detach").arg(self.0);
            if force {
                command.arg("-force");
            }
            run(&mut command)
        };
        if let Err(error) = detach(false).or_else(|_| detach(true)) {
            tracing::warn!(mount = %self.0.display(), "could not detach the update's image: {error}");
        }
    }
}

/// Checks the staged bundle before anything trusts it: its signature seals
/// what is inside, it is the same app, and it is the release that was asked
/// for, which is newer than this one. A validly signed older DMG replayed in
/// place of the release fails the last.
fn check(staged: &Path, installed: &Path, version: &str) -> Result<(), String> {
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(staged))?;
    accept(
        &bundle_info(staged)?,
        &bundle_info(installed)?,
        version,
        CURRENT,
    )
}

#[derive(Debug)]
struct BundleInfo {
    identifier: String,
    version: String,
}

fn bundle_info(bundle: &Path) -> Result<BundleInfo, String> {
    let plist = bundle.join("Contents/Info.plist");
    let read = |key: &str| {
        let output = run(Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist))?;
        Ok::<_, String>(output.trim().to_string())
    };
    Ok(BundleInfo {
        identifier: read("CFBundleIdentifier")?,
        version: read("CFBundleShortVersionString")?,
    })
}

/// `installed` as well as `current`: something else may have put a newer copy
/// in place since this one launched.
fn accept(
    staged: &BundleInfo,
    installed: &BundleInfo,
    version: &str,
    current: &str,
) -> Result<(), String> {
    if staged.identifier != installed.identifier {
        return Err(format!(
            "it is {}, and the installed app is {}",
            staged.identifier, installed.identifier
        ));
    }
    if staged.version != version {
        return Err(format!(
            "it is version {}, and the release is {version}",
            staged.version
        ));
    }
    for (newer_than, which) in [
        (current, "running"),
        (installed.version.as_str(), "installed"),
    ] {
        if !is_newer(&staged.version, newer_than) {
            return Err(format!(
                "its version {} is not newer than the {which} {newer_than}",
                staged.version
            ));
        }
    }
    Ok(())
}

/// Swaps two paths in one atomic step, so no moment exists without an app at
/// its path, whenever this process dies. APFS and HFS+ can; where the volume
/// cannot, the update fails and the release page is the way, since moving the
/// app aside first would leave nothing installed if interrupted.
fn exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    let path = |path: &Path| CString::new(path.as_os_str().as_bytes());
    let (first, second) = (path(first)?, path(second)?);
    // SAFETY: two NUL-terminated paths that outlive the call.
    match unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, path::PathBuf, process::Command};

    use super::{
        BundleInfo, accept, blocked, bundle_info, bundle_of, check, exchange, read_only_volume,
        run, sibling, stage,
    };
    use crate::update::download::tests::scratch;

    #[test]
    fn the_bundle_is_found_from_its_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/DBDelve.app/Contents/MacOS/DBDelve"
            )),
            Some(PathBuf::from("/Applications/DBDelve.app"))
        );
        for loose in [
            "/Users/me/dbdelve/target/release/dbdelve",
            "/Applications/DBDelve/Contents/MacOS/DBDelve",
            "/Applications/DBDelve.app/Contents/Helpers/dbdelve-odbc",
            "/DBDelve",
        ] {
            assert_eq!(bundle_of(Path::new(loose)), None, "{loose}");
        }
    }

    #[test]
    fn a_bundle_it_cannot_swap_is_blocked() {
        assert!(
            blocked(Path::new(
                "/private/var/folders/xy/T/AppTranslocation/0A1B/d/DBDelve.app"
            ))
            .is_some()
        );

        let directory = scratch("blocked");
        let bundle = directory.join("DBDelve.app");
        fs::create_dir(&bundle).unwrap();
        assert_eq!(blocked(&bundle), None);
        assert!(!read_only_volume(&bundle));

        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
        let locked = blocked(&bundle);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(locked, Some("its folder is not writable"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_the_same_app_at_the_release_version_and_newer_is_accepted() {
        let info = |identifier: &str, version: &str| BundleInfo {
            identifier: identifier.into(),
            version: version.into(),
        };
        let id = "com.shayanabbas.dbdelve";
        let installed = info(id, "0.7.1");
        accept(&info(id, "0.8.0"), &installed, "0.8.0", "0.7.1").unwrap();
        let other = info("com.example.other", "0.8.0");
        assert!(accept(&other, &installed, "0.8.0", "0.7.1").is_err());
        assert!(accept(&info(id, "0.7.9"), &installed, "0.8.0", "0.7.1").is_err());
        // A validly signed older release served as the latest: its plist
        // agrees with the feed, and it is still refused.
        assert!(accept(&info(id, "0.7.0"), &installed, "0.7.0", "0.7.1").is_err());
        assert!(accept(&info(id, "0.7.1"), &installed, "0.7.1", "0.7.1").is_err());
        // Newer than this process, older than what was installed since.
        assert!(accept(&info(id, "0.8.0"), &info(id, "0.9.0"), "0.8.0", "0.7.1").is_err());
    }

    fn fake_bundle(at: &Path, identifier: &str, version: &str) {
        fs::create_dir_all(at.join("Contents/MacOS")).unwrap();
        fs::write(
            at.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>{identifier}</string>
	<key>CFBundleExecutable</key><string>DBDelve</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>{version}</string>
</dict>
</plist>
"#
            ),
        )
        .unwrap();
        fs::copy("/usr/bin/true", at.join("Contents/MacOS/DBDelve")).unwrap();
    }

    #[test]
    fn the_plist_is_read_for_identifier_and_version() {
        let directory = scratch("plist");
        let bundle = directory.join("DBDelve.app");
        fake_bundle(&bundle, "com.example.dbdelve", "1.2.3");
        let info = bundle_info(&bundle).unwrap();
        assert_eq!(info.identifier, "com.example.dbdelve");
        assert_eq!(info.version, "1.2.3");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_exchange_puts_the_update_in_place_and_the_app_where_it_was() {
        let directory = scratch("exchange");
        let installed = directory.join("DBDelve.app");
        let staged = sibling(&installed, "update-new");
        assert_eq!(staged, directory.join(".DBDelve.update-new.app"));

        fs::create_dir(&installed).unwrap();
        fs::write(installed.join("version"), "old").unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("version"), "new").unwrap();
        exchange(&staged, &installed).unwrap();
        assert_eq!(
            fs::read_to_string(installed.join("version")).unwrap(),
            "new"
        );
        assert_eq!(fs::read_to_string(staged.join("version")).unwrap(), "old");

        // Nothing staged: it fails, and the app is untouched.
        fs::remove_dir_all(&staged).unwrap();
        assert!(exchange(&staged, &installed).is_err());
        assert_eq!(
            fs::read_to_string(installed.join("version")).unwrap(),
            "new"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// The real tools end to end: a signed bundle in a compressed DMG, staged
    /// out of it and checked, with the image detached afterwards.
    #[test]
    fn a_dmg_is_staged_and_its_app_checked() {
        let directory = scratch("stage");
        let source = directory.join("source");
        let app = source.join("DBDelve.app");
        fake_bundle(&app, "com.example.dbdelve", "999.0.0");
        run(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app))
        .unwrap();
        let dmg = directory.join("DBDelve-999.0.0.dmg");
        run(Command::new("/usr/bin/hdiutil")
            .args([
                "create",
                "-quiet",
                "-volname",
                "DBDelve test",
                "-format",
                "UDZO",
                "-srcfolder",
            ])
            .arg(&source)
            .arg(&dmg))
        .unwrap();

        let running = directory.join("DBDelve.app");
        fake_bundle(&running, "com.example.dbdelve", "0.0.1");
        let staged = sibling(&running, "update-new");
        let mount = directory.join("mnt");
        stage(&dmg, &mount, &staged).unwrap();
        assert!(
            fs::read_dir(&mount).unwrap().next().is_none(),
            "still mounted"
        );
        check(&staged, &running, "999.0.0").unwrap();
        assert!(check(&staged, &running, "998.0.0").is_err());

        // Tampering after signing breaks the seal codesign checks.
        fs::write(staged.join("Contents/MacOS/extra"), "x").unwrap();
        assert!(check(&staged, &running, "999.0.0").is_err());

        // A DMG without the app detaches all the same.
        let empty = directory.join("empty");
        fs::create_dir(&empty).unwrap();
        fs::write(empty.join("readme"), "nothing here").unwrap();
        let hollow = directory.join("hollow.dmg");
        run(Command::new("/usr/bin/hdiutil")
            .args([
                "create",
                "-quiet",
                "-volname",
                "hollow",
                "-format",
                "UDZO",
                "-srcfolder",
            ])
            .arg(&empty)
            .arg(&hollow))
        .unwrap();
        let mount = directory.join("mnt-hollow");
        assert!(stage(&hollow, &mount, &directory.join(".hollow.app")).is_err());
        assert!(
            fs::read_dir(&mount).unwrap().next().is_none(),
            "still mounted"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

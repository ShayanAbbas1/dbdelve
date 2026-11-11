use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read as _, Write as _},
    os::{fd::AsRawFd as _, unix::ffi::OsStrExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use minisign_verify::{PublicKey, Signature};

use super::{CURRENT, Dmg, Progress, is_newer};

/// The base64 line of the release key's `minisign.pub` (the line after the
/// untrusted comment). The release workflow signs every asset with its secret
/// half, `UPDATE_SIGNING_KEY`, and an update that does not verify against this
/// is never installed. While this is not a key, no install updates itself.
const RELEASE_PUBLIC_KEY: &str = "RWS7Oo2qbLFxuFuw9Ygz9Ew/UX5XomznTQHGTHGNRfmLqWPa3RqbChNw";

/// The release bundle's id, which `dev/bundle.sh` sets and the Keychain scopes
/// saved passwords to.
const RELEASE_IDENTIFIER: &str = "com.shayanabbas.dbdelve";

const APP_IN_DMG: &str = "DBDelve.app";

/// A release DMG is tens of megabytes; nothing this large is one.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_SIGNATURE: u64 = 4 * 1024;

const COULD_NOT_DOWNLOAD: &str = "The update could not be downloaded.";
const NOT_SIGNED: &str = "The download's signature did not check out.";
const COULD_NOT_UNPACK: &str = "The downloaded update could not be opened.";
const NOT_THE_APP: &str = "The downloaded app is not the release it should be.";
const COULD_NOT_REPLACE: &str = "The installed app could not be replaced.";
const NO_SCRATCH: &str = "There was nowhere to put the download.";
const ALREADY_RUNNING: &str = "Another DBDelve is already installing an update.";

/// Everything an install needs, found when the release was: having one is what
/// makes this install eligible to update itself.
pub(crate) struct Installer {
    bundle: PathBuf,
    version: String,
    dmg: Dmg,
    key: PublicKey,
}

/// The installer for `version`, when this install may replace itself with it.
/// Each reason it may not is logged once per check.
pub(super) fn installer(version: &str, dmg: Option<Dmg>) -> Option<Installer> {
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

/// Only a test build takes its key from the environment, so a shipped one
/// trusts nothing but the release key.
fn release_key() -> Option<PublicKey> {
    #[cfg(feature = "update-test-hooks")]
    if let Some(key) = std::env::var("DBDELVE_UPDATE_PUBLIC_KEY")
        .ok()
        .filter(|key| !key.is_empty())
    {
        return parse_key(&key);
    }
    parse_key(RELEASE_PUBLIC_KEY)
}

fn parse_key(key: &str) -> Option<PublicKey> {
    PublicKey::from_base64(key.trim()).ok()
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

fn writable(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
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
    let file = scratch.0.join(format!("DBDelve-{version}.dmg"));
    let agent = agent();
    tracing::info!(url = dmg.url, size = dmg.size, "downloading the update");
    download(&agent, &dmg.url, dmg.size, &file, progress).map_err(failed(COULD_NOT_DOWNLOAD))?;
    let signature =
        fetch_signature(&agent, &dmg.signature_url).map_err(failed(COULD_NOT_DOWNLOAD))?;
    verify(&file, &signature, key).map_err(failed(NOT_SIGNED))?;
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

/// Held for the length of an install, so a second DBDelve process cannot
/// clear the first one's staged copy as a leftover. Released when the
/// returned file closes.
fn lock(bundle: &Path) -> Result<File, String> {
    let parent = bundle.parent().ok_or("the app has no parent directory")?;
    let directory = File::open(parent).map_err(|error| error.to_string())?;
    // SAFETY: a descriptor this function owns, for the length of the call.
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(directory)
}

fn failed(reason: &'static str) -> impl FnOnce(String) -> &'static str {
    move |detail| {
        tracing::warn!("update failed: {reason} {detail}");
        reason
    }
}

/// Hidden, and beside the bundle so it is on the same volume as it.
fn sibling(bundle: &Path, tag: &str) -> PathBuf {
    let stem = bundle
        .file_stem()
        .map_or("DBDelve".into(), |stem| stem.to_string_lossy());
    bundle.with_file_name(format!(".{stem}.{tag}.app"))
}

fn remove(path: &Path) {
    if path.symlink_metadata().is_err() {
        return;
    }
    if let Err(error) = fs::remove_dir_all(path) {
        tracing::warn!(path = %path.display(), "could not remove it: {error}");
    }
}

/// A fresh directory of its own, gone with everything in it when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path =
            std::env::temp_dir().join(format!("dbdelve-update-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove(&self.0);
    }
}

fn agent() -> ureq::Agent {
    // No global timeout: a slow line takes as long as it takes, and only a
    // stalled one is cut off.
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30 * 60)))
        .user_agent(concat!("dbdelve/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn download(
    agent: &ureq::Agent,
    url: &str,
    size: u64,
    to: &Path,
    progress: &Progress,
) -> Result<(), String> {
    if size > MAX_DOWNLOAD {
        return Err(format!("the release lists it at {size} bytes"));
    }
    let mut response = agent.get(url).call().map_err(|error| error.to_string())?;
    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .reader();
    let mut file = File::create_new(to).map_err(|error| error.to_string())?;
    let mut buffer = vec![0; 256 * 1024];
    let mut received = 0;
    progress.downloaded(0, size);
    loop {
        let read = body.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        received += read as u64;
        if received > size {
            return Err(format!(
                "it is longer than the {size} bytes the release lists"
            ));
        }
        file.write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
        progress.downloaded(received, size);
    }
    if received != size {
        return Err(format!(
            "{received} of the {size} bytes the release lists arrived"
        ));
    }
    file.sync_all().map_err(|error| error.to_string())
}

fn fetch_signature(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    agent
        .get(url)
        .call()
        .and_then(|mut response| {
            response
                .body_mut()
                .with_config()
                .limit(MAX_SIGNATURE)
                .read_to_string()
        })
        .map_err(|error| format!("its signature: {error}"))
}

/// Prehashed signatures only, which is what minisign has made by default
/// since 0.10; the release workflow checks it made one.
fn verify(file: &Path, signature: &str, key: &PublicKey) -> Result<(), String> {
    let signature = Signature::decode(signature).map_err(|error| error.to_string())?;
    let mut verifier = key
        .verify_stream(&signature)
        .map_err(|error| error.to_string())?;
    let mut file = File::open(file).map_err(|error| error.to_string())?;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
    }
    verifier.finalize().map_err(|error| error.to_string())
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

/// Stdout on success; on failure, the exit status and what the tool said.
fn run(command: &mut Command) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, path::PathBuf, process::Command};

    use minisign_verify::PublicKey;

    use super::{
        BundleInfo, accept, blocked, bundle_info, bundle_of, check, exchange, lock, parse_key,
        read_only_volume, run, sibling, stage, verify,
    };

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-update-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// A throwaway key, and the minisign signature of `data` under it.
    fn signed(data: &[u8]) -> (PublicKey, String) {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(Some(&pk), &sk, data, None, None).unwrap();
        (
            PublicKey::from_base64(&pk.to_base64()).unwrap(),
            signature.into_string(),
        )
    }

    #[test]
    fn a_signature_verifies_only_the_file_it_signed_under_its_own_key() {
        let directory = scratch("verify");
        let file = directory.join("DBDelve-9.9.9.dmg");
        fs::write(&file, b"the update").unwrap();
        let (key, signature) = signed(b"the update");
        verify(&file, &signature, &key).unwrap();

        fs::write(&file, b"the updatE").unwrap();
        assert!(verify(&file, &signature, &key).is_err());

        fs::write(&file, b"the update").unwrap();
        let (other_key, _) = signed(b"the update");
        assert!(verify(&file, &signature, &other_key).is_err());

        assert!(verify(&file, "not a signature", &key).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_placeholder_is_no_key() {
        assert!(parse_key("REPLACE_WITH_RELEASE_PUBLIC_KEY").is_none());
        assert!(parse_key("").is_none());
        // The public key minisign-verify's own tests use.
        assert!(parse_key("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3").is_some());
        assert!(parse_key(super::RELEASE_PUBLIC_KEY).is_some());
    }

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

    #[test]
    fn one_install_at_a_time_per_folder() {
        let directory = scratch("lock");
        let bundle = directory.join("DBDelve.app");
        let held = lock(&bundle).unwrap();
        assert!(lock(&bundle).is_err());
        drop(held);
        lock(&bundle).unwrap();
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

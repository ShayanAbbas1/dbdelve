use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read as _, Write as _},
    os::{
        fd::AsRawFd as _,
        unix::{ffi::OsStrExt as _, fs::DirBuilderExt as _},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use minisign_verify::{PublicKey, Signature};

use super::Progress;

/// The base64 line of the release key's `minisign.pub` (the line after the
/// untrusted comment). The release workflow signs every asset with its secret
/// half, `UPDATE_SIGNING_KEY`, and an update that does not verify against this
/// is never installed. While this is not a key, no install updates itself.
const RELEASE_PUBLIC_KEY: &str = "RWS7Oo2qbLFxuFuw9Ygz9Ew/UX5XomznTQHGTHGNRfmLqWPa3RqbChNw";

/// A release asset is tens of megabytes; nothing this large is one.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_SIGNATURE: u64 = 4 * 1024;

pub(super) const COULD_NOT_DOWNLOAD: &str = "The update could not be downloaded.";
pub(super) const NOT_SIGNED: &str = "The download's signature did not check out.";
pub(super) const COULD_NOT_UNPACK: &str = "The downloaded update could not be opened.";
pub(super) const NOT_THE_APP: &str = "The downloaded app is not the release it should be.";
pub(super) const COULD_NOT_REPLACE: &str = "The installed app could not be replaced.";
pub(super) const NO_SCRATCH: &str = "There was nowhere to put the download.";
pub(super) const ALREADY_RUNNING: &str = "Another DBDelve is already installing an update.";

/// Only a test build takes its key from the environment, so a shipped one
/// trusts nothing but the release key.
pub(super) fn release_key() -> Option<PublicKey> {
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

pub(super) fn writable(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Held for the length of an install, so a second DBDelve process cannot
/// clear the first one's staged copy as a leftover. Released when the
/// returned file closes.
pub(super) fn lock(installed: &Path) -> Result<File, String> {
    let parent = installed
        .parent()
        .ok_or("the app has no parent directory")?;
    let directory = File::open(parent).map_err(|error| error.to_string())?;
    // SAFETY: a descriptor this function owns, for the length of the call.
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(directory)
}

pub(super) fn failed(reason: &'static str) -> impl FnOnce(String) -> &'static str {
    move |detail| {
        tracing::warn!("update failed: {reason} {detail}");
        reason
    }
}

pub(super) fn remove(path: &Path) {
    let Ok(metadata) = path.symlink_metadata() else {
        return;
    };
    let removed = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    if let Err(error) = removed {
        tracing::warn!(path = %path.display(), "could not remove it: {error}");
    }
}

/// A fresh directory of its own, gone with everything in it when dropped.
/// Only this user can enter it: what is verified in it is what gets installed,
/// so nobody else may swap a file in between.
pub(super) struct Scratch(pub(super) PathBuf);

impl Scratch {
    pub(super) fn new() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path =
            std::env::temp_dir().join(format!("dbdelve-update-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove(&self.0);
    }
}

pub(super) fn agent() -> ureq::Agent {
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

pub(super) fn download(
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

pub(super) fn fetch_signature(agent: &ureq::Agent, url: &str) -> Result<String, String> {
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
///
/// The signature must also name `asset` as the file it signed. minisign's
/// default trusted comment, `timestamp:<n>\tfile:<name>\thashed`, is covered by
/// the signature, and the name carries the version: without this, an older
/// release's asset and its genuine signature, served under the new name,
/// would verify.
pub(super) fn verify(
    file: &Path,
    signature: &str,
    key: &PublicKey,
    asset: &str,
) -> Result<(), String> {
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
    verifier.finalize().map_err(|error| error.to_string())?;
    let comment = signature.trusted_comment();
    match comment
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"))
    {
        Some(signed) if signed == asset => Ok(()),
        _ => Err(format!("it was signed as {comment:?}, not as {asset}")),
    }
}

/// Stdout on success; on failure, the exit status and what the tool said.
pub(super) fn run(command: &mut Command) -> Result<String, String> {
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
pub(super) mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf};

    use minisign_verify::PublicKey;

    use super::{Scratch, lock, parse_key, remove, verify};

    pub(in crate::update) fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-update-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// A throwaway key, and the minisign signature of `data` under it with the
    /// trusted comment minisign writes for a file called `name`.
    pub(in crate::update) fn signed(data: &[u8], name: &str) -> (PublicKey, String) {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let comment = format!("timestamp:1700000000\tfile:{name}\thashed");
        let signature = minisign::sign(Some(&pk), &sk, data, Some(&comment), None).unwrap();
        (
            PublicKey::from_base64(&pk.to_base64()).unwrap(),
            signature.into_string(),
        )
    }

    #[test]
    fn a_signature_verifies_only_the_file_it_signed_under_its_own_key() {
        let directory = scratch("verify");
        let name = "DBDelve-9.9.9.dmg";
        let file = directory.join(name);
        fs::write(&file, b"the update").unwrap();
        let (key, signature) = signed(b"the update", name);
        verify(&file, &signature, &key, name).unwrap();

        fs::write(&file, b"the updatE").unwrap();
        assert!(verify(&file, &signature, &key, name).is_err());

        fs::write(&file, b"the update").unwrap();
        let (other_key, _) = signed(b"the update", name);
        assert!(verify(&file, &signature, &other_key, name).is_err());

        assert!(verify(&file, "not a signature", &key, name).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_signature_binds_the_asset_name_it_was_made_for() {
        let directory = scratch("verify-name");
        let old = "dbdelve-0.7.0-x86_64.AppImage";
        let new = "dbdelve-0.8.0-x86_64.AppImage";
        let file = directory.join(new);
        // The older release's file and its genuine signature, served as the
        // new release.
        fs::write(&file, b"the old release").unwrap();
        let (key, signature) = signed(b"the old release", old);
        verify(&file, &signature, &key, old).unwrap();
        assert!(verify(&file, &signature, &key, new).is_err());
        // A name that only starts the same is another file.
        assert!(verify(&file, &signature, &key, "dbdelve-0.7.0-x86_64.App").is_err());

        // A signature with no file named in its comment binds nothing.
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let bare = minisign::sign(Some(&pk), &sk, &b"the old release"[..], None, None)
            .unwrap()
            .into_string();
        let key = PublicKey::from_base64(&pk.to_base64()).unwrap();
        assert!(verify(&file, &bare, &key, new).is_err());

        // The comment is covered by the signature: editing it breaks it.
        let (key, signature) = signed(b"the old release", old);
        let forged = signature.replace(old, new);
        assert!(verify(&file, &forged, &key, new).is_err());
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
    fn one_install_at_a_time_per_folder() {
        let directory = scratch("lock");
        let bundle = directory.join("DBDelve.app");
        let held = lock(&bundle).unwrap();
        assert!(lock(&bundle).is_err());
        drop(held);
        lock(&bundle).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_scratch_directory_is_private_and_goes_when_dropped() {
        let scratch = Scratch::new().unwrap();
        let path = scratch.0.clone();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(path.join("download"), "x").unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    #[test]
    fn a_leftover_is_removed_whether_file_or_folder() {
        let directory = scratch("remove");
        let file = directory.join(".dbdelve.AppImage.update-new");
        fs::write(&file, "half a copy").unwrap();
        remove(&file);
        assert!(!file.exists());
        let folder = directory.join(".DBDelve.update-new.app");
        fs::create_dir_all(folder.join("Contents")).unwrap();
        remove(&folder);
        assert!(!folder.exists());
        remove(&directory.join("never-there"));
        fs::remove_dir_all(directory).unwrap();
    }
}

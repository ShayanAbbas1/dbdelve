//! The echo plugin the host's tests drive, built on first use.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::db::plugin::Plugin;

/// Unit tests get no `CARGO_BIN_EXE_*` for another package's binary, and
/// artifact dependencies are nightly-only, so the first test to ask builds
/// echo into this test binary's own target directory and profile: plain
/// `cargo test` stays self-contained on a fresh checkout, and cargo's build
/// lock serializes nextest's parallel processes.
pub(super) fn plugin() -> &'static Plugin {
    static PLUGIN: OnceLock<&'static Plugin> = OnceLock::new();
    PLUGIN.get_or_init(|| {
        let test = std::env::current_exe().expect("the test binary has a path");
        // target/<profile>/deps/<test binary>
        let profile_dir = test.parent().and_then(Path::parent).expect("under target/");
        let target_dir = profile_dir.parent().expect("under target/");
        let profile = match profile_dir.file_name().and_then(|name| name.to_str()) {
            Some("debug") => "dev",
            Some(profile) => profile,
            None => panic!("no profile directory above {}", test.display()),
        };
        let built = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "-p",
                "dbdelve-plugin-echo",
                "--profile",
                profile,
            ])
            .arg("--manifest-path")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .arg("--target-dir")
            .arg(target_dir)
            .output()
            .expect("cargo runs");
        assert!(
            built.status.success(),
            "building echo failed:\n{}",
            String::from_utf8_lossy(&built.stderr)
        );
        Box::leak(Box::new(Plugin {
            id: "echo".to_owned(),
            name: "Echo".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executable: profile_dir.join(format!("dbdelve-echo{}", std::env::consts::EXE_SUFFIX)),
        }))
    })
}

/// Whether a process with this id exists. On Unix that counts one exited but
/// not yet reaped, so gone also proves the host reaped it.
#[cfg(unix)]
pub(super) fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 checks for the process and delivers nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
pub(super) fn alive(pid: u32) -> bool {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;

    // SAFETY: a null handle is checked before use, and a non-null one is a
    // fresh handle this function owns and closes.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let handle = OwnedHandle::from_raw_handle(handle);
        let mut code = 0;
        GetExitCodeProcess(
            std::os::windows::io::AsRawHandle::as_raw_handle(&handle),
            &mut code,
        ) != 0
            && code == STILL_ACTIVE
    }
}

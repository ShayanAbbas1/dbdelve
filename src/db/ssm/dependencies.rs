//! Local tool checks. No credentials or AWS API calls are needed.
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Found(PathBuf),
    Missing,
    WrongExecutable(String),
    Unavailable(String),
}

pub(crate) fn check(aws: Option<&Path>, plugin: Option<&Path>) -> [Status; 2] {
    let result = [
        check_tool(aws, "aws"),
        check_tool(plugin, "session-manager-plugin"),
    ];
    #[cfg(windows)]
    if let [Status::Found(aws), Status::Found(plugin)] = &result
        && let Err(error) = super::windows_plugin_lookup(aws, plugin)
    {
        return [result[0].clone(), Status::Unavailable(error.message)];
    }
    result
}

fn check_tool(explicit: Option<&Path>, name: &str) -> Status {
    if explicit.is_some_and(|path| !path.is_absolute()) {
        return Status::Unavailable("Choose an absolute executable path.".into());
    }
    if name == "session-manager-plugin"
        && explicit.is_some_and(|path| {
            path.file_name().is_none_or(|file| {
                file != if cfg!(windows) {
                    "session-manager-plugin.exe"
                } else {
                    name
                }
            })
        })
    {
        return Status::WrongExecutable("Choose the session-manager-plugin executable.".into());
    }
    let Ok(path) = super::executable(explicit, name) else {
        return Status::Missing;
    };
    match probe(&path, name, Duration::from_secs(3)) {
        Ok(true) => Status::Found(path),
        Ok(false) => Status::WrongExecutable(if name == "aws" {
            "Not recognized as AWS CLI. Choose the aws executable.".into()
        } else {
            "Not recognized as Session Manager plugin. Choose the session-manager-plugin executable.".into()
        }),
        Err(message) => Status::Unavailable(message.into()),
    }
}

fn probe(path: &Path, name: &str, timeout: Duration) -> Result<bool, &'static str> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("AWS_PAGER", "")
        .env("AWS_CLI_AUTO_PROMPT", "off");
    // Reuse the tunnel watchdog so timeout and owner exit clean up descendants
    // too, including Windows children born before a job could be assigned.
    let mut process = super::process::Process::spawn(command)
        .map_err(|_| "Could not start. Check executable permissions and installation.")?;
    let child = process.child.as_mut().expect("live probe");
    let (send, receive) = mpsc::channel();
    for mut output in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let send = send.clone();
        thread::spawn(move || {
            // Bound retained output, but keep draining so a noisy tool cannot
            // block on a full pipe. The caller's deadline also covers EOF.
            let mut bytes = Vec::new();
            let result = output
                .by_ref()
                .take(16 * 1024 + 1)
                .read_to_end(&mut bytes)
                .and_then(|_| std::io::copy(&mut output, &mut std::io::sink()));
            let _ = send.send(result.map(|_| bytes));
        });
    }
    drop(send);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(
                        "Version check failed. Repair the installation or choose another executable.",
                    );
                }
                let mut recognized = false;
                for _ in 0..2 {
                    let bytes = receive
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .map_err(|_| "Version check timed out while reading output.")?
                        .map_err(|_| "Could not read executable version output.")?;
                    if bytes.len() > 16 * 1024 {
                        return Err("Version output was too large. Choose the correct executable.");
                    }
                    recognized |= version_matches(name, &bytes);
                }
                return Ok(recognized);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                process.stop();
                return Err(
                    "Version check timed out or could not complete. Check the installation and try again.",
                );
            }
        }
    }
}

// AWS CLI v1 can report its version on stderr; v2 uses stdout. The plugin
// prints only a four-part version, so its executable-name check also matters.
fn version_matches(name: &str, bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // The process watchdog appends its own exit marker to stdout.
    let text = text
        .trim()
        .strip_suffix("DBDELVE_SSM_CHILD_EXITED")
        .unwrap_or(text)
        .trim();
    let numeric_version = |version: &str, parts: usize| {
        let mut count = 0;
        version.split('.').all(|part| {
            count += 1;
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
        }) && count == parts
    };
    match name {
        "aws" => text
            .split_whitespace()
            .next()
            .and_then(|word| word.strip_prefix("aws-cli/"))
            .is_some_and(|version| numeric_version(version, 3)),
        "session-manager-plugin" => numeric_version(text.trim(), 4),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssm_dependency_paths_do_not_fall_back() {
        assert_eq!(
            check_tool(
                Some(&std::env::temp_dir().join("definitely-missing-dbdelve-tool/aws")),
                "aws"
            ),
            Status::Missing
        );
        assert!(matches!(
            check_tool(Some(Path::new("relative/aws")), "aws"),
            Status::Unavailable(_)
        ));
        assert!(matches!(
            check_tool(
                Some(&std::env::temp_dir().join("wrong-plugin")),
                "session-manager-plugin"
            ),
            Status::WrongExecutable(_)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn ssm_dependency_rejects_wrong_tools_even_when_they_exit_successfully() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("dbdelve-probe-identity-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["aws", "session-manager-plugin"] {
            let path = root.join(name);
            for output in [
                "",
                "2.0.77\nabcdef123456\narm64",
                "Cursor 2.0.77",
                "aws-cli/not-a-version",
                "1.2.3",
            ] {
                std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
                assert!(
                    matches!(check_tool(Some(&path), name), Status::WrongExecutable(_)),
                    "{name} accepted {output:?}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ssm_dependency_version_formats_are_tool_specific() {
        for (name, output, expected) in [
            (
                "aws",
                "aws-cli/2.15.9 Python/3.11.6 Darwin/22.6.0 exe/x86_64 prompt/off\n",
                true,
            ),
            (
                "aws",
                "aws-cli/1.32.0 Python/3.11.6 Linux/6.1 botocore/1.34.0\r\n",
                true,
            ),
            (
                "aws",
                "aws-cli/2.15.9 Python/3.11.6 Windows/10 exe/AMD64\r\n",
                true,
            ),
            ("aws", "aws-cli/not-a-version", false),
            ("aws", "not-aws-cli/2.15.9", false),
            ("aws", "1.2.707.0", false),
            ("session-manager-plugin", "1.2.707.0\r\n", true),
            ("session-manager-plugin", "1.3.0.0\n", true),
            (
                "session-manager-plugin",
                "1.2.707.0\nDBDELVE_SSM_CHILD_EXITED\n",
                true,
            ),
            (
                "session-manager-plugin",
                "DBDELVE_SSM_CHILD_EXITED\n",
                false,
            ),
            ("session-manager-plugin", "1.2.707", false),
            ("session-manager-plugin", "1.2.707.0\nother tool", false),
            ("session-manager-plugin", "aws-cli/2.15.9", false),
        ] {
            assert_eq!(
                version_matches(name, output.as_bytes()),
                expected,
                "{name}: {output}"
            );
        }
        assert!(!version_matches("aws", &[0xff]));
    }

    #[cfg(unix)]
    #[test]
    fn ssm_dependency_probe_validates_stdout_stderr_and_exit_status() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("dbdelve-probe-output-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (name, output) in [
            ("aws", "aws-cli/2.15.9 Python/3.11.6 Darwin/22.6.0"),
            ("aws", "aws-cli/1.32.0 Python/3.11.6 Linux/6.1"),
            ("session-manager-plugin", "1.2.707.0"),
        ] {
            let path = root.join(name);
            for stream in ["1", "2"] {
                for exit in [0, 1] {
                    std::fs::write(&path, format!("#!/bin/sh\n[ \"$1\" = --version ] || exit 1\nprintf '%s\\n' '{output}' >&{stream}\nexit {exit}\n")).unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                    let result = check_tool(Some(&path), name);
                    assert_eq!(
                        matches!(result, Status::Found(_)),
                        exit == 0,
                        "{name}, stream {stream}, exit {exit}: {result:?}"
                    );
                }
            }
        }
        // Both pipes must be drained without unbounded allocation or blocking.
        let path = root.join("aws");
        std::fs::write(&path, "#!/bin/sh\nprintf 'aws-cli/2.15.9\\n'\nhead -c 100000 /dev/zero\nhead -c 100000 /dev/zero >&2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            check_tool(Some(&path), "aws"),
            Status::Unavailable(_)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ssm_dependency_timeout_stops_descendants() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("dbdelve-probe-tree-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("aws");
        let pid_file = root.join("child-pid");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nsleep 60 &\necho $! > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Long enough for a loaded runner to start the script and write the
        // pid; the script sleeps far longer, so the timeout still fires.
        assert!(probe(&path, "aws", Duration::from_secs(5)).is_err());
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("probe descendant did not start; cleanup was not tested")
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "probe descendant survived timeout"
            );
            thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn ssm_dependency_probe_checks_exit_and_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("dbdelve-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("aws");
        for (script, success) in [
            ("[ \"$1\" = --version ] && echo aws-cli/2.15.9", true),
            ("exit 1", false),
            ("exec sleep 30", false),
        ] {
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let start = Instant::now();
            let timeout = if script.starts_with("exec") {
                Duration::from_millis(150)
            } else {
                Duration::from_secs(2)
            };
            let result = probe(&path, "aws", timeout);
            assert_eq!(result.is_ok(), success, "{result:?}");
            assert!(start.elapsed() < Duration::from_secs(3));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            check_tool(Some(&path), "aws"),
            Status::Unavailable(_)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}

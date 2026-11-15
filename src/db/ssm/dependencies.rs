//! Local tool checks. No credentials or AWS API calls are needed.
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Found(PathBuf),
    Missing,
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
        return Status::Unavailable("Choose the session-manager-plugin executable.".into());
    }
    let Ok(path) = super::executable(explicit, name) else {
        return Status::Missing;
    };
    match probe(&path, Duration::from_secs(3)) {
        Ok(()) => Status::Found(path),
        Err(message) => Status::Unavailable(message.into()),
    }
}

fn probe(path: &Path, timeout: Duration) -> Result<(), &'static str> {
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
    for mut output in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        thread::spawn(move || {
            let _ = std::io::copy(&mut output, &mut std::io::sink());
        });
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(
                        "Version check failed. Repair the installation or choose another executable.",
                    )
                };
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
            check_tool(Some(Path::new("/wrong-plugin")), "session-manager-plugin"),
            Status::Unavailable(_)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn ssm_dependency_timeout_stops_descendants() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("dbdelve-probe-tree-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("aws");
        let marker = root.join("survived");
        let started = root.join("started");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n(touch '{}'; sleep 2; touch '{}') &\nwait\n",
                started.display(),
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(probe(&path, Duration::from_millis(500)).is_err());
        thread::sleep(Duration::from_secs(3));
        let did_start = started.exists();
        let survived = marker.exists();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            did_start,
            "probe descendant did not start; cleanup was not tested"
        );
        assert!(!survived, "probe descendant survived timeout");
    }
    #[cfg(unix)]
    #[test]
    fn ssm_dependency_probe_checks_exit_and_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("dbdelve-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("aws");
        for (script, success) in [
            ("[ \"$1\" = --version ]", true),
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
            let result = probe(&path, timeout);
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

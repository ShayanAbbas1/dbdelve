//! A local port forward over the system `ssh` binary.
//!
//! The binary rather than an SSH library, because it brings `~/.ssh/config`
//! (aliases, `ProxyJump`, `IdentityFile`, `User`), ssh-agent, agent-backed
//! keys and hardware keys along with it. The engines dial [`Tunnel::local_addr`]
//! and never learn that a tunnel is there.

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{DbError, SshTunnel, plain_error};

/// Long enough for a hardware key waiting on a touch.
const READY_WITHIN: Duration = Duration::from_secs(30);

/// Attempts at a local port, for when something else takes the one we picked
/// between releasing it and ssh binding it.
const ATTEMPTS: usize = 3;

/// How much of ssh's stderr is kept. Its error is the last thing it says.
const STDERR_TAIL: usize = 8 * 1024;

/// What ssh prints when `ExitOnForwardFailure` finds the local port taken.
const FORWARD_FAILED: &str = "Could not request local forwarding";

/// A running `ssh -L`. Dropping it ends ssh and returns once it has exited.
pub struct Tunnel {
    local: SocketAddr,
    process: Child,
    /// Created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the OS ends ssh
    /// when this handle closes, however DBDelve exits.
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl Tunnel {
    /// Forward a local port to `target_host:target_port` as seen from the SSH
    /// host.
    pub fn open(ssh: &SshTunnel, target_host: &str, target_port: u16) -> Result<Self, DbError> {
        let mut attempt = 1;
        loop {
            let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .and_then(|listener| listener.local_addr())
                .map_err(|error| {
                    plain_error(format!(
                        "Could not reserve a local port for the SSH tunnel: {error}"
                    ))
                })?
                .port();
            let mut args = ssh_args(ssh, port, target_host, target_port)?;
            // Tests never read the real ~/.ssh.
            if cfg!(test)
                && let Ok(config) = std::env::var("dbdelve_SSH_CONFIG")
            {
                args.splice(0..0, ["-F".to_owned(), config]);
            }

            let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut tunnel = spawn(local, &args).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => no_ssh(),
                _ => plain_error(format!("Could not start ssh: {error}")),
            })?;
            let stderr = drain(tunnel.process.stderr.take().expect("stderr is piped"));

            match wait_ready(&mut tunnel.process, local) {
                Ready::Listening => return Ok(tunnel),
                Ready::Exited(status) => {
                    let stderr = stderr.join().unwrap_or_default();
                    let stderr = String::from_utf8_lossy(&stderr);
                    if stderr.contains(FORWARD_FAILED) && attempt < ATTEMPTS {
                        attempt += 1;
                        continue;
                    }
                    return Err(failure(&ssh.host, status, &stderr));
                }
                Ready::TimedOut => {
                    return Err(plain_error(format!(
                        "SSH tunnel through {} did not come up within {} seconds.",
                        ssh.host,
                        READY_WITHIN.as_secs()
                    )));
                }
            }
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // Closing the watchdog's stdin is the same teardown a crash gets.
        #[cfg(unix)]
        drop(self.process.stdin.take());
        #[cfg(windows)]
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// ssh's arguments, bar the program name.
fn ssh_args(
    ssh: &SshTunnel,
    local_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<Vec<String>, DbError> {
    if ssh.host.starts_with('-') {
        return Err(plain_error(format!(
            "SSH host {} starts with '-', which ssh would read as an option.",
            ssh.host
        )));
    }
    let target_host = if target_host.contains(':') && !target_host.starts_with('[') {
        format!("[{target_host}]")
    } else {
        target_host.to_owned()
    };

    let mut args: Vec<String> = [
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        // Otherwise accept-new's "Permanently added" warning lands in stderr.
        "-o",
        "LogLevel=ERROR",
        "-L",
    ]
    .map(String::from)
    .into();
    args.push(format!(
        "127.0.0.1:{local_port}:{target_host}:{target_port}"
    ));
    if let Some(port) = ssh.port {
        args.extend(["-p".to_owned(), port.to_string()]);
    }
    if !ssh.user.is_empty() {
        args.extend(["-l".to_owned(), ssh.user.clone()]);
    }
    if let Some(identity) = ssh.identity_file.as_ref().filter(|path| !path.is_empty()) {
        args.extend(["-i".to_owned(), identity.clone()]);
    }
    args.extend(["--".to_owned(), ssh.host.clone()]);
    Ok(args)
}

enum Ready {
    Listening,
    Exited(ExitStatus),
    TimedOut,
}

fn wait_ready(process: &mut Child, local: SocketAddr) -> Ready {
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = process.try_wait() {
            return Ready::Exited(status);
        }
        // ponytail: the probe is one forwarded connection opened and closed
        // before the engine's, so the database logs a connection that sent
        // nothing (Postgres "incomplete startup packet"; MySQL counts it toward
        // max_connect_errors until the real connect resets it), and a stranger
        // listening on the port before ssh binds it passes too. Upgrade: run
        // ssh with -v and wait for its "Local forwarding listening" line.
        if TcpStream::connect_timeout(&local, Duration::from_secs(1)).is_ok() {
            return Ready::Listening;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ready::TimedOut
}

fn failure(host: &str, status: ExitStatus, stderr: &str) -> DbError {
    // The watchdog shell's "command not found".
    if cfg!(unix) && status.code() == Some(127) {
        return no_ssh();
    }
    let stderr = stderr.trim().replace("\r\n", "\n");
    if stderr.is_empty() {
        plain_error(format!(
            "SSH tunnel through {host} failed: ssh exited ({status})."
        ))
    } else {
        plain_error(format!("SSH tunnel through {host} failed: {stderr}"))
    }
}

fn no_ssh() -> DbError {
    plain_error("No ssh executable was found.".to_owned())
}

/// Reads stderr to the end, so ssh never blocks on a full pipe, keeping the
/// last [`STDERR_TAIL`] bytes.
fn drain(mut stderr: ChildStderr) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    tail.extend_from_slice(&chunk[..read]);
                    tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        tail
    })
}

/// ssh runs under a shell holding the read end of a pipe on its stdin, whose
/// write end only DBDelve holds. When DBDelve exits, however it exits, the
/// pipe closes, `read` returns, and the shell kills ssh. The argv arrives as
/// `"$@"`, never spliced into the script. `exec 3<&0` because POSIX hands an
/// asynchronous list /dev/null as its stdin; the shell exits with ssh's status.
#[cfg(unix)]
const WATCHDOG: &str = "exec 3<&0; \"$@\" </dev/null & pid=$!; \
    (read _ <&3; kill $pid) >/dev/null 2>&1 & watcher=$!; \
    wait $pid; status=$?; kill $watcher 2>/dev/null; exit $status";

#[cfg(unix)]
fn spawn(local: SocketAddr, args: &[String]) -> io::Result<Tunnel> {
    Ok(Tunnel {
        local,
        process: watched("ssh", args)?,
    })
}

#[cfg(unix)]
fn watched(program: &str, args: &[String]) -> io::Result<Child> {
    Command::new("/bin/sh")
        .args(["-c", WATCHDOG, "sh", program])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
}

#[cfg(windows)]
fn spawn(local: SocketAddr, args: &[String]) -> io::Result<Tunnel> {
    use std::os::windows::process::CommandExt;
    use std::path::Path;

    let command = |program: &Path| {
        let mut command = Command::new(program);
        command
            .args(args)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    };
    let mut process = match command(Path::new("ssh")).spawn() {
        // Windows' own OpenSSH lives here, and is not always on PATH.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            command(&Path::new(&root).join(r"System32\OpenSSH\ssh.exe")).spawn()?
        }
        spawned => spawned?,
    };
    match kill_on_close_job(&process) {
        Ok(job) => Ok(Tunnel {
            local,
            process,
            _job: job,
        }),
        Err(error) => {
            let _ = process.kill();
            let _ = process.wait();
            Err(error)
        }
    }
}

// ponytail: ssh runs for a moment before it joins the job, so a DBDelve killed
// in that window leaves it behind. Upgrade: spawn suspended, assign, resume.
#[cfg(windows)]
fn kill_on_close_job(process: &Child) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: null security attributes and a null name are documented as
    // valid, and a non-null handle is a fresh one nothing else owns.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: both handles are open for the length of the calls, and the
    // pointer and size describe `limits`.
    let assigned = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        ) != 0
            && AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) != 0
    };
    if !assigned {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(host: &str) -> SshTunnel {
        SshTunnel {
            host: host.to_owned(),
            ..SshTunnel::default()
        }
    }

    const OPTIONS: [&str; 14] = [
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "LogLevel=ERROR",
    ];

    #[test]
    fn what_the_config_leaves_blank_is_left_to_ssh() {
        let args = ssh_args(&tunnel("bastion"), 40000, "db.internal", 5432).unwrap();
        let mut expected = OPTIONS.to_vec();
        expected.extend(["-L", "127.0.0.1:40000:db.internal:5432", "--", "bastion"]);
        assert_eq!(args, expected);

        let blank_identity = SshTunnel {
            identity_file: Some(String::new()),
            ..tunnel("bastion")
        };
        assert_eq!(
            ssh_args(&blank_identity, 40000, "db.internal", 5432).unwrap(),
            expected
        );
    }

    #[test]
    fn every_field_set_reaches_ssh_before_the_host() {
        let ssh = SshTunnel {
            host: "bastion.example.com".to_owned(),
            port: Some(2222),
            user: "deploy@corp".to_owned(),
            identity_file: Some("~/.ssh/id_ed25519".to_owned()),
        };
        let mut expected = OPTIONS.to_vec();
        expected.extend([
            "-L",
            "127.0.0.1:40000:10.0.0.5:3306",
            "-p",
            "2222",
            "-l",
            "deploy@corp",
            "-i",
            "~/.ssh/id_ed25519",
            "--",
            "bastion.example.com",
        ]);
        assert_eq!(ssh_args(&ssh, 40000, "10.0.0.5", 3306).unwrap(), expected);
    }

    #[test]
    fn an_ipv6_target_is_bracketed_in_the_forward() {
        for host in ["fd00::5", "[fd00::5]"] {
            let args = ssh_args(&tunnel("bastion"), 40000, host, 5432).unwrap();
            assert!(
                args.contains(&"127.0.0.1:40000:[fd00::5]:5432".to_owned()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_host_ssh_would_read_as_an_option_is_refused() {
        let error =
            ssh_args(&tunnel("-oProxyCommand=touch /tmp/x"), 40000, "db", 5432).unwrap_err();
        assert!(
            error.message.contains("starts with '-'"),
            "{}",
            error.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn closing_the_watchdogs_stdin_ends_what_it_runs() {
        let mut child = watched("sleep", &["30".to_owned()]).unwrap();
        let started = Instant::now();
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_ssh_is_named_as_missing() {
        let mut child = watched("dbdelve-no-such-ssh", &[]).unwrap();
        // Held, because `wait` would close it and the watchdog would race the
        // shell's own exit with a kill.
        let _stdin = child.stdin.take();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(127));
        assert_eq!(
            failure("bastion", status, "sh: dbdelve-no-such-ssh: not found").message,
            "No ssh executable was found."
        );
    }

    fn live_tunnel(ssh: &SshTunnel) -> Result<Tunnel, DbError> {
        std::env::var("dbdelve_SSH_CONFIG").expect("dbdelve_SSH_CONFIG is required");
        Tunnel::open(ssh, "postgres", 5432)
    }

    /// Sends Postgres's 8-byte SSLRequest and reads its one-byte answer.
    fn postgres_answers(local: SocketAddr) {
        use std::io::Write;

        let mut stream = TcpStream::connect(local).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
            .unwrap();
        let mut answer = [0];
        stream.read_exact(&mut answer).unwrap();
        assert!(matches!(&answer, b"S" | b"N"), "{answer:?}");
    }

    fn ssh_is_running_for(local: SocketAddr) -> bool {
        Command::new("pgrep")
            .args(["-f", &format!("127.0.0.1:{}:postgres:5432", local.port())])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_tunnel_reaches_postgres_through_the_bastion() {
        let tunnel = live_tunnel(&tunnel("dbdelve-bastion")).unwrap();
        postgres_answers(tunnel.local_addr());
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_tunnel_reaches_postgres_through_a_jump_host() {
        let tunnel = live_tunnel(&tunnel("dbdelve-inner")).unwrap();
        postgres_answers(tunnel.local_addr());
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_host_ssh_cannot_reach_fails_fast_with_ssh_s_message() {
        let unreachable = SshTunnel {
            port: Some(1),
            ..tunnel("127.0.0.1")
        };
        let stranger = SshTunnel {
            user: "nobody".to_owned(),
            ..tunnel("dbdelve-bastion")
        };
        for (ssh, said) in [
            (
                tunnel("dbdelve-nowhere.invalid"),
                "Could not resolve hostname",
            ),
            (unreachable, "Connection refused"),
            (stranger, "Permission denied"),
        ] {
            let started = Instant::now();
            let Err(error) = live_tunnel(&ssh) else {
                panic!("{} opened a tunnel", ssh.host);
            };
            assert!(started.elapsed() < Duration::from_secs(10));
            assert!(
                error
                    .message
                    .starts_with(&format!("SSH tunnel through {} failed: ", ssh.host)),
                "{}",
                error.message
            );
            assert!(error.message.contains(said), "{}", error.message);
        }
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_dropping_the_tunnel_ends_ssh_and_closes_the_port() {
        let tunnel = live_tunnel(&tunnel("dbdelve-bastion")).unwrap();
        let local = tunnel.local_addr();
        assert!(ssh_is_running_for(local));

        drop(tunnel);

        assert!(TcpStream::connect(local).is_err());
        assert!(!ssh_is_running_for(local));
    }
}

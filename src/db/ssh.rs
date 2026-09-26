//! A local port forward over the system `ssh` binary.
//!
//! The binary rather than an SSH library, because it brings `~/.ssh/config`
//! (aliases, `ProxyJump`, `IdentityFile`, `User`), ssh-agent, agent-backed
//! keys and hardware keys along with it. An engine dials [`Tunnel::dial`] and
//! keeps the server's own name for TLS.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{DbError, ServerConfig, SshTunnel, plain_error};

/// Long enough for a hardware key waiting on a touch.
const READY_WITHIN: Duration = Duration::from_secs(30);

/// Attempts at a local port, for when something else takes the one we picked
/// between releasing it and ssh binding it.
const ATTEMPTS: usize = 3;

/// Lines of what ssh said that are kept, and how many of the last an error
/// shows. Its error is the last thing it says; a banner comes first.
const KEPT_LINES: usize = 20;
const SHOWN_LINES: usize = 3;

/// The longest line read whole; a longer one arrives in pieces.
const LINE_LIMIT: u64 = 1024;

/// What `-v` prints once every forward is bound, `ExitOnForwardFailure`
/// having exited ssh before this for any that could not be. "Local forwarding
/// listening" alone is not enough: ssh prints it before it binds.
const SESSION_STARTED: &str = "debug1: Entering interactive session.";

/// What ssh prints, per connection, when the SSH host could not reach the
/// forward's target.
const OPEN_FAILED: &str = "open failed: ";

/// How long an engine's failed connect waits for ssh to say why.
const EXPLAINED_WITHIN: Duration = Duration::from_millis(500);

/// A running `ssh -L`. Dropping it ends ssh and returns once it has exited.
pub struct Tunnel {
    local: SocketAddr,
    host: String,
    said: Arc<Mutex<Said>>,
    /// Behind a mutex so a dial can ask whether ssh is still running.
    process: Mutex<Child>,
    /// Created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the OS ends ssh
    /// when this handle closes, however DBDelve exits.
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

/// What the stderr drain has read.
#[derive(Default)]
struct Said {
    /// What a person might need, newest last: no `-v` debug output, and none
    /// of the chatter `chatter` names.
    lines: VecDeque<String>,
    /// ssh has set our forward's listener up.
    listening: bool,
    /// ... and gone on to the session, so it is bound.
    ready: bool,
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
            if host_keys_left_to_default(&args) {
                args.splice(
                    0..0,
                    [
                        "-o".to_owned(),
                        "StrictHostKeyChecking=accept-new".to_owned(),
                    ],
                );
            }

            let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut tunnel =
                spawn(local, &ssh.host, &args).map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => no_ssh(),
                    _ => plain_error(format!("Could not start ssh: {error}")),
                })?;
            let process = tunnel
                .process
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let draining = drain(
                process.stderr.take().expect("stderr is piped"),
                tunnel.said.clone(),
                port,
            );

            match wait_ready(process, &tunnel.said) {
                Ready::Listening => return Ok(tunnel),
                Ready::Exited(status) => {
                    let _ = draining.join();
                    let said = tunnel.shown();
                    if our_port_was_taken(&said, port) && attempt < ATTEMPTS {
                        attempt += 1;
                        continue;
                    }
                    return Err(failure(&ssh.host, status, &said));
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

    /// The address to dial, once ssh is known to still be running: a freed
    /// port is one anything local could have bound since.
    pub fn dial(&self) -> Result<SocketAddr, DbError> {
        let running = matches!(
            self.process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .try_wait(),
            Ok(None)
        );
        if running {
            return Ok(self.local);
        }
        let said = self.shown();
        Err(plain_error(if said.is_empty() {
            format!("SSH tunnel through {} closed.", self.host)
        } else {
            format!("SSH tunnel through {} closed: {said}", self.host)
        }))
    }

    /// `error`, from an engine that dialled through this tunnel, with ssh's
    /// reason appended when the SSH host could not reach the server. Without
    /// it the engine can only say that the connection closed.
    pub fn explain(&self, mut error: DbError) -> DbError {
        let deadline = Instant::now() + EXPLAINED_WITHIN;
        loop {
            let reason = self.said().lines.iter().rev().find_map(|line| {
                line.split_once(OPEN_FAILED)
                    .map(|(_, reason)| reason.to_owned())
            });
            if let Some(reason) = reason {
                error.message = format!(
                    "{}\nSSH tunnel through {}: {OPEN_FAILED}{reason}",
                    error.message, self.host
                );
                return error;
            }
            if Instant::now() >= deadline {
                return error;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn said(&self) -> MutexGuard<'_, Said> {
        self.said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The last few lines ssh said, for an error.
    fn shown(&self) -> String {
        let said = self.said();
        let skip = said.lines.len().saturating_sub(SHOWN_LINES);
        said.lines
            .iter()
            .skip(skip)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Opens `server`'s tunnel when it has one, then `connect` through it; an engine
/// connection keeps the tunnel for as long as any clone of it lives.
pub(super) fn tunnelled<C>(
    server: &ServerConfig,
    default_port: u16,
    connect: impl FnOnce(Option<Arc<Tunnel>>) -> Result<C, DbError>,
) -> Result<C, DbError> {
    let Some(ssh) = &server.ssh else {
        return connect(None);
    };
    let tunnel = Arc::new(Tunnel::open(
        ssh,
        &server.host,
        server.port.unwrap_or(default_port),
    )?);
    connect(Some(tunnel.clone())).map_err(|error| tunnel.explain(error))
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let process = self
            .process
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Closing the watchdog's stdin is the same teardown a crash gets.
        #[cfg(unix)]
        drop(process.stdin.take());
        #[cfg(windows)]
        let _ = process.kill();
        let _ = process.wait();
    }
}

/// ssh's arguments, bar the program name and what `Tunnel::open` adds.
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
    let identity = ssh.identity_file.as_ref().filter(|path| !path.is_empty());
    if let Some(error) = identity.and_then(|path| SshTunnel::identity_file_error(path)) {
        return Err(plain_error(error));
    }
    let target_host = if target_host.contains(':') && !target_host.starts_with('[') {
        format!("[{target_host}]")
    } else {
        target_host.to_owned()
    };

    let mut args: Vec<String> = [
        "-N",
        "-T",
        // What readiness is read from, and it outranks a config `LogLevel`
        // quiet enough to hide "open failed" too.
        "-v",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        // A config's ControlPersist would fork ssh into the background past
        // Drop and the watchdog, and an existing master would take the
        // forward over from a process we do not hold.
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
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
    if let Some(identity) = identity {
        // ssh expands `%` tokens in an identity file, even one given here.
        args.extend(["-i".to_owned(), identity.replace('%', "%%")]);
    }
    args.extend(["--".to_owned(), ssh.host.clone()]);
    Ok(args)
}

/// Whether nothing the user configured decides host-key checking. Only then
/// is trust-on-first-use added, since a command-line `-o` outranks their
/// config. `ssh -G` evaluates the config for these arguments and exits without
/// connecting; if it cannot, nothing is added.
fn host_keys_left_to_default(args: &[String]) -> bool {
    ssh_command()
        .arg("-G")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == "stricthostkeychecking ask")
        })
}

enum Ready {
    Listening,
    Exited(ExitStatus),
    TimedOut,
}

fn wait_ready(process: &mut Child, said: &Mutex<Said>) -> Ready {
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = process.try_wait() {
            return Ready::Exited(status);
        }
        if said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ready
        {
            return Ready::Listening;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ready::TimedOut
}

/// Only our own port is worth another attempt: a forward the user's config
/// adds collides the same way every time, and each attempt may cost a
/// hardware-key touch.
fn our_port_was_taken(said: &str, port: u16) -> bool {
    let ours = format!("cannot listen to port: {port}");
    said.lines().any(|line| line.ends_with(&ours))
}

fn failure(host: &str, status: ExitStatus, said: &str) -> DbError {
    // The watchdog shell's "command not found".
    if cfg!(unix) && status.code() == Some(127) {
        return no_ssh();
    }
    if said.is_empty() {
        plain_error(format!(
            "SSH tunnel through {host} failed: ssh exited ({status})."
        ))
    } else {
        plain_error(format!("SSH tunnel through {host} failed: {said}"))
    }
}

fn no_ssh() -> DbError {
    plain_error("No ssh executable was found.".to_owned())
}

/// Reads stderr to the end, so ssh never blocks on a full pipe, a line at a
/// time into `said`.
fn drain(stderr: ChildStderr, said: Arc<Mutex<Said>>, port: u16) -> JoinHandle<()> {
    thread::spawn(move || {
        let listening = format!("debug1: Local forwarding listening on 127.0.0.1 port {port}.");
        let mut reader = BufReader::new(stderr);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match (&mut reader).take(LINE_LIMIT).read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            let line = printable(&String::from_utf8_lossy(&raw));
            let mut said = said.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if line.starts_with("debug") {
                if line == listening {
                    said.listening = true;
                } else if said.listening && line == SESSION_STARTED {
                    said.ready = true;
                }
            } else if !chatter(&line) {
                said.lines.push_back(line);
                if said.lines.len() > KEPT_LINES {
                    said.lines.pop_front();
                }
            }
        }
    })
}

/// A line without its terminal escapes or control characters, which a
/// `ProxyCommand` may colour its output with.
fn printable(line: &str) -> String {
    let mut printable = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.next() == Some('[') {
                for char in chars.by_ref() {
                    if ('@'..='~').contains(&char) {
                        break;
                    }
                }
            }
        } else if !char.is_control() {
            printable.push(char);
        }
    }
    printable.trim().to_owned()
}

/// What `-v` and a first connect print that says nothing about a failure.
fn chatter(line: &str) -> bool {
    line.is_empty()
        || [
            "Warning: Permanently added",
            "Authenticated to ",
            "Transferred: ",
            "Bytes per second: ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

/// ssh runs under a shell holding the read end of a pipe on its stdin, whose
/// write end only DBDelve holds. When DBDelve exits, however it exits, the
/// pipe closes, `read` returns, and the shell kills ssh. The argv arrives as
/// `"$@"`, never spliced into the script. `exec 3<&0` because POSIX hands an
/// asynchronous list /dev/null as its stdin; the shell exits with ssh's status.
/// ssh alone gets the stderr pipe, as fd 4, so the shell's own job reports
/// ("Terminated") never reach an error.
#[cfg(unix)]
const WATCHDOG: &str = "exec 3<&0 4>&2 2>/dev/null; \
    \"$@\" </dev/null 2>&4 3<&- 4>&- & pid=$!; \
    (read _ <&3; kill $pid) >/dev/null 4>&- & watcher=$!; \
    wait $pid; status=$?; kill $watcher; exit $status";

#[cfg(unix)]
fn ssh_command() -> Command {
    Command::new("ssh")
}

#[cfg(unix)]
fn spawn(local: SocketAddr, host: &str, args: &[String]) -> io::Result<Tunnel> {
    Ok(Tunnel {
        local,
        host: host.to_owned(),
        said: Arc::default(),
        process: Mutex::new(watched("ssh", args)?),
    })
}

#[cfg(unix)]
fn watched(program: &str, args: &[String]) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", WATCHDOG, "sh", program])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // std hands the child the spawning thread's signal mask, and the GCD
    // worker a background connect runs on blocks every signal: ssh would
    // ignore the watchdog's kill and outlive DBDelve. SIGTERM's disposition
    // too, since an ignored one survives exec just the same.
    // SAFETY: sigemptyset, pthread_sigmask and signal are async-signal-safe,
    // which is all a hook between fork and exec may call.
    unsafe {
        command.pre_exec(|| {
            let mut empty = std::mem::MaybeUninit::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            match libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut()) {
                0 => {}
                error => return Err(io::Error::from_raw_os_error(error)),
            }
            if libc::signal(libc::SIGTERM, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Windows' own OpenSSH, when no other ssh is on PATH: it is not always put
/// there.
#[cfg(windows)]
fn ssh_command() -> Command {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};

    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("ssh.exe").is_file()));
    let program = if on_path {
        PathBuf::from("ssh")
    } else {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join(r"System32\OpenSSH\ssh.exe")
    };
    let mut command = Command::new(program);
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
fn spawn(local: SocketAddr, host: &str, args: &[String]) -> io::Result<Tunnel> {
    let mut process = ssh_command()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    match kill_on_close_job(&process) {
        Ok(job) => Ok(Tunnel {
            local,
            host: host.to_owned(),
            said: Arc::default(),
            process: Mutex::new(process),
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

/// The dev bastion an engine's `live_ssh_` tests tunnel through.
#[cfg(test)]
pub(super) fn live_bastion(alias: &str) -> Option<SshTunnel> {
    std::env::var("dbdelve_SSH_CONFIG").expect("dbdelve_SSH_CONFIG is required");
    Some(SshTunnel {
        host: alias.to_owned(),
        ..SshTunnel::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn tunnel(host: &str) -> SshTunnel {
        SshTunnel {
            host: host.to_owned(),
            ..SshTunnel::default()
        }
    }

    const OPTIONS: [&str; 15] = [
        "-N",
        "-T",
        "-v",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
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
            identity_file: Some("~/.ssh/100%.key".to_owned()),
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
            "~/.ssh/100%%.key",
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

    #[test]
    fn a_relative_identity_file_is_refused() {
        let relative = SshTunnel {
            identity_file: Some("keys/id_ed25519".to_owned()),
            ..tunnel("bastion")
        };
        let error = ssh_args(&relative, 40000, "db", 5432).unwrap_err();
        assert_eq!(
            error.message,
            "Identity file must be an absolute path to the key file."
        );
    }

    #[test]
    fn what_ssh_said_is_shown_without_escapes_or_chatter() {
        assert_eq!(
            printable("\x1b[31mnc: connect failed\x1b[0m\r\n"),
            "nc: connect failed"
        );
        for chatter_line in [
            "Warning: Permanently added '[127.0.0.1]:52222' (ED25519) to the list of known hosts.",
            "Authenticated to 127.0.0.1 ([127.0.0.1]:52222) using \"publickey\".",
            "Transferred: sent 3524, received 3852 bytes, in 3.9 seconds",
            "",
        ] {
            assert!(chatter(chatter_line), "{chatter_line}");
        }
        assert!(!chatter(
            "channel 1: open failed: connect failed: Name does not resolve"
        ));
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

        // The shell's own "Terminated" report is not ssh's to show.
        let mut said = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut said)
            .unwrap();
        assert_eq!(said, "");
    }

    #[test]
    fn only_a_collision_on_our_own_port_is_retried() {
        let taken = |port| {
            format!(
                "bind [127.0.0.1]:{port}: Address already in use\n\
                 channel_setup_fwd_listener_tcpip: cannot listen to port: {port}\n\
                 Could not request local forwarding."
            )
        };
        assert!(our_port_was_taken(&taken(47305), 47305));
        assert!(!our_port_was_taken(&taken(47305), 7305));
        assert!(!our_port_was_taken(&taken(5432), 47305));
    }

    /// Every signal blocked on the spawning thread, as on a GCD worker, which
    /// is where the app's background executor runs a connect.
    #[cfg(unix)]
    fn on_a_thread_blocking_every_signal<T: Send + 'static>(
        spawn: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        thread::spawn(|| {
            // SAFETY: `all` is initialised by sigfillset before it is read, and
            // the mask changes only this thread's.
            unsafe {
                let mut all = std::mem::MaybeUninit::uninit();
                libc::sigfillset(all.as_mut_ptr());
                libc::pthread_sigmask(libc::SIG_SETMASK, all.as_ptr(), std::ptr::null_mut());
            }
            spawn()
        })
        .join()
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_watchdog_started_with_every_signal_blocked_still_ends_what_it_runs() {
        let mut child =
            on_a_thread_blocking_every_signal(|| watched("sleep", &["30".to_owned()]).unwrap());
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the watchdog's kill did not end what it ran");
            }
            thread::sleep(Duration::from_millis(20));
        }
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
        postgres_answers(tunnel.dial().unwrap());
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_tunnel_reaches_postgres_through_a_jump_host() {
        let tunnel = live_tunnel(&tunnel("dbdelve-inner")).unwrap();
        postgres_answers(tunnel.dial().unwrap());
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
        let local = tunnel.dial().unwrap();
        assert!(ssh_is_running_for(local));

        drop(tunnel);

        assert!(TcpStream::connect(local).is_err());
        assert!(!ssh_is_running_for(local));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_tunnel_opened_with_every_signal_blocked_still_ends_on_drop() {
        let tunnel =
            on_a_thread_blocking_every_signal(|| live_tunnel(&tunnel("dbdelve-bastion")).unwrap());
        let local = tunnel.dial().unwrap();

        let (dropped, done) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(tunnel);
            let _ = dropped.send(());
        });
        assert!(
            done.recv_timeout(Duration::from_secs(5)).is_ok(),
            "Drop is still waiting on ssh"
        );
        assert!(!ssh_is_running_for(local));
    }

    #[test]
    #[ignore = "requires the dev bastions configured through dbdelve_SSH_CONFIG"]
    fn live_ssh_a_tunnel_whose_ssh_has_gone_refuses_to_be_dialled() {
        let tunnel = live_tunnel(&tunnel("dbdelve-bastion")).unwrap();
        let port = tunnel.dial().unwrap().port();
        assert!(
            Command::new("pkill")
                .args(["-f", &format!("^ssh .*127.0.0.1:{port}:")])
                .status()
                .unwrap()
                .success()
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match tunnel.dial() {
                Err(error) => break error,
                Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(_) => panic!("a tunnel whose ssh was killed still dials"),
            }
        };
        assert!(
            error
                .message
                .starts_with("SSH tunnel through dbdelve-bastion closed"),
            "{}",
            error.message
        );
    }

    #[test]
    #[ignore = "requires an ssh executable"]
    fn live_ssh_trust_on_first_use_is_added_only_where_nothing_configured_it() {
        let config = std::env::temp_dir().join(format!("dbdelve-ssh-{}.conf", std::process::id()));
        std::fs::write(
            &config,
            "Host strict\n    StrictHostKeyChecking yes\nHost tofu\n    StrictHostKeyChecking accept-new\n",
        )
        .unwrap();
        let defaults = |host: &str| {
            host_keys_left_to_default(&[
                "-F".to_owned(),
                config.display().to_string(),
                "--".to_owned(),
                host.to_owned(),
            ])
        };
        let answers = [
            defaults("unconfigured"),
            defaults("strict"),
            defaults("tofu"),
        ];
        let _ = std::fs::remove_file(&config);
        assert_eq!(answers, [true, false, false]);
    }
}

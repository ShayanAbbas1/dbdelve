//! A local port forward over the system `ssh` binary, or a private stream to a
//! Unix socket on the SSH host over the same binary's stdio.
//!
//! The binary rather than an SSH library, because it brings `~/.ssh/config`
//! (aliases, `ProxyJump`, `IdentityFile`, `User`), ssh-agent, agent-backed
//! keys and hardware keys along with it. An engine dials [`Tunnel::dial`] and
//! keeps the server's own name for TLS.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
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
const LINE_LIMIT: usize = 1024;

/// What `-v` prints once every forward is bound, `ExitOnForwardFailure`
/// having exited ssh before this for any that could not be. "Local forwarding
/// listening" alone is not enough: ssh prints it before it binds.
const SESSION_STARTED: &str = "debug1: Entering interactive session.";

/// What ssh prints, per connection, when the SSH host could not reach the
/// forward's target.
const OPEN_FAILED: &str = "open failed: ";

/// How long an engine's failed connect waits for ssh to say why.
const EXPLAINED_WITHIN: Duration = Duration::from_millis(500);

/// What `-v` prints once ssh has logged in and sent the remote command. Not
/// `SESSION_STARTED`: a `ProxyJump` hop prints that before the last login.
const COMMAND_SENT: &str = "debug1: Sending command: ";

/// How long a dropped bridge's ssh has to deliver what the driver last wrote
/// and exit before it is killed.
const CLOSED_WITHIN: Duration = Duration::from_secs(5);

const RUN_AS_NEEDS_A_SOCKET: &str =
    "Run as needs Host to be the socket directory on the SSH host, such as /var/run/postgresql.";

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
    /// Must survive log truncation: ssh continues even when it cannot save a key.
    host_key_error: Option<String>,
    /// ssh has set our forward's listener up.
    listening: bool,
    /// ... and gone on to the session, so it is bound.
    ready: bool,
    /// A line read so far whose newline has not arrived.
    unfinished: String,
}

impl Tunnel {
    /// Forward a local port to `target_host:target_port` as seen from the SSH
    /// host.
    pub fn open(ssh: &SshTunnel, target_host: &str, target_port: u16) -> Result<Self, DbError> {
        let started = Instant::now();
        tracing::debug!(ssh_host = ssh.host, "opening ssh tunnel");
        let opened = Self::opened(ssh, target_host, target_port);
        let elapsed = started.elapsed();
        match opened {
            Ok(tunnel) => {
                tracing::info!(?elapsed, "ssh tunnel up");
                Ok(tunnel)
            }
            Err((kind, error)) => {
                tracing::warn!(kind, ?elapsed, "ssh tunnel failed");
                tracing::debug!("ssh tunnel failed: {}", error.message);
                Err(error)
            }
        }
    }

    /// A failure comes with its kind, for the log, which may not say what ssh
    /// said or where it was going.
    fn opened(
        ssh: &SshTunnel,
        target_host: &str,
        target_port: u16,
    ) -> Result<Self, (&'static str, DbError)> {
        let mut attempt = 1;
        loop {
            let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .and_then(|listener| listener.local_addr())
                .map_err(|error| {
                    (
                        "local port",
                        plain_error(format!(
                            "Could not reserve a local port for the SSH tunnel: {error}"
                        )),
                    )
                })?
                .port();
            let args = configured(
                ssh_args(ssh, port, target_host, target_port)
                    .map_err(|error| ("arguments", error))?,
            );

            let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut tunnel =
                spawn(local, &ssh.host, &args).map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => ("no ssh", no_ssh()),
                    _ => (
                        "spawn",
                        plain_error(format!("Could not start ssh: {error}")),
                    ),
                })?;
            let process = tunnel
                .process
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let draining = drain(
                process.stderr.take().expect("stderr is piped"),
                tunnel.said.clone(),
                Some(port),
            );

            match wait_ready(process, &tunnel.said) {
                Ready::Listening => return Ok(tunnel),
                Ready::HostKeyNotSaved(message) => {
                    return Err((
                        "host key not saved",
                        plain_error(format!("SSH tunnel through {} failed: {message}", ssh.host)),
                    ));
                }
                Ready::Exited(status) => {
                    drained(draining, &tunnel.said);
                    let said = tunnel.shown();
                    if our_port_was_taken(&said, port) && attempt < ATTEMPTS {
                        attempt += 1;
                        continue;
                    }
                    return Err(("ssh exited", failure(&ssh.host, status, &said)));
                }
                Ready::TimedOut => {
                    return Err((
                        "timed out",
                        plain_error(format!(
                            "SSH tunnel through {} did not come up within {} seconds.",
                            ssh.host,
                            READY_WITHIN.as_secs()
                        )),
                    ));
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
        tracing::warn!("ssh tunnel found closed");
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

    fn shown(&self) -> String {
        self.said().shown()
    }
}

impl Said {
    /// The last few lines ssh said, for an error.
    fn shown(&self) -> String {
        let skip = self.lines.len().saturating_sub(SHOWN_LINES);
        self.lines
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
        tracing::info!("ssh tunnel down");
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

/// A Unix socket on the SSH host, reached through a fresh ssh per connection
/// whose stdio is the stream, never through a local listener: when the server
/// trusts whoever opens the socket (peer authentication), a port anyone on
/// this machine could dial would hand them that login.
///
/// ponytail: one ssh login per connection (the session, the side session,
/// sizes, every cancel), since `ControlMaster=no`. Upgrade: a DBDelve-owned
/// master (`-M -S <private path> -o ControlPersist=no`, watched like a
/// tunnel) the bridges multiplex over; Windows' OpenSSH cannot multiplex.
pub struct SocketBridge {
    host: String,
    args: Vec<String>,
}

/// One bridged connection's ssh. Dropping it lets ssh deliver what the driver
/// last wrote, then ends it.
pub struct Bridged {
    host: String,
    said: Arc<Mutex<Said>>,
    draining: JoinHandle<()>,
    /// Taken by `Drop`, which hands it to a thread of its own.
    process: Option<Process>,
}

/// Ended and waited for on drop.
struct Process {
    child: Child,
    /// The write end of the watchdog's pipe, which only DBDelve holds.
    #[cfg(unix)]
    watchdog: Option<io::PipeWriter>,
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl SocketBridge {
    /// Checks everything and asks `ssh -G` once; runs nothing until
    /// [`Self::connect`].
    pub fn new(ssh: &SshTunnel, socket: &str) -> Result<Self, DbError> {
        Ok(Self {
            host: ssh.host.clone(),
            args: configured(bridge_args(ssh, socket)?),
        })
    }

    /// A new ssh session to the socket, once it has logged in, and the
    /// driver's end of it.
    pub fn connect(&self) -> Result<(TcpStream, Bridged), DbError> {
        let started = Instant::now();
        tracing::debug!(ssh_host = self.host, "opening ssh bridge");
        let opened = self.connected();
        let elapsed = started.elapsed();
        match opened {
            Ok(bridge) => {
                tracing::info!(?elapsed, "ssh bridge up");
                Ok(bridge)
            }
            Err((kind, error)) => {
                tracing::warn!(kind, ?elapsed, "ssh bridge failed");
                tracing::debug!("ssh bridge failed: {}", error.message);
                Err(error)
            }
        }
    }

    /// A failure comes with its kind, for the log, which may not say what ssh
    /// said, where it was going or who it ran as.
    fn connected(&self) -> Result<(TcpStream, Bridged), (&'static str, DbError)> {
        let mut process = spawn_bridge(&self.args).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => ("no ssh", no_ssh()),
            _ => (
                "spawn",
                plain_error(format!("Could not start ssh: {error}")),
            ),
        })?;
        let said = Arc::<Mutex<Said>>::default();
        let draining = drain(
            process.child.stderr.take().expect("stderr is piped"),
            said.clone(),
            None,
        );
        match wait_ready(&mut process.child, &said) {
            Ready::Listening => {}
            Ready::HostKeyNotSaved(message) => {
                return Err((
                    "host key",
                    plain_error(format!(
                        "SSH bridge through {} failed: {message}",
                        self.host
                    )),
                ));
            }
            Ready::Exited(status) => {
                drained(draining, &said);
                let said = said.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // The watchdog shell's "command not found". Once the command
                // is sent, a 127 is the SSH host's: no socat, or no sudo.
                if cfg!(unix) && status.code() == Some(127) && !said.ready {
                    return Err(("no ssh", no_ssh()));
                }
                return Err((
                    "ssh exited",
                    plain_error(if said.lines.is_empty() {
                        format!(
                            "SSH bridge through {} failed: ssh exited ({status}).",
                            self.host
                        )
                    } else {
                        format!("SSH bridge through {} failed: {}", self.host, said.shown())
                    }),
                ));
            }
            Ready::TimedOut => {
                return Err((
                    "timed out",
                    plain_error(format!(
                        "SSH bridge through {} did not come up within {} seconds.",
                        self.host,
                        READY_WITHIN.as_secs()
                    )),
                ));
            }
        }

        let unopened = |error: io::Error| {
            (
                "socket pair",
                plain_error(format!(
                    "Could not open a stream for the SSH bridge: {error}"
                )),
            )
        };
        let driver = piped_to(&mut process.child).map_err(unopened)?;
        Ok((
            driver,
            Bridged {
                host: self.host.clone(),
                said,
                draining,
                process: Some(process),
            },
        ))
    }
}

impl Bridged {
    /// `error`, from an engine whose connect through this bridge failed, with
    /// what ssh last said once it has exited: sudo's or socat's reason arrives
    /// there, and the engine can only say that the connection closed.
    pub fn explain(&self, mut error: DbError) -> DbError {
        let deadline = Instant::now() + EXPLAINED_WITHIN;
        while !self.draining.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let said = self
            .said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .shown();
        if self.draining.is_finished() && !said.is_empty() {
            error.message = format!(
                "{}\nSSH bridge through {}: {said}",
                error.message, self.host
            );
        }
        error
    }
}

impl Drop for Bridged {
    fn drop(&mut self) {
        // Not killed at once: a cancel request or a Terminate may still be on
        // its way through ssh, which exits by itself once the driver's end has
        // closed and socat has delivered it.
        let Some(mut process) = self.process.take() else {
            return;
        };
        thread::spawn(move || {
            let deadline = Instant::now() + CLOSED_WITHIN;
            while matches!(process.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        });
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Closing the watchdog's pipe is the same teardown a crash gets.
        #[cfg(unix)]
        drop(self.watchdog.take());
        #[cfg(windows)]
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ssh.run_as`, when it says something.
fn run_as(ssh: &SshTunnel) -> Option<&str> {
    ssh.run_as.as_deref().filter(|user| !user.is_empty())
}

/// Why `ssh` cannot bridge to `socket`, if it cannot. Both reach the remote
/// command, which the SSH host's login shell parses and whose `:`, `,` and `!`
/// socat parses again, so they are checked rather than quoted.
///
/// ponytail: a path outside these characters (a space, a comma) is refused.
/// Quoting for both the remote shell and socat's address grammar is the
/// upgrade, if anyone needs one.
pub(super) fn socket_bridge_error(ssh: &SshTunnel, socket: &str) -> Option<String> {
    if let Some(user) = run_as(ssh)
        && (user.starts_with('-')
            || !user
                .chars()
                .all(|char| char.is_ascii_alphanumeric() || "._-".contains(char)))
    {
        return Some(format!(
            "Run as {user} must be a user name of letters, digits, '.', '_' and '-', \
             not starting with '-'."
        ));
    }
    if !socket.starts_with('/')
        || !socket
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || "._-/".contains(char))
    {
        return Some(format!(
            "Unix socket {socket} must be an absolute path of letters, digits, '.', '_', '-' and '/'."
        ));
    }
    None
}

/// The message a forward refuses Run as with, for a caller to refuse it first.
pub(super) fn run_as_needs_a_socket(ssh: &SshTunnel) -> Option<String> {
    run_as(ssh).map(|_| RUN_AS_NEEDS_A_SOCKET.to_owned())
}

/// ssh's arguments for a bridge, bar the program name and what `configured`
/// adds.
fn bridge_args(ssh: &SshTunnel, socket: &str) -> Result<Vec<String>, DbError> {
    if let Some(error) = socket_bridge_error(ssh, socket) {
        return Err(plain_error(error));
    }
    // No escape character: stdin is the driver's bytes.
    let mut args = session_args(ssh, ["-e".to_owned(), "none".to_owned()])?;
    let open = format!("socat STDIO UNIX-CONNECT:{socket}");
    args.push(match run_as(ssh) {
        // -n: under BatchMode, a sudo that wants a password fails saying so
        // rather than waiting for one nobody can type.
        Some(user) => format!("sudo -n -u {user} {open}"),
        None => open,
    });
    Ok(args)
}

/// Two connected loopback sockets, for a stream between the driver and ssh's
/// stdio: Windows cannot hand a child a socket as stdio, so this is the same
/// everywhere. The listener lives until its own connect is accepted, and
/// drops whatever else connects first.
fn socket_pair() -> io::Result<(TcpStream, TcpStream)> {
    paired_through(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?)
}

/// The driver's end of a socket pair whose other end is copied to `child`'s
/// stdin and from its stdout.
fn piped_to(child: &mut Child) -> io::Result<TcpStream> {
    let (driver, ours) = socket_pair()?;
    let mut to_driver = ours.try_clone()?;
    let mut from_driver = ours;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    thread::spawn(move || {
        let _ = pump(&mut stdout, &mut to_driver);
        let _ = to_driver.shutdown(Shutdown::Write);
    });
    thread::spawn(move || {
        let _ = pump(&mut from_driver, &mut stdin);
        // ssh's EOF, which ends socat on the SSH host and then ssh.
        drop(stdin);
    });
    Ok(driver)
}

/// `io::copy` without its Linux `splice`: a blocking splice from a socket into
/// a pipe holds the pipe's lock while it waits for more, so ssh could not read
/// what the last one delivered, the driver's startup message among it.
fn pump(from: &mut impl Read, to: &mut impl io::Write) -> io::Result<()> {
    let mut chunk = [0; 16 * 1024];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => to.write_all(&chunk[..read])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn paired_through(listener: TcpListener) -> io::Result<(TcpStream, TcpStream)> {
    let driver = TcpStream::connect(listener.local_addr()?)?;
    let expected = driver.local_addr()?;
    loop {
        let (ours, peer) = listener.accept()?;
        if peer == expected {
            driver.set_nodelay(true)?;
            ours.set_nodelay(true)?;
            return Ok((driver, ours));
        }
    }
}

#[cfg(unix)]
fn spawn_bridge(args: &[String]) -> io::Result<Process> {
    watched_bridge("ssh", args)
}

#[cfg(windows)]
fn spawn_bridge(args: &[String]) -> io::Result<Process> {
    let mut child = crate::db::spawn(
        ssh_command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    match kill_on_close_job(&child) {
        Ok(job) => Ok(Process { child, _job: job }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// ssh's arguments for a forward, bar the program name and what `configured`
/// adds.
fn ssh_args(
    ssh: &SshTunnel,
    local_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<Vec<String>, DbError> {
    // A forward would connect as the SSH user, never as the one asked for.
    if run_as(ssh).is_some() {
        return Err(plain_error(RUN_AS_NEEDS_A_SOCKET.to_owned()));
    }
    let target_host = if target_host.contains(':') && !target_host.starts_with('[') {
        format!("[{target_host}]")
    } else {
        target_host.to_owned()
    };
    session_args(
        ssh,
        [
            "-N".to_owned(),
            "-o".to_owned(),
            "ExitOnForwardFailure=yes".to_owned(),
            "-L".to_owned(),
            format!("127.0.0.1:{local_port}:{target_host}:{target_port}"),
        ],
    )
}

/// What every session of ours passes ssh, around `session`'s own options, up
/// to and including the host.
fn session_args(
    ssh: &SshTunnel,
    session: impl IntoIterator<Item = String>,
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

    let mut args: Vec<String> = [
        "-T",
        // What readiness is read from, and it outranks a config `LogLevel`
        // quiet enough to hide "open failed" too.
        "-v",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        // A config's ControlPersist would fork ssh into the background past
        // Drop and the watchdog, and an existing master would take the
        // session over from a process we do not hold.
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
    ]
    .map(String::from)
    .into();
    args.extend(session);
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
    match std::env::var("DBDELVE_SSH_KNOWN_HOSTS") {
        Ok(files) if !files.trim().is_empty() => {
            args.extend(["-o".to_owned(), format!("UserKnownHostsFile={files}")]);
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => {}
        Err(error) => {
            return Err(plain_error(format!(
                "Invalid DBDELVE_SSH_KNOWN_HOSTS: {error}"
            )));
        }
    }
    args.extend(["--".to_owned(), ssh.host.clone()]);
    Ok(args)
}

fn configured(mut args: Vec<String>) -> Vec<String> {
    // Tests never read the real ~/.ssh.
    if cfg!(test)
        && let Ok(config) = std::env::var("dbdelve_SSH_CONFIG")
    {
        args.splice(0..0, ["-F".to_owned(), config]);
    }
    if host_key_checking_is_ask(&args) {
        args.splice(
            0..0,
            [
                "-o".to_owned(),
                "StrictHostKeyChecking=accept-new".to_owned(),
            ],
        );
    }
    args
}

/// `ssh -G` cannot distinguish a default `ask` from an explicit one, so both
/// receive `accept-new`. If configuration evaluation fails, add no override.
fn host_key_checking_is_ask(args: &[String]) -> bool {
    crate::db::spawn(
        ssh_command()
            .arg("-G")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .and_then(Child::wait_with_output)
    .is_ok_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == "stricthostkeychecking ask")
    })
}

enum Ready {
    Listening,
    HostKeyNotSaved(String),
    Exited(ExitStatus),
    TimedOut,
}

fn wait_ready(process: &mut Child, said: &Mutex<Said>) -> Ready {
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = process.try_wait() {
            return Ready::Exited(status);
        }
        let said = said.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(message) = &said.host_key_error {
            return Ready::HostKeyNotSaved(message.clone());
        }
        if said.ready {
            return Ready::Listening;
        }
        drop(said);
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

/// Waits, briefly, for the drain to read the stderr of an ssh that has
/// exited: a `ProxyCommand` that left a process holding stderr would
/// otherwise keep it open, and the connect waiting, indefinitely. What it
/// has read by then, a line it never ended included, is what the error shows.
fn drained(draining: JoinHandle<()>, said: &Mutex<Said>) {
    let deadline = Instant::now() + EXPLAINED_WITHIN;
    while !draining.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if draining.is_finished() {
        let _ = draining.join();
        return;
    }
    let mut said = said.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let unfinished = std::mem::take(&mut said.unfinished);
    if !unfinished.starts_with("debug") && !chatter(&unfinished) {
        said.lines.push_back(unfinished);
    }
}

fn no_ssh() -> DbError {
    plain_error("No ssh executable was found.".to_owned())
}

/// Reads stderr to the end, so ssh never blocks on a full pipe, a line at a
/// time into `said`. `port` is a forward's; a session without one is ready
/// once ssh has sent its command.
fn drain(mut stderr: ChildStderr, said: Arc<Mutex<Said>>, port: Option<u16>) -> JoinHandle<()> {
    thread::spawn(move || {
        let listening = port
            .map(|port| format!("debug1: Local forwarding listening on 127.0.0.1 port {port}."));
        let mut chunk = [0; LINE_LIMIT];
        let mut raw = Vec::new();
        loop {
            let read = match stderr.read(&mut chunk) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            raw.extend_from_slice(&chunk[..read]);
            let mut said = said.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while let Some(end) = line_end(&raw, read == 0) {
                let line = printable(&String::from_utf8_lossy(&raw[..end]));
                raw.drain(..end);
                if line.starts_with("Failed to add the host to the list of known hosts (") {
                    said.host_key_error.get_or_insert_with(|| line.clone());
                }
                if line.starts_with("debug") {
                    if listening.as_ref() == Some(&line) {
                        said.listening = true;
                    } else if said.listening && line == SESSION_STARTED
                        || listening.is_none() && line.starts_with(COMMAND_SENT)
                    {
                        said.ready = true;
                    }
                } else if !chatter(&line) {
                    said.lines.push_back(line);
                    if said.lines.len() > KEPT_LINES {
                        said.lines.pop_front();
                    }
                }
            }
            said.unfinished = printable(&String::from_utf8_lossy(&raw));
            if read == 0 {
                break;
            }
        }
    })
}

/// Where the first line in `raw` ends: past its newline, at `LINE_LIMIT`, or,
/// once stderr has closed, wherever it stops.
fn line_end(raw: &[u8], closed: bool) -> Option<usize> {
    let limit = raw.len().min(LINE_LIMIT);
    match raw[..limit].iter().position(|&byte| byte == b'\n') {
        Some(at) => Some(at + 1),
        None if limit == LINE_LIMIT || closed && limit > 0 => Some(limit),
        None => None,
    }
}

/// A line without its terminal escapes or control characters, which a
/// `ProxyCommand` may colour its output with.
pub(super) fn printable(line: &str) -> String {
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

/// `WATCHDOG` for a session whose stdin and stdout are its data: the pipe
/// arrives as fd 3 instead, put there by `watched_bridge`, and ssh is handed
/// the shell's stdin (kept as fd 5, past the asynchronous list's /dev/null)
/// and stdout. A bridge's ssh reads no stdin until it has logged in, which
/// can wait on a hardware key's touch, so EOF there cannot stand in for it.
#[cfg(unix)]
const BRIDGE_WATCHDOG: &str = "exec 4>&2 2>/dev/null 5<&0; \
    \"$@\" <&5 2>&4 3<&- 4>&- 5<&- & pid=$!; \
    (read _ <&3; kill $pid) </dev/null >/dev/null 4>&- 5<&- & watcher=$!; \
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
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", WATCHDOG, "sh", program])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unmask_signals(&mut command);
    crate::db::spawn(&mut command)
}

#[cfg(unix)]
fn watched_bridge(program: &str, args: &[String]) -> io::Result<Process> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let (alive, watchdog) = io::pipe()?;
    // Above 0 to 2, which std fills with the child's stdio before `pre_exec`
    // runs: with a std descriptor closed, the pipe could have been given one.
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor nothing else owns.
    let alive = match unsafe { libc::fcntl(alive.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) } {
        -1 => return Err(io::Error::last_os_error()),
        fd => unsafe { OwnedFd::from_raw_fd(fd) },
    };
    let alive_fd = alive.as_raw_fd();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", BRIDGE_WATCHDOG, "sh", program])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unmask_signals(&mut command);
    // SAFETY: dup2 is async-signal-safe, which is all a hook between fork
    // and exec may call. The copy on fd 3 is the one end not close-on-exec.
    unsafe {
        command.pre_exec(move || match libc::dup2(alive_fd, 3) {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let child = crate::db::spawn(&mut command)?;
    Ok(Process {
        child,
        watchdog: Some(watchdog),
    })
}

/// std hands a child the spawning thread's signal mask, and the GCD worker a
/// background connect runs on blocks every signal: ssh would ignore the
/// watchdog's kill and outlive DBDelve, and a plugin would ignore every signal
/// it did not unblock itself. SIGTERM's disposition too, since an ignored one
/// survives exec just the same.
#[cfg(unix)]
pub(super) fn unmask_signals(command: &mut Command) {
    use std::os::unix::process::CommandExt;

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
    let mut process = crate::db::spawn(
        ssh_command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )?;
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
pub(super) fn kill_on_close_job(process: &Child) -> io::Result<std::os::windows::io::OwnedHandle> {
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

    const OPTIONS: [&str; 12] = [
        "-T",
        "-v",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
    ];

    const FORWARD: [&str; 4] = ["-N", "-o", "ExitOnForwardFailure=yes", "-L"];

    #[test]
    fn what_the_config_leaves_blank_is_left_to_ssh() {
        let args = ssh_args(&tunnel("bastion"), 40000, "db.internal", 5432).unwrap();
        let mut expected = OPTIONS.to_vec();
        expected.extend(FORWARD);
        expected.extend(["127.0.0.1:40000:db.internal:5432", "--", "bastion"]);
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
            run_as: None,
        };
        let mut expected = OPTIONS.to_vec();
        expected.extend(FORWARD);
        expected.extend([
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
    fn a_bridge_runs_socat_on_the_ssh_host_as_the_user_asked_for() {
        let ssh = SshTunnel {
            host: "bastion.example.com".to_owned(),
            port: Some(2222),
            user: "deploy@corp".to_owned(),
            identity_file: Some("~/.ssh/id_ed25519".to_owned()),
            run_as: Some("postgres".to_owned()),
        };
        let mut expected = OPTIONS.to_vec();
        expected.extend([
            "-e",
            "none",
            "-p",
            "2222",
            "-l",
            "deploy@corp",
            "-i",
            "~/.ssh/id_ed25519",
            "--",
            "bastion.example.com",
            "sudo -n -u postgres socat STDIO UNIX-CONNECT:/var/run/postgresql/.s.PGSQL.5432",
        ]);
        assert_eq!(
            bridge_args(&ssh, "/var/run/postgresql/.s.PGSQL.5432").unwrap(),
            expected
        );

        for run_as in [None, Some(String::new())] {
            let as_the_ssh_user = SshTunnel {
                run_as,
                ..tunnel("bastion")
            };
            let args = bridge_args(&as_the_ssh_user, "/tmp/.s.PGSQL.5433").unwrap();
            assert_eq!(
                args.last().unwrap(),
                "socat STDIO UNIX-CONNECT:/tmp/.s.PGSQL.5433"
            );
        }
    }

    #[test]
    fn what_the_remote_shell_or_socat_would_read_is_refused() {
        for user in ["-u", "post gres", "postgres;id", "$(id)", "a,b", "a'b"] {
            let ssh = SshTunnel {
                run_as: Some(user.to_owned()),
                ..tunnel("bastion")
            };
            let error = bridge_args(&ssh, "/tmp/.s.PGSQL.5432").unwrap_err();
            assert!(error.message.starts_with("Run as "), "{}", error.message);
        }
        for socket in [
            "tmp/.s.PGSQL.5432",
            "/var/run/my db/.s.PGSQL.5432",
            "/tmp/a,fork/.s.PGSQL.5432",
            "/tmp/a:b/.s.PGSQL.5432",
            "/tmp/a!!b/.s.PGSQL.5432",
            "/tmp/$(id)/.s.PGSQL.5432",
        ] {
            let error = bridge_args(&tunnel("bastion"), socket).unwrap_err();
            assert!(
                error.message.starts_with("Unix socket "),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn a_forward_refuses_run_as_rather_than_connecting_as_the_ssh_user() {
        let ssh = SshTunnel {
            run_as: Some("postgres".to_owned()),
            ..tunnel("bastion")
        };
        assert_eq!(
            ssh_args(&ssh, 40000, "db", 5432).unwrap_err().message,
            RUN_AS_NEEDS_A_SOCKET
        );
    }

    #[test]
    fn a_socket_pair_carries_bytes_both_ways() {
        use std::io::Write;

        let (mut driver, mut ours) = socket_pair().unwrap();
        driver.write_all(b"ping").unwrap();
        ours.write_all(b"pong").unwrap();
        let mut read = [0; 4];
        ours.read_exact(&mut read).unwrap();
        assert_eq!(&read, b"ping");
        driver.read_exact(&mut read).unwrap();
        assert_eq!(&read, b"pong");
    }

    #[test]
    fn a_connection_that_reaches_the_pairs_listener_first_is_not_its_other_end() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut stranger = TcpStream::connect(listener.local_addr().unwrap()).unwrap();

        let (driver, ours) = paired_through(listener).unwrap();

        assert_eq!(ours.peer_addr().unwrap(), driver.local_addr().unwrap());
        stranger
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let closed = stranger.read(&mut [0; 1]);
        assert!(
            matches!(closed, Ok(0))
                || closed
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::ConnectionReset),
            "the stranger's connection was kept: {closed:?}"
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
    fn readiness_after(messages: &str) -> Ready {
        let port = 40000;
        let output = format!(
            "{messages}\ndebug1: Local forwarding listening on 127.0.0.1 port {port}.\n{SESSION_STARTED}"
        );
        let mut tunnel = Tunnel {
            local: (Ipv4Addr::LOCALHOST, port).into(),
            host: "bastion".to_owned(),
            said: Arc::default(),
            process: Mutex::new(
                watched(
                    "sh",
                    &[
                        "-c".to_owned(),
                        "printf '%s\\n' \"$1\" >&2; exec sleep 30".to_owned(),
                        "sh".to_owned(),
                        output,
                    ],
                )
                .unwrap(),
            ),
        };
        let process = tunnel.process.get_mut().unwrap();
        let draining = drain(
            process.stderr.take().unwrap(),
            tunnel.said.clone(),
            Some(port),
        );
        // Both the failure and readiness must have arrived before we decide.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !tunnel.said.lock().unwrap().ready {
            assert!(Instant::now() < deadline, "stderr never reached readiness");
            thread::sleep(Duration::from_millis(10));
        }
        let ready = wait_ready(process, &tunnel.said);
        drop(tunnel);
        draining.join().unwrap();
        ready
    }

    #[cfg(unix)]
    #[test]
    fn a_host_key_write_failure_outlives_the_log_and_prevents_readiness() {
        let message = "Failed to add the host to the list of known hosts (/app data/known_hosts).";
        let messages = format!("{message}\n{}", "server banner\n".repeat(KEPT_LINES + 1));
        let Ready::HostKeyNotSaved(error) = readiness_after(&messages) else {
            panic!("a host key that was not saved did not prevent readiness");
        };
        assert_eq!(error, message);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_hosts_and_successful_registration_still_reach_readiness() {
        for message in [
            "debug1: Host 'bastion' is known and matches the ED25519 host key.",
            "Warning: Permanently added 'bastion' (ED25519) to the list of known hosts.",
            "Failed to add the ED25519 host key for IP address '127.0.0.1' to the list of known hosts (/app data/known_hosts).",
            "server banner",
        ] {
            assert!(
                matches!(readiness_after(message), Ready::Listening),
                "{message}"
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn an_exited_ssh_whose_stderr_another_process_holds_still_says_what_it_said() {
        let mut child = Command::new("sh")
            .args(["-c", "echo 'no route to host' >&2; sleep 3 & exit 255"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let said = Arc::<Mutex<Said>>::default();
        let draining = drain(child.stderr.take().unwrap(), said.clone(), None);
        child.wait().unwrap();

        let started = Instant::now();
        drained(draining, &said);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(said.lock().unwrap().shown(), "no route to host");
    }

    #[cfg(unix)]
    #[test]
    fn an_exited_ssh_whose_stderr_another_process_holds_still_says_what_it_left_unended() {
        let mut child = Command::new("sh")
            .args(["-c", "printf 'no route to host' >&2; sleep 3 & exit 255"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let said = Arc::<Mutex<Said>>::default();
        let draining = drain(child.stderr.take().unwrap(), said.clone(), None);
        child.wait().unwrap();

        let started = Instant::now();
        drained(draining, &said);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(said.lock().unwrap().shown(), "no route to host");
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

    /// A bridge watchdog's `sleep` and its pid: a kill aimed at the watchdog
    /// shell instead would end the shell and leave the `sleep` running.
    #[cfg(unix)]
    fn watched_sleep() -> (Process, libc::pid_t) {
        use std::io::BufRead;

        let script = "echo $$; exec sleep 30".to_owned();
        let mut process = watched_bridge("sh", &["-c".to_owned(), script]).unwrap();
        let mut pid = String::new();
        io::BufReader::new(process.child.stdout.take().unwrap())
            .read_line(&mut pid)
            .unwrap();
        (process, pid.trim().parse().unwrap())
    }

    /// Ended and reaped: a zombie still answers.
    #[cfg(unix)]
    fn gone(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        let failed = unsafe { libc::kill(pid, 0) } == -1;
        failed && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[test]
    fn a_bridge_watchdog_ends_what_it_runs_before_it_reads_stdin() {
        let (mut process, sleeping) = on_a_thread_blocking_every_signal(watched_sleep);
        // Held open, as for an ssh still waiting on a login.
        let _stdin = process.child.stdin.take();
        drop(process.watchdog.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone(sleeping) {
            if Instant::now() > deadline {
                // SAFETY: SIGKILL to the pid the sleep reported.
                unsafe { libc::kill(sleeping, libc::SIGKILL) };
                panic!("closing the watchdog's pipe did not end what it ran");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_bridge_watchdog_hands_what_it_runs_its_stdin_and_stdout() {
        use std::io::Write;

        let mut process = watched_bridge("cat", &[]).unwrap();
        let mut stdin = process.child.stdin.take().unwrap();
        stdin.write_all(b"ping").unwrap();
        drop(stdin);
        let mut echoed = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut echoed)
            .unwrap();
        assert_eq!(echoed, "ping");
    }

    #[cfg(unix)]
    #[test]
    fn a_bridged_child_answers_the_driver_while_its_stream_stays_open() {
        use std::io::Write;

        let mut process = watched_bridge("cat", &[]).unwrap();
        let mut driver = piped_to(&mut process.child).unwrap();
        driver.write_all(b"ping").unwrap();
        driver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut echoed = [0; 4];
        driver.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"ping");
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_bridge_whose_ssh_never_exits_is_ended_once_its_grace_is_out() {
        let (mut process, sleeping) = watched_sleep();
        let draining = drain(process.child.stderr.take().unwrap(), Arc::default(), None);

        let dropped = Instant::now();
        drop(Bridged {
            host: "bastion".to_owned(),
            said: Arc::default(),
            draining,
            process: Some(process),
        });

        thread::sleep(Duration::from_secs(1));
        assert!(
            !gone(sleeping),
            "ended before what it was sending had its grace"
        );
        while !gone(sleeping) {
            assert!(
                dropped.elapsed() < CLOSED_WITHIN + Duration::from_secs(3),
                "still running well past its grace"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(dropped.elapsed() >= CLOSED_WITHIN);
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
    fn live_ssh_accept_new_is_added_only_when_ssh_reports_ask() {
        let config = std::env::temp_dir().join(format!("dbdelve-ssh-{}.conf", std::process::id()));
        std::fs::write(
            &config,
            "Host strict\n    StrictHostKeyChecking yes\n\
             Host tofu\n    StrictHostKeyChecking accept-new\n\
             Host ask\n    StrictHostKeyChecking ask\n\
             Host unchecked\n    StrictHostKeyChecking no\n",
        )
        .unwrap();
        let reports_ask = |host: &str| {
            host_key_checking_is_ask(&[
                "-F".to_owned(),
                config.display().to_string(),
                "--".to_owned(),
                host.to_owned(),
            ])
        };
        let answers = [
            reports_ask("unconfigured"),
            reports_ask("ask"),
            reports_ask("strict"),
            reports_ask("tofu"),
            reports_ask("unchecked"),
        ];
        let _ = std::fs::remove_file(&config);
        assert_eq!(answers, [true, true, false, false, false]);
    }
}

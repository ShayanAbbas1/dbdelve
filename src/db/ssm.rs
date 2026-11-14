//! Remote-host forwarding via the user's AWS CLI and Session Manager plugin.
//! Credentials stay with the CLI. Neither its environment nor raw output is logged.
use super::{DbError, plain_error};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::Read,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
mod process;
pub use process::helper;

const READY_WITHIN: Duration = Duration::from_secs(30);
const DOCUMENT: &str = "AWS-StartPortForwardingSessionToRemoteHost";

#[derive(Clone, Debug, Default)]
pub struct ConnectAttempt(Arc<AtomicU8>);
impl PartialEq for ConnectAttempt {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ConnectAttempt {}
impl ConnectAttempt {
    // Cancellation and completion can only win while the attempt is opening.
    const OPENING: u8 = 0;
    const CANCELLED: u8 = 1;
    const FINISHED: u8 = 2;

    pub fn cancel(&self) {
        let _ = self.0.compare_exchange(
            Self::OPENING,
            Self::CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::CANCELLED
    }
    fn is_finished(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::FINISHED
    }
    pub fn finish(&self) -> Result<(), DbError> {
        self.0
            .compare_exchange(
                Self::OPENING,
                Self::FINISHED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| plain_error("Connection cancelled.".into()))
    }
    pub fn guard(&self) -> ConnectGuard {
        ConnectGuard(self.clone())
    }
}
pub struct ConnectGuard(ConnectAttempt);
impl Drop for ConnectGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsmTunnel {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_cli: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_manager_plugin: Option<PathBuf>,
    /// Only the connection attempt owns this. It never reaches profiles.toml.
    #[serde(skip)]
    pub attempt: Option<ConnectAttempt>,
}
impl SsmTunnel {
    pub fn validate(&self) -> Result<(), String> {
        let id = self
            .target
            .strip_prefix("i-")
            .or_else(|| self.target.strip_prefix("mi-"));
        if !id.is_some_and(|id| {
            matches!(id.len(), 8 | 17) && id.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err("SSM target must be a managed instance ID (i-… or mi-…).".into());
        }
        for (label, path) in [
            ("AWS CLI", &self.aws_cli),
            ("Session Manager plugin", &self.session_manager_plugin),
        ] {
            if path.as_ref().is_some_and(|p| !p.is_absolute()) {
                return Err(format!("{label} path must be absolute."));
            }
        }
        if let Some(path) = &self.session_manager_plugin {
            let expected = if cfg!(windows) {
                "session-manager-plugin.exe"
            } else {
                "session-manager-plugin"
            };
            if path.file_name().is_none_or(|name| name != expected) {
                return Err(format!("Plugin path must name {expected}."));
            }
        }
        for value in [&self.profile, &self.region].into_iter().flatten() {
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(
                    "AWS profile and region must be nonempty and contain no control characters."
                        .into(),
                );
            }
        }
        Ok(())
    }
    fn check_cancelled(&self) -> Result<(), DbError> {
        if self
            .attempt
            .as_ref()
            .is_some_and(ConnectAttempt::is_cancelled)
        {
            Err(plain_error("SSM connection cancelled.".into()))
        } else {
            Ok(())
        }
    }
}

fn arguments(config: &SsmTunnel, host: &str, port: u16) -> Result<Vec<String>, DbError> {
    config.validate().map_err(plain_error)?;
    if host.is_empty()
        || host.starts_with('/')
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',')
        || port == 0
    {
        return Err(plain_error("SSM requires one TCP database host and a nonzero port; Unix sockets and host lists are not supported.".into()));
    }
    let params = serde_json::json!({"host": [host], "portNumber": [port.to_string()], "localPortNumber": ["0"]});
    let mut args = vec![
        "ssm".into(),
        "start-session".into(),
        "--target".into(),
        config.target.clone(),
        "--document-name".into(),
        DOCUMENT.into(),
        "--parameters".into(),
        params.to_string(),
    ];
    for (flag, value) in [("--profile", &config.profile), ("--region", &config.region)] {
        if let Some(value) = value {
            args.push(flag.into());
            args.push(value.clone());
        }
    }
    Ok(args)
}

fn executable(explicit: Option<&Path>, name: &str) -> Result<PathBuf, DbError> {
    let found = if let Some(path) = explicit {
        Some(path.to_owned()).filter(|p| p.is_file())
    } else {
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        };
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(&file))
                .find(|p| p.is_file())
        })
    };
    found.ok_or_else(|| plain_error(format!("Could not find {name}. Install it or set its absolute path in the SSM connection settings. Desktop apps may have a different PATH.")))
}
#[cfg(any(windows, test))]
fn windows_plugin_lookup(aws: &Path, plugin: &Path) -> Result<(), DbError> {
    // CreateProcess searches the calling application's directory before cwd
    // and PATH. We set cwd to the selected plugin directory below; refuse a
    // different copy next to aws.exe rather than silently running it.
    let shadow = aws
        .parent()
        .ok_or_else(|| plain_error("AWS CLI path has no parent directory.".into()))?
        .join("session-manager-plugin.exe");
    if shadow.exists() {
        let actual = std::fs::canonicalize(shadow).map_err(|e| plain_error(e.to_string()))?;
        let selected = std::fs::canonicalize(plugin).map_err(|e| plain_error(e.to_string()))?;
        if actual != selected {
            return Err(plain_error("Another Session Manager plugin next to the AWS CLI would override the selected plugin. Select that copy or use a different CLI installation.".into()));
        }
    }
    Ok(())
}

fn command(config: &SsmTunnel, args: &[String]) -> Result<Command, DbError> {
    let aws = executable(config.aws_cli.as_deref(), "aws")?;
    let plugin = executable(
        config.session_manager_plugin.as_deref(),
        "session-manager-plugin",
    )?;
    let mut paths = vec![
        plugin
            .parent()
            .expect("resolved executable has a parent")
            .to_owned(),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    #[cfg(windows)]
    windows_plugin_lookup(&aws, &plugin)?;
    let mut command = Command::new(aws);
    #[cfg(windows)]
    command.current_dir(plugin.parent().expect("resolved plugin parent"));
    command
        .args(args)
        .env(
            "PATH",
            std::env::join_paths(paths).map_err(|e| plain_error(e.to_string()))?,
        )
        .env("AWS_PAGER", "")
        .env("AWS_CLI_AUTO_PROMPT", "off");
    Ok(command)
}

#[derive(Default)]
struct Output {
    port: Option<u16>,
    session: Option<String>,
    lines: VecDeque<String>,
    closed: bool,
}
impl Output {
    fn line(&mut self, line: &str, stdout: bool) {
        let line = super::ssh::printable(line);
        if stdout
            && let Some(session) = line.strip_prefix("Starting session with SessionId: ")
            && valid_session_id(session)
        {
            self.session = Some(session.into());
        }
        // The plugin prints this only after net.Listen succeeds. CLI StartSession
        // banners and stderr must not be mistaken for a listening socket.
        if stdout
            && let Some(rest) = line.strip_prefix("Port ")
            && let Some((port, session)) = rest.split_once(" opened for sessionId ")
            && let Ok(port) = port.parse::<u16>()
            && port != 0
            && let Some(session) = session.strip_suffix('.')
            && valid_session_id(session)
        {
            self.port = Some(port);
            self.session = Some(session.into());
        }
        if stdout
            && (line.starts_with("Exiting session with sessionId:")
                || line == "DBDELVE_SSM_CHILD_EXITED")
        {
            self.closed = true;
        }
        if !line.is_empty() {
            // CLI tracebacks can contain plugin argv, including session tokens.
            // Keep only classified messages; never surface arbitrary CLI output.
            if let Some(message) = diagnostic(&line) {
                if self.lines.len() == 20 {
                    self.lines.pop_front();
                }
                self.lines.push_back(message.into());
            }
        }
    }
    fn reason(&self) -> String {
        self.lines.back().cloned().unwrap_or_else(|| {
            "Check the AWS profile, region, instance connectivity, and installed tools.".into()
        })
    }
}
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
}
fn diagnostic(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("sso")
        && (lower.contains("expired") || lower.contains("token") || lower.contains("login"))
    {
        Some(
            "AWS SSO login has expired or is unavailable. Run aws sso login (with --profile for this connection), then retry.",
        )
    } else if lower.contains("accessdenied") || lower.contains("not authorized") {
        Some(
            "Access denied. Check permission to start sessions on this instance and the remote-host forwarding document.",
        )
    } else if lower.contains("targetnotconnected") {
        Some(
            "The SSM target is not connected. Check the target, region, agent, and network access.",
        )
    } else if lower.contains("sessionmanagerplugin is not found")
        || lower.contains("session-manager-plugin: not found")
    {
        Some("Session Manager plugin was not found. Install it or set its absolute path.")
    } else if lower.contains("config profile") || lower.contains("could not be found") {
        Some("The AWS profile could not be loaded. Check the profile in the connection settings.")
    } else if lower.contains("specify a region") {
        Some("Set an AWS region in the connection settings or AWS configuration.")
    } else if lower.contains("expiredtoken")
        || lower.contains("unable to locate credentials")
        || lower.contains("invalidclienttokenid")
    {
        Some("AWS credentials are missing, invalid, or expired. Refresh them and retry.")
    } else if lower.contains("connection to destination port failed") {
        Some(
            "The managed instance could not reach the database. Check its DNS and network access to the database host and port.",
        )
    } else {
        None
    }
}
// Both long-lived sessions and best-effort cleanup must drain both pipes:
// an unread pipe can block the CLI before it exits.
fn capture_output(child: &mut Child) -> Arc<Mutex<Output>> {
    let output = Arc::new(Mutex::new(Output::default()));
    drain(
        child.stdout.take().expect("stdout piped"),
        output.clone(),
        true,
    );
    drain(
        child.stderr.take().expect("stderr piped"),
        output.clone(),
        false,
    );
    output
}

fn drain(reader: impl Read + Send + 'static, output: Arc<Mutex<Output>>, stdout: bool) {
    thread::spawn(move || {
        let mut reader = reader;
        let mut bytes = [0; 1024];
        let mut line = Vec::new();
        let mut oversized = false;
        loop {
            let count = match reader.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &byte in &bytes[..count] {
                if byte == b'\n' {
                    if !oversized {
                        output
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .line(&String::from_utf8_lossy(&line), stdout);
                    }
                    line.clear();
                    oversized = false;
                } else if line.len() < 4096 {
                    line.push(byte);
                } else {
                    oversized = true;
                }
            }
        }
        if !oversized && !line.is_empty() {
            output
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .line(&String::from_utf8_lossy(&line), stdout);
        }
    });
}

pub(super) struct Tunnel {
    local: SocketAddr,
    process: Arc<Mutex<process::Process>>,
    output: Arc<Mutex<Output>>,
    config: SsmTunnel,
}
impl Tunnel {
    pub fn open(config: &SsmTunnel, host: &str, port: u16) -> Result<Self, DbError> {
        Self::open_with_timeout(config, host, port, READY_WITHIN)
    }
    fn open_with_timeout(
        config: &SsmTunnel,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Self, DbError> {
        let deadline = Instant::now() + timeout;
        config.check_cancelled()?;
        let args = arguments(config, host, port)?;
        let mut process = process::Process::spawn(command(config, &args)?)
            .map_err(|e| plain_error(format!("Could not start SSM tunnel: {e}")))?;
        let output = capture_output(process.child.as_mut().unwrap());
        let mut tunnel = Self {
            local: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            process: Arc::new(Mutex::new(process)),
            output,
            config: config.clone(),
        };
        if let Some(attempt) = config.attempt.clone() {
            let weak = Arc::downgrade(&tunnel.process);
            thread::spawn(move || {
                while !attempt.is_finished() {
                    let Some(process) = weak.upgrade() else {
                        return;
                    };
                    if attempt.is_cancelled() {
                        process.lock().unwrap_or_else(|p| p.into_inner()).stop();
                        return;
                    }
                    drop(process);
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }
        loop {
            config.check_cancelled()?;
            let running = tunnel
                .process
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .running();
            let (port, closed, reason) = {
                let out = tunnel.output.lock().unwrap_or_else(|p| p.into_inner());
                (out.port, out.closed, out.reason())
            };
            if !running || closed {
                return Err(plain_error(format!(
                    "SSM session ended before it was ready. {reason}"
                )));
            }
            if let Some(port) = port {
                config.check_cancelled()?;
                tunnel.local.set_port(port);
                return Ok(tunnel);
            }
            if Instant::now() >= deadline {
                return Err(plain_error(format!(
                    "SSM tunnel did not become ready within {} seconds. {reason}",
                    timeout.as_secs()
                )));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn dial(&self) -> Result<SocketAddr, DbError> {
        self.config.check_cancelled()?;
        let running = self
            .process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .running();
        let output = self.output.lock().unwrap_or_else(|p| p.into_inner());
        if running && !output.closed {
            Ok(self.local)
        } else {
            Err(plain_error(format!(
                "SSM tunnel closed. {}",
                output.reason()
            )))
        }
    }
    pub fn explain(&self, mut error: DbError) -> DbError {
        let output = self.output.lock().unwrap_or_else(|p| p.into_inner());
        if !output.lines.is_empty() {
            error
                .message
                .push_str(&format!("\nSSM: {}", output.reason()));
        }
        error
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop();
        let session = self
            .output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session
            .clone();
        let terminate = session.and_then(|session| {
            let mut args = vec![
                "ssm".into(),
                "terminate-session".into(),
                "--session-id".into(),
                session,
            ];
            for (flag, value) in [
                ("--profile", &self.config.profile),
                ("--region", &self.config.region),
            ] {
                if let Some(value) = value {
                    args.push(flag.into());
                    args.push(value.clone());
                }
            }
            command(&self.config, &args).ok()
        });
        if let Some(command) = terminate {
            thread::spawn(move || {
                if let Ok(mut process) = process::Process::spawn(command) {
                    capture_output(process.child.as_mut().unwrap());
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while process.running() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> SsmTunnel {
        SsmTunnel {
            target: "i-0123456789abcdef0".into(),
            ..Default::default()
        }
    }
    #[test]
    fn windows_plugin_override_refuses_a_shadowing_cli_directory() {
        let root =
            std::env::temp_dir().join(format!("dbdelve-plugin-lookup-{}", std::process::id()));
        let cli = root.join("cli");
        let plugins = root.join("plugins");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();
        let chosen = plugins.join("session-manager-plugin.exe");
        let shadow = cli.join("session-manager-plugin.exe");
        std::fs::write(&chosen, b"chosen").unwrap();
        std::fs::write(&shadow, b"different").unwrap();
        assert!(windows_plugin_lookup(&cli.join("aws.exe"), &chosen).is_err());
        assert!(windows_plugin_lookup(&cli.join("aws.exe"), &shadow).is_ok());
        std::fs::remove_file(&shadow).unwrap();
        assert!(windows_plugin_lookup(&cli.join("aws.exe"), &chosen).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parameters_are_json_arguments_not_shell_source() {
        let mut config = config();
        config.profile = Some("a profile;$(echo nope)".into());
        let args = arguments(&config, "db.example.test", 5432).unwrap();
        assert_eq!(args.last().unwrap(), "a profile;$(echo nope)");
        let params: serde_json::Value = serde_json::from_str(&args[7]).unwrap();
        assert_eq!(params["host"][0], "db.example.test");
        assert_eq!(params["localPortNumber"][0], "0");
        assert!(!args.contains(&"--region".to_string()));
    }
    #[test]
    fn refuses_socket_hosts_lists_zero_ports_and_invalid_targets() {
        for host in ["", "/var/run/postgresql", "host1,host2", "host\nname"] {
            assert!(arguments(&config(), host, 5432).is_err());
        }
        assert!(arguments(&config(), "host", 0).is_err());
        for target in ["", "-x", "ecs:cluster", "i-nothex"] {
            assert!(
                SsmTunnel {
                    target: target.into(),
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
    }
    #[test]
    fn only_a_valid_stdout_bound_port_is_ready() {
        let mut out = Output::default();
        out.line("Starting session with SessionId: user-123", true);
        out.line("Port 15432 opened for sessionId user-123.", false);
        out.line("Port 0 opened for sessionId user-123.", true);
        out.line("Port 65536 opened for sessionId user-123.", true);
        assert_eq!(out.port, None);
        out.line("Port 15432 opened for sessionId user-123.", true);
        assert_eq!(out.port, Some(15432));
        out.line("Exiting session with sessionId: user-123.", true);
        assert!(out.closed);
    }
    #[test]
    fn watchdog_exit_invalidates_readiness_before_cleanup_finishes() {
        let mut out = Output::default();
        out.line("Port 15432 opened for sessionId test-session.", true);
        out.line("DBDELVE_SSM_CHILD_EXITED", true);
        assert!(out.closed);
    }

    #[test]
    fn cli_traceback_cannot_expose_session_tokens() {
        let mut out = Output::default();
        out.line(
            "CalledProcessError: command ['plugin', 'SECRET_TOKEN'] failed",
            false,
        );
        assert!(!out.reason().contains("SECRET_TOKEN"));
        out.line("Error loading SSO Token: expired", false);
        assert!(out.reason().contains("aws sso login"));
    }
    #[test]
    fn cancelled_attempt_spawns_nothing() {
        let attempt = ConnectAttempt::default();
        attempt.cancel();
        let config = SsmTunnel {
            attempt: Some(attempt),
            ..config()
        };
        assert!(
            Tunnel::open(&config, "host", 5432)
                .err()
                .unwrap()
                .message
                .contains("cancelled")
        );
    }
    #[test]
    fn dropping_attempt_owner_cancels_its_worker() {
        let attempt = ConnectAttempt::default();
        let owner = attempt.guard();
        assert!(!attempt.is_cancelled());
        drop(owner);
        assert!(attempt.is_cancelled());
    }
}

#[cfg(all(test, unix, any(target_os = "macos", target_env = "gnu")))]
fn fake_cli() -> Option<i32> {
    use std::io::Write;
    let role = std::env::var("DBDELVE_TEST_SSM_ROLE").ok()?;
    let record = |text: String| {
        if let Ok(path) = std::env::var("DBDELVE_TEST_SSM_LOG")
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(file, "{} {text}", std::process::id());
        }
    };
    record(format!("role={role}"));
    if role == "cli" {
        return Some(
            Command::new(std::env::current_exe().unwrap())
                .env("DBDELVE_TEST_SSM_ROLE", "plugin")
                .status()
                .unwrap()
                .code()
                .unwrap_or(1),
        );
    }
    let behavior = std::env::var("DBDELVE_TEST_SSM_BEHAVIOR").unwrap();
    if behavior == "fail" {
        let _ = writeln!(std::io::stderr(), "TargetNotConnected");
        return Some(1);
    }
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    record(format!("bound {port} behavior={behavior}"));
    if behavior != "hang" {
        let _ = writeln!(
            std::io::stdout(),
            "Port {port} opened for sessionId test-session."
        );
        let _ = std::io::stdout().flush();
    }
    if behavior == "exit" {
        thread::sleep(Duration::from_millis(150));
        return Some(1);
    }
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        record(format!("accepted {:?}", stream.peer_addr()));
        record(format!("write {:?}", stream.write_all(b"ssm-test")));
    }
    Some(0)
}

#[cfg(all(test, unix, any(target_os = "macos", target_env = "gnu")))]
mod lifecycle_tests {
    use super::*;
    use std::{fs, net::TcpStream, os::unix::fs::PermissionsExt};
    struct Fixture {
        root: PathBuf,
        config: SsmTunnel,
    }
    impl Fixture {
        fn new(behavior: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "dbdelve ssm {} {} {}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&root).unwrap();
            let aws = root.join("aws");
            let executable = std::env::current_exe().unwrap();
            let quoted = executable.to_str().unwrap().replace('\'', "'\\''");
            let log = root.join("test.log").display().to_string();
            fs::write(&aws, format!("#!/bin/sh\nif [ \"$2\" = terminate-session ]; then exit 0; fi\nexport DBDELVE_TEST_SSM_ROLE=cli DBDELVE_TEST_SSM_BEHAVIOR={behavior}\nexport DBDELVE_TEST_SSM_LOG='{log}'\nexec '{quoted}'\n")).unwrap();
            fs::set_permissions(&aws, fs::Permissions::from_mode(0o700)).unwrap();
            let plugin = root.join("session-manager-plugin");
            fs::write(&plugin, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&plugin, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                root,
                config: SsmTunnel {
                    target: "i-0123456789abcdef0".into(),
                    aws_cli: Some(aws),
                    session_manager_plugin: Some(plugin),
                    ..Default::default()
                },
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn ready_tunnel_dials_and_drop_releases_port() {
        let fixture = Fixture::new("ready");
        let tunnel = Tunnel::open(&fixture.config, "db.example", 5432).unwrap_or_else(|e| {
            panic!(
                "{}; fake log: {:?}",
                e.message,
                fs::read_to_string(fixture.root.join("test.log"))
            )
        });
        let address = tunnel.dial().unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut response = [0; 8];
        if let Err(error) = stream.read_exact(&mut response) {
            thread::sleep(Duration::from_millis(50));
            panic!(
                "read failed {error}; address={address}; diag={:?}",
                fs::read_to_string(fixture.root.join("test.log"))
            );
        }
        assert_eq!(&response, b"ssm-test");
        drop(stream);
        drop(tunnel);
        let deadline = Instant::now() + Duration::from_secs(3);
        while TcpStream::connect(address).is_ok() {
            assert!(Instant::now() < deadline, "listener survived drop");
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn plugin_exit_is_not_a_live_tunnel() {
        let fixture = Fixture::new("exit");
        let tunnel = Tunnel::open(&fixture.config, "db.example", 5432).unwrap_or_else(|e| {
            panic!(
                "{}; fake log: {:?}",
                e.message,
                fs::read_to_string(fixture.root.join("test.log"))
            )
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while tunnel.dial().is_ok() {
            assert!(Instant::now() < deadline, "dead plugin reported live");
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn timeout_and_cancellation_end_startup() {
        let mut fixture = Fixture::new("hang");
        let error = Tunnel::open_with_timeout(
            &fixture.config,
            "db.example",
            5432,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(error.message.contains("did not become ready"));
        let attempt = ConnectAttempt::default();
        fixture.config.attempt = Some(attempt.clone());
        let worker = thread::spawn(move || Tunnel::open(&fixture.config, "db.example", 5432));
        thread::sleep(Duration::from_millis(100));
        attempt.cancel();
        assert!(
            worker
                .join()
                .unwrap()
                .err()
                .unwrap()
                .message
                .contains("cancelled")
        );
    }
    #[test]
    fn cancellation_after_readiness_closes_the_tunnel() {
        let mut fixture = Fixture::new("ready");
        let attempt = ConnectAttempt::default();
        fixture.config.attempt = Some(attempt.clone());
        let tunnel = Tunnel::open(&fixture.config, "db.example", 5432).unwrap_or_else(|e| {
            panic!(
                "{}; fake log: {:?}",
                e.message,
                fs::read_to_string(fixture.root.join("test.log"))
            )
        });
        attempt.cancel();
        let deadline = Instant::now() + Duration::from_secs(3);
        while tunnel.dial().is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn finishing_attempt_keeps_established_tunnel_alive() {
        let mut fixture = Fixture::new("ready");
        let attempt = ConnectAttempt::default();
        let guard = attempt.guard();
        fixture.config.attempt = Some(attempt.clone());
        let tunnel = Tunnel::open(&fixture.config, "db.example", 5432).unwrap_or_else(|e| {
            panic!(
                "{}; fake log: {:?}",
                e.message,
                fs::read_to_string(fixture.root.join("test.log"))
            )
        });
        attempt.finish().unwrap();
        drop(guard);
        assert!(!attempt.is_cancelled());
        assert!(tunnel.dial().is_ok());
    }
}

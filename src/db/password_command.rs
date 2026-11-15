//! A login's password printed by a command the user gives, as pgAdmin's
//! "password exec command" does: how a cloud IAM token, which lasts minutes,
//! reaches a profile that outlives it.
//!
//! Run at each engine's login site rather than once at connect, so a side
//! session, a cancel's own login or a redial opened after the token has
//! expired signs in with a fresh one.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::{DbError, Engine, ServerConfig, plain_error};

/// A command whose standard output is the password, `%HOSTNAME%`, `%PORT%`
/// and `%USERNAME%` standing for the profile's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordCommand {
    pub command: String,
    /// Seconds its output is reused for; 0 runs it for every login.
    pub expires: u32,
}

/// As long as an ssh tunnel is given to come up, which can wait on a hardware
/// key's touch too.
const FINISH_WITHIN: Duration = Duration::from_secs(30);

/// How much of a failed command's stderr its error shows.
const SAID_LINES: usize = 3;

/// What is kept of each stream: far more than any password, and than the last
/// lines of stderr need, while a runaway command cannot fill memory.
const KEPT_BYTES: usize = 64 * 1024;

/// How every error from running the command begins, so a log can leave one
/// out even where another error wraps it.
const SAID_BY_US: &str = "The password command";

/// pgAdmin's spellings, so a command copied from it works unchanged.
const PLACEHOLDERS: [&str; 3] = ["%HOSTNAME%", "%PORT%", "%USERNAME%"];

/// Outputs by the command line that printed them, with when it ran and for how
/// long the run that cached it said to keep it. Memory only, never disk.
static CACHE: Mutex<BTreeMap<String, Cached>> = Mutex::new(BTreeMap::new());

struct Cached {
    password: String,
    ran: Instant,
    kept_for: Duration,
}

impl ServerConfig {
    /// What a login sends as the password: the one typed, or the command's
    /// output when there is a command.
    pub(super) fn login_password(&self, engine: Engine) -> Result<Cow<'_, str>, DbError> {
        let Some(command) = &self.password_command else {
            return Ok(Cow::Borrowed(&self.password));
        };
        let line = command_line(&command.command, self, engine)?;
        let kept_for = Duration::from_secs(command.expires.into());
        if let Some(password) = cached(&line, kept_for) {
            tracing::debug!("password command output reused");
            return Ok(Cow::Owned(password));
        }
        let ran = Instant::now();
        let password = run(&line, FINISH_WITHIN)?;
        if !kept_for.is_zero() {
            cache().insert(
                line,
                Cached {
                    password: password.clone(),
                    ran,
                    kept_for,
                },
            );
        }
        Ok(Cow::Owned(password))
    }

    /// What a login gave, the cached output dropped if it failed, so the next
    /// login (a side session, a cancel's, a redial, Reconnect or Test) runs
    /// the command again rather than resend a token the server may have
    /// refused. Called at each login site with the server it signed in to: a
    /// Postgres service fills in what the profile leaves blank, placeholders
    /// included.
    pub(super) fn forgetting_on_failure<T>(
        &self,
        engine: Engine,
        signed_in: Result<T, DbError>,
    ) -> Result<T, DbError> {
        if signed_in.is_err() {
            self.forget_login_password(engine);
        }
        signed_in
    }

    pub(super) fn forget_login_password(&self, engine: Engine) {
        let Some(command) = &self.password_command else {
            return;
        };
        if let Ok(line) = command_line(&command.command, self, engine) {
            cache().remove(&line);
        }
    }
}

/// An error's text as a log may carry it. A password command's can quote its
/// stderr, which can echo the secret it printed (`set -x` does), so none of
/// it is logged at any level, unlike a server's connect error.
///
/// ponytail: matched by text, since `DbError` is built as a literal in over a
/// hundred places and the connect logs only hold its message. Its ceiling is
/// an unrelated error that quotes "The password command", whose debug line is
/// then left out too; the user still sees it. A flag the resolver sets on
/// `DbError`, carried to these log sites, is the upgrade if that ever costs a
/// diagnosis.
pub fn loggable_error(message: &str) -> &str {
    if message.contains(SAID_BY_US) {
        "(a password command's error, not logged)"
    } else {
        message
    }
}

fn cache() -> std::sync::MutexGuard<'static, BTreeMap<String, Cached>> {
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cached(line: &str, kept_for: Duration) -> Option<String> {
    if kept_for.is_zero() {
        return None;
    }
    let mut cache = cache();
    cache.retain(|_, cached| cached.ran.elapsed() < cached.kept_for);
    cache
        .get(line)
        .filter(|cached| cached.ran.elapsed() < kept_for)
        .map(|cached| cached.password.clone())
}

/// The command with each placeholder replaced by its value, quoted for the
/// shell. One pass, so a value that spells a placeholder stays as written.
/// The host is the profile's, never a tunnel's local end: a token is signed
/// for the server it names.
fn command_line(template: &str, server: &ServerConfig, engine: Engine) -> Result<String, DbError> {
    let port = login_port(server, engine).map_or_else(String::new, |port| port.to_string());
    let values = [
        ("host", server.host.as_str()),
        ("port", port.as_str()),
        ("username", server.user.as_str()),
    ];
    let mut line = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('%') {
        line.push_str(&rest[..at]);
        rest = &rest[at..];
        match PLACEHOLDERS
            .into_iter()
            .zip(values)
            .find(|(placeholder, _)| rest.starts_with(*placeholder))
        {
            Some((placeholder, (name, value))) => {
                if !bare(&template[..template.len() - rest.len()]) {
                    return Err(not_bare(placeholder));
                }
                line.push_str(&quoted(value).ok_or_else(|| unquotable(name))?);
                rest = &rest[placeholder.len()..];
            }
            None => {
                line.push('%');
                rest = &rest[1..];
            }
        }
    }
    line.push_str(rest);
    Ok(line)
}

/// The port the login dials when the profile names none.
fn login_port(server: &ServerConfig, engine: Engine) -> Option<u16> {
    match engine {
        Engine::ClickHouse => Some(super::clickhouse::port(server)),
        _ => server.port.or(engine.default_port()),
    }
}

/// Whether the shell reads what follows `before` outside any quotes and not
/// escaped, the one place a value quoted by `quoted` stays one word.
fn bare(before: &str) -> bool {
    if cfg!(windows) {
        bare_for_cmd(before)
    } else {
        bare_for_sh(before)
    }
}

/// Quotes and backslashes as sh reads them. Past a backtick, a comment, a
/// heredoc, `$'…'`, or `$(`/`${` inside double quotes, where quoting nests,
/// the scan loses track and admits nothing more.
fn bare_for_sh(before: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut chars = before.chars().peekable();
    while let Some(char) = chars.next() {
        let next = chars.peek().copied();
        match (quote, char) {
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Single, _) => {}
            _ if escaped => escaped = false,
            (_, '\\') => escaped = true,
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, '`' | '#') | (Quote::Double, '`') => return false,
            (Quote::None, '<') if next == Some('<') => return false,
            (Quote::None, '$') if next == Some('\'') => return false,
            (Quote::Double, '$') if matches!(next, Some('(' | '{')) => return false,
            _ => {}
        }
    }
    matches!(quote, Quote::None) && !escaped && !before.ends_with('$')
}

/// cmd's own rules: a double quote opens or closes, and outside quotes a
/// caret escapes the next character.
fn bare_for_cmd(before: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for char in before.chars() {
        match char {
            _ if escaped => escaped = false,
            '"' => quoted = !quoted,
            '^' if !quoted => escaped = true,
            _ => {}
        }
    }
    !quoted && !escaped
}

fn not_bare(placeholder: &str) -> DbError {
    plain_error(format!(
        "The password command was not run: {placeholder} is inside quotes, or after an \
         escape or other shell syntax that would undo the quoting DBDelve gives its value. \
         Write each placeholder unquoted, as in --username %USERNAME%: DBDelve quotes \
         its value itself."
    ))
}

fn quoted(value: &str) -> Option<String> {
    if cfg!(windows) {
        quoted_for_cmd(value)
    } else {
        Some(quoted_for_sh(value))
    }
}

fn quoted_for_sh(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// ponytail: cmd has no escape inside double quotes for `"` itself, and
/// expands `%` (and `!`, under delayed expansion) there anyway, while a
/// trailing `\` would escape the closing quote for the program's own argument
/// parser. A value with any of those is refused rather than half-quoted; a
/// caret-escaping quoter is the upgrade if a real host or username needs one.
fn quoted_for_cmd(value: &str) -> Option<String> {
    let unsafe_char = |char: char| matches!(char, '"' | '%' | '!') || char.is_control();
    (!value.contains(unsafe_char) && !value.ends_with('\\')).then(|| format!("\"{value}\""))
}

fn unquotable(name: &str) -> DbError {
    plain_error(format!(
        "The password command was not run: the {name} has a character the command shell \
         cannot be given safely (a double quote, %, ! or a trailing backslash)."
    ))
}

fn run(line: &str, within: Duration) -> Result<String, DbError> {
    let started = Instant::now();
    let ran = output(line, within);
    let elapsed = started.elapsed();
    match &ran {
        Ok(_) => tracing::info!(?elapsed, "password command ran"),
        Err(_) => tracing::warn!(?elapsed, "password command failed"),
    }
    ran
}

fn output(line: &str, within: Duration) -> Result<String, DbError> {
    let mut command = shell(line);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = super::spawn(&mut command).map_err(|error| {
        plain_error(format!(
            "The password command could not be started: {error}"
        ))
    })?;
    // cmd's children are not ours to kill, so the job owns them: `stop` ends
    // cmd, and this handle's drop on return ends the rest and closes the
    // pipes the drains are reading.
    #[cfg(windows)]
    let _job = match super::ssh::kill_on_close_job(&child) {
        Ok(job) => job,
        Err(error) => {
            stop(&mut child);
            return Err(plain_error(format!(
                "The password command could not be started: {error}"
            )));
        }
    };
    let deadline = Instant::now() + within;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let unfinished = || {
        plain_error(format!(
            "The password command did not finish within {} seconds.",
            within.as_secs()
        ))
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                stop(&mut child);
                return Err(unfinished());
            }
            Err(error) => {
                stop(&mut child);
                return Err(plain_error(format!(
                    "The password command could not be waited for: {error}"
                )));
            }
        }
    };
    // Something the command left running in the background can still hold
    // its output open.
    let read = |pipe: mpsc::Receiver<Drained>| {
        pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    };
    let (Ok(stdout), Ok(stderr)) = (read(stdout), read(stderr)) else {
        stop(&mut child);
        return Err(unfinished());
    };
    if !status.success() {
        return Err(failure(status, &said(&stderr.last)));
    }
    if stdout.overflowed {
        return Err(plain_error(format!(
            "The password command printed more than {} KiB, more than any password.",
            KEPT_BYTES / 1024
        )));
    }
    let password = String::from_utf8(stdout.last).map_err(|_| {
        plain_error("The password command printed something that is not text.".into())
    })?;
    match without_trailing_newlines(password) {
        password if password.is_empty() => {
            Err(plain_error("The password command printed nothing.".into()))
        }
        password => Ok(password),
    }
}

struct Drained {
    /// The last `KEPT_BYTES` read.
    last: Vec<u8>,
    overflowed: bool,
}

/// Reads to the end, so a command never blocks on a full pipe, keeping only
/// the last `KEPT_BYTES`.
fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Drained> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut last = Vec::new();
            let mut overflowed = false;
            let mut chunk = [0; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => last.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
                // Trimmed only once twice the limit is held, so the bytes
                // moved stay proportional to the bytes read.
                if last.len() > 2 * KEPT_BYTES {
                    last.drain(..last.len() - KEPT_BYTES);
                    overflowed = true;
                }
            }
            if last.len() > KEPT_BYTES {
                last.drain(..last.len() - KEPT_BYTES);
                overflowed = true;
            }
            let _ = sender.send(Drained { last, overflowed });
        });
    }
    receiver
}

/// What the line printed, less the line ending `echo` and most programs end
/// it with. Anything else, spaces included, may be part of the password.
fn without_trailing_newlines(mut output: String) -> String {
    while output.ends_with('\n') {
        output.pop();
        if output.ends_with('\r') {
            output.pop();
        }
    }
    output
}

/// The last lines of stderr, as an error can show them.
fn said(stderr: &[u8]) -> String {
    let lines: Vec<String> = String::from_utf8_lossy(stderr)
        .lines()
        .map(super::ssh::printable)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(SAID_LINES)..].join("\n")
}

/// The shell's own exit status for a program it could not find.
const NOT_FOUND: i32 = if cfg!(windows) { 9009 } else { 127 };

fn failure(status: ExitStatus, said: &str) -> DbError {
    if status.code() == Some(NOT_FOUND) {
        let opened_from = if cfg!(target_os = "macos") {
            "the Finder or the Dock"
        } else {
            "a desktop launcher"
        };
        let mut message = format!(
            "The password command's program was not found. If it is installed, give its full path: an app \
             opened from {opened_from} does not see the PATH your shell sets."
        );
        if !said.is_empty() {
            message.push('\n');
            message.push_str(said);
        }
        return plain_error(message);
    }
    match said.is_empty() {
        true => plain_error(format!("The password command failed ({status}).")),
        false => plain_error(format!("The password command failed: {said}")),
    }
}

#[cfg(unix)]
fn shell(line: &str) -> Command {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("/bin/sh");
    command.args(["-c", line]);
    // A group of its own, so a timeout stops whatever the line started and
    // not only the shell.
    command.process_group(0);
    super::ssh::unmask_signals(&mut command);
    command
}

#[cfg(windows)]
fn shell(line: &str) -> Command {
    use std::os::windows::process::CommandExt;

    let mut command = Command::new("cmd");
    // `/S` has cmd strip exactly the outer quotes and keep every other, and
    // `/D` skips AutoRun, whose output would be read as the password. Raw,
    // since std quotes for a program's argument parser, which cmd is not.
    command
        .args(["/D", "/S", "/C"])
        .raw_arg(format!("\"{line}\""))
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
}

fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(group) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill touches no memory; the negated pid names the group
        // `shell` put the command in.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(user: &str, command: &str, expires: u32) -> ServerConfig {
        ServerConfig {
            host: "db.example.test".into(),
            user: user.into(),
            password: "typed".into(),
            password_command: Some(PasswordCommand {
                command: command.into(),
                expires,
            }),
            ..ServerConfig::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn ssm_password_command_receives_the_remote_endpoint() {
        let mut server = server("reader", "printf '%s:%s' %HOSTNAME% %PORT%", 0);
        server.ssm = Some(super::super::SsmTunnel {
            target: "i-0123456789abcdef0".into(),
            ..Default::default()
        });
        assert_eq!(
            server.login_password(Engine::Postgres).unwrap(),
            "db.example.test:5432"
        );
    }

    #[test]
    fn without_a_command_the_typed_password_is_sent() {
        let server = ServerConfig {
            password: "typed".into(),
            ..ServerConfig::default()
        };
        assert_eq!(server.login_password(Engine::Postgres).unwrap(), "typed");
    }

    #[test]
    fn each_placeholder_is_replaced_quoted_and_others_are_left_alone() {
        let server = server("me", "", 0);
        let line = command_line(
            "token --host %HOSTNAME% --port %PORT% --user %USERNAME% %PATH% 100%",
            &server,
            Engine::Postgres,
        )
        .unwrap();
        let quote = |value: &str| quoted(value).unwrap();
        assert_eq!(
            line,
            format!(
                "token --host {} --port {} --user {} %PATH% 100%",
                quote("db.example.test"),
                quote("5432"),
                quote("me"),
            )
        );
    }

    /// Unix only: cmd is never given a value with a `%` in it.
    #[cfg(unix)]
    #[test]
    fn a_value_that_spells_a_placeholder_stays_as_written() {
        let server = server("%PORT%", "", 0);
        let line = command_line("token --user %USERNAME%", &server, Engine::Postgres).unwrap();
        assert_eq!(line, "token --user '%PORT%'");
    }

    #[test]
    fn a_placeholder_inside_quotes_refuses_the_command() {
        let server = server("$(printf INJECTED >&2)", "", 0);
        let error =
            command_line(r#"printf %s "%USERNAME%""#, &server, Engine::Postgres).unwrap_err();
        assert!(
            error
                .message
                .starts_with("The password command was not run: %USERNAME% is inside quotes"),
            "{}",
            error.message
        );
    }

    #[test]
    fn sh_admits_a_placeholder_only_outside_quotes_and_escapes() {
        for admitted in [
            "token --user ",
            r#"printf "%s" \" "#,
            r"printf 'it'\''s' ",
            r#"printf "a\"b" "#,
            "token --user=",
            "token $(id) ",
        ] {
            assert!(bare_for_sh(admitted), "{admitted}");
        }
        for refused in [
            r#"printf %s ""#,
            "printf %s '",
            r#"printf %s "it's "#,
            r"printf %s \",
            "printf %s 'unclosed ",
            "printf %s `echo ",
            r#"printf "$(echo " "#,
            "printf %s $",
            "printf %s $'a\\' ' ",
            "cat <<EOF\n'",
            "# it's\nprintf %s '",
        ] {
            assert!(!bare_for_sh(refused), "{refused}");
        }
    }

    #[test]
    fn cmd_admits_a_placeholder_only_outside_quotes_and_carets() {
        for admitted in ["token --user ", r#"token "a b" "#, r#"token ^" "#] {
            assert!(bare_for_cmd(admitted), "{admitted}");
        }
        for refused in [r#"token ""#, r#"token "a b"#, "token ^"] {
            assert!(!bare_for_cmd(refused), "{refused}");
        }
    }

    #[test]
    fn the_port_is_the_one_the_login_dials() {
        let mut server = server("u", "", 0);
        assert_eq!(login_port(&server, Engine::MySql), Some(3306));
        server.sslmode = crate::tls::SslMode::Require;
        assert_eq!(login_port(&server, Engine::ClickHouse), Some(8443));
        server.port = Some(1);
        assert_eq!(login_port(&server, Engine::ClickHouse), Some(1));
    }

    #[test]
    fn sh_quoting_keeps_a_quote_a_dollar_and_spaces() {
        assert_eq!(quoted_for_sh("o'n $x @y"), r"'o'\''n $x @y'");
    }

    #[test]
    fn cmd_quoting_refuses_what_it_cannot_quote() {
        assert_eq!(
            quoted_for_cmd("o'n $x @y & z").as_deref(),
            Some("\"o'n $x @y & z\"")
        );
        assert_eq!(
            quoted_for_cmd(r"DOMAIN\me").as_deref(),
            Some("\"DOMAIN\\me\"")
        );
        for refused in ["a\"b", "%PATH%", "a!b", r"a\", "a\nb"] {
            assert_eq!(quoted_for_cmd(refused), None, "{refused}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_username_reaches_the_command_exactly_as_typed() {
        let user = "o'neil $HOME `id` me@example.com";
        let server = server(user, "printf %s %USERNAME%", 0);
        assert_eq!(server.login_password(Engine::Postgres).unwrap(), user);
    }

    #[cfg(unix)]
    #[test]
    fn only_trailing_line_endings_are_trimmed() {
        assert_eq!(run(r"printf ' pw \r\n\n'", FINISH_WITHIN).unwrap(), " pw ");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_prints_nothing_fails() {
        let error = run(r"printf '\n'", FINISH_WITHIN).unwrap_err();
        assert_eq!(error.message, "The password command printed nothing.");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_shows_its_last_stderr_lines() {
        let error = run(
            r"printf 'one\ntwo\n\033[31mthree\033[0m\nfour\n' >&2; exit 3",
            FINISH_WITHIN,
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "The password command failed: two\nthree\nfour"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_past_the_limit_is_an_error_and_stderr_keeps_its_last_lines() {
        let error = run("yes | head -c 100000", FINISH_WITHIN).unwrap_err();
        assert_eq!(
            error.message,
            "The password command printed more than 64 KiB, more than any password."
        );
        let error = run(
            "yes | head -c 200000 >&2; echo last >&2; exit 1",
            FINISH_WITHIN,
        )
        .unwrap_err();
        assert_eq!(error.message, "The password command failed: y\ny\nlast");
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_quoting_stderr_is_never_logged() {
        let error = run("set -x; printf %s secret-token; exit 1", FINISH_WITHIN).unwrap_err();
        assert!(error.message.contains("secret-token"), "{}", error.message);
        assert!(!loggable_error(&error.message).contains("secret-token"));
        assert_eq!(loggable_error("server said no"), "server said no");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_program_asks_for_its_full_path() {
        let error = run("dbdelve-no-such-program", FINISH_WITHIN).unwrap_err();
        assert!(
            error
                .message
                .starts_with("The password command's program was not found. If it is installed, give its full path"),
            "{}",
            error.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_time_is_stopped() {
        let started = Instant::now();
        let error = run("sleep 5; printf late", Duration::from_millis(200)).unwrap_err();
        assert!(
            error
                .message
                .starts_with("The password command did not finish")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn output_is_reused_within_its_expiry_and_never_with_none() {
        let runs = |expires: u32| {
            let dir = std::env::temp_dir().join(format!(
                "dbdelve-password-command-{}-{expires}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let count = dir.join("runs");
            let _ = std::fs::remove_file(&count);
            let server = server(
                "u",
                &format!(
                    "printf x >> {}; printf token",
                    quoted_for_sh(&count.to_string_lossy())
                ),
                expires,
            );
            for _ in 0..2 {
                assert_eq!(server.login_password(Engine::Postgres).unwrap(), "token");
            }
            let first = std::fs::read_to_string(&count).unwrap().len();
            server.forget_login_password(Engine::Postgres);
            assert_eq!(server.login_password(Engine::Postgres).unwrap(), "token");
            let after_forget = std::fs::read_to_string(&count).unwrap().len();
            let _ = std::fs::remove_dir_all(&dir);
            (first, after_forget)
        };
        assert_eq!(runs(60), (1, 2));
        assert_eq!(runs(0), (2, 3));
    }
}

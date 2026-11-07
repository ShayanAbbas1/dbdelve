//! The diagnostic log: a daily `dbdelve.<date>.log` beside `panic.log`, kept
//! on this machine for the user to read and attach to a bug report.
//!
//! # What may be written
//!
//! A log is only worth asking for if people can attach it without combing it
//! first, so every `tracing` call in DBDelve keeps to these:
//!
//! - **Never, at any level:** query text, result rows or cell values,
//!   passwords, tokens or other secrets, or a whole connection string or URI.
//! - **Not at info:** hostnames, ports, usernames, database names, the name a
//!   user gave a connection, or error text a server sent, which can quote a
//!   row (a Postgres duplicate key's detail does).
//! - **At info:** the version, OS and arch at startup; plugin and theme loads,
//!   by file name; connection lifecycle by engine and [`connection_number`],
//!   with the outcome, the stage a failure happened at, and durations; tunnels;
//!   cancels; exports and imports by format with row counts.
//! - **At debug**, which only Verbose logging or `DBDELVE_LOG` turns on:
//!   hostnames, database names and server error text, each in a `debug!` line
//!   beside the info line it explains.
//!
//! Third-party crates are held to warn, or info when verbose, and never lower
//! than that: a driver's debug output can carry the SQL it sends.

use std::{
    collections::BTreeMap,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt as _, reload};

/// An EnvFilter directive that replaces both the defaults and the Settings
/// toggle, for a developer or a bug that needs one crate louder.
const ENVIRONMENT: &str = "DBDELVE_LOG";

const PREFIX: &str = "dbdelve";
const SUFFIX: &str = "log";
const KEPT_FILES: usize = 7;

/// What Copy Diagnostics carries of the log: the last lines, read from no more
/// than the last bytes of the file, so a verbose day's log is not read whole
/// on the frame thread.
const TAIL_LINES: usize = 200;
const TAIL_BYTES: u64 = 256 * 1024;

struct Installed {
    filter: reload::Handle<EnvFilter, Registry>,
    from_environment: bool,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Installed first thing in `main`, beside the panic hook. A log that cannot be
/// opened leaves the app running without one: nothing here returns an error
/// or panics.
///
/// ponytail: rotated by day, seven files kept, because daily is what
/// tracing-appender rotates on. At info a day is a few kilobytes; a verbose
/// day has no ceiling but the disk. A size-based roller is the upgrade if a
/// verbose log is ever seen to grow past a few megabytes.
///
/// The file is written as each line is logged rather than through
/// tracing-appender's background writer: the app exits without unwinding to
/// drop that writer's guard, and the panic line is the one most worth having.
pub(crate) fn install() {
    let file = directory().and_then(|directory| {
        // Made here, since the appender reads the folder for files to prune
        // before it creates it, and says so on stderr.
        std::fs::create_dir_all(&directory).ok()?;
        RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix(PREFIX)
            .filename_suffix(SUFFIX)
            .max_log_files(KEPT_FILES)
            .build(directory)
            .ok()
    });
    let opened = file.is_some();
    let requested = std::env::var(ENVIRONMENT)
        .ok()
        .filter(|directive| !directive.trim().is_empty());
    let (filter, refused) = match requested.map(EnvFilter::try_new) {
        Some(Ok(filter)) => (Some(filter), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    let from_environment = filter.is_some();
    let (filter, handle) = reload::Layer::new(filter.unwrap_or_else(|| levels(false)));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(file.map(|file| fmt::layer().with_ansi(false).with_writer(file)))
        // `cargo run` keeps a console, which is where a developer looks.
        .with(cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr)));
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    let _ = INSTALLED.set(Installed {
        filter: handle,
        from_environment,
    });
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "dbdelve started"
    );
    if let Some(error) = refused {
        tracing::warn!(
            "{ENVIRONMENT} is not a filter this build reads, so it was ignored: {error}"
        );
    }
    if !opened {
        tracing::warn!("the log file could not be opened, so nothing is written to it");
    }
}

/// The default levels: DBDelve's own lines at info, everything else at warn,
/// one step louder each when verbose.
fn levels(verbose: bool) -> EnvFilter {
    let (own, others) = match verbose {
        true => ("debug", "info"),
        false => ("info", "warn"),
    };
    EnvFilter::new(format!("{others},dbdelve={own}"))
}

/// The Settings toggle, applied at once. `DBDELVE_LOG`, when it was set,
/// outranks it, and the flag is only remembered for Copy Diagnostics.
pub(crate) fn set_verbose(verbose: bool) {
    if VERBOSE.swap(verbose, Ordering::Relaxed) == verbose {
        return;
    }
    let Some(installed) = INSTALLED.get() else {
        return;
    };
    if installed.from_environment {
        tracing::info!(
            verbose,
            "verbose logging changed; {ENVIRONMENT} still decides"
        );
        return;
    }
    // Logged on whichever side of the change is still listening.
    if !verbose {
        tracing::info!("verbose logging off");
    }
    let _ = installed.filter.reload(levels(verbose));
    if verbose {
        tracing::info!("verbose logging on");
    }
}

/// What the log's level is, as Copy Diagnostics reports it.
fn verbosity() -> String {
    if INSTALLED
        .get()
        .is_some_and(|installed| installed.from_environment)
    {
        return format!(
            "{ENVIRONMENT}={}",
            std::env::var(ENVIRONMENT).unwrap_or_default()
        );
    }
    match VERBOSE.load(Ordering::Relaxed) {
        true => "on".to_string(),
        false => "off".to_string(),
    }
}

/// A number for each connection, the same for as long as DBDelve runs, so the
/// lines one connection writes can be told from another's without naming it.
pub(crate) fn connection_number(profile: &str) -> usize {
    static NUMBERS: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
    let mut numbers = NUMBERS.lock().unwrap_or_else(PoisonError::into_inner);
    let next = numbers.len() + 1;
    *numbers.entry(profile.to_string()).or_insert(next)
}

/// Where a panic goes when nobody is watching stderr. A `.app` is spawned by
/// launchd, so a panic message lands in the unified log and the abort that
/// follows writes an `.ips` trace that does not carry it -- every stranger's
/// bug report would read "it closed" with nothing to read after it. Installed
/// first thing in `main`, because the deaths hardest to guess at from outside
/// are the ones before a window exists.
///
/// The panic goes to the diagnostic log as well, but after `panic.log` has it:
/// that file is written with nothing but `std`, so it still gets the panic
/// when the log is what panicked.
///
/// ponytail: one appended file, never rotated. A few kilobytes per crash; if
/// that ever becomes a real number, truncate on open past some size.
pub(crate) fn install_panic_log() {
    let Some(directory) = directory() else {
        return;
    };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing in here may panic: a panic inside the hook aborts with less
        // to read than the one it was called for. Hence every result dropped
        // rather than unwrapped.
        use std::io::Write as _;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        if std::fs::create_dir_all(&directory).is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("panic.log"))
        {
            let _ = writeln!(
                file,
                "--- {at} (unix seconds)\n{info}\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
        tracing::error!("{info}");
        previous(info);
    }));
}

/// A log is state, not cache. macOS uses `Logs`, Linux the XDG state
/// directory — a cache cleaner is entitled to delete anything in the other
/// one. Windows uses LocalAppData for the same reason: it stays on this machine.
pub(crate) fn directory() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
        Some(PathBuf::from(home).join("Library/Logs/dbdelve"))
    }
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .filter(|value| !value.is_empty())
                    .map(|profile| PathBuf::from(profile).join("AppData").join("Local"))
            })?;
        Some(root.join("dbdelve"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
        Some(
            match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
                Some(state_home) => PathBuf::from(state_home).join("dbdelve"),
                None => PathBuf::from(home).join(".local/state/dbdelve"),
            },
        )
    }
}

/// The file being written today: the dates in the names sort as text.
fn current_log(directory: &Path) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(&format!("{PREFIX}.")) && name.ends_with(&format!(".{SUFFIX}"))
                })
        })
        .max()
}

/// The end of `path`, from a line boundary.
fn read_end(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let from = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(match from {
        0 => text.into_owned(),
        // Begun mid-line, and maybe mid-character: the first line is a part.
        _ => text
            .split_once('\n')
            .map_or_else(String::new, |(_, rest)| rest.to_string()),
    })
}

/// What Copy Diagnostics puts on the clipboard, read now.
pub(crate) fn diagnostics(plugins: &[&crate::db::Plugin]) -> String {
    let current = directory().and_then(|directory| current_log(&directory));
    let log = current.as_ref().map(|path| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name, read_end(path).map_err(|error| error.to_string()))
    });
    let plugins = plugins
        .iter()
        .map(|plugin| format!("{} {} ({})", plugin.name, plugin.version, plugin.id))
        .collect::<Vec<_>>();
    report(
        &[
            ("DBDelve", env!("CARGO_PKG_VERSION").to_string()),
            (
                "OS",
                format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            ),
            (
                "Plugins",
                match plugins.is_empty() {
                    true => "none".to_string(),
                    false => plugins.join(", "),
                },
            ),
            ("Verbose logging", verbosity()),
        ],
        log.as_ref()
            .map(|(name, text)| (name.as_str(), text.as_deref().map_err(String::as_str))),
    )
}

/// The facts, then the last [`TAIL_LINES`] of the log, as one fenced block for
/// a GitHub issue. The fence is longer than any run of backticks the log holds,
/// so a line quoting one cannot close it early.
fn report(facts: &[(&str, String)], log: Option<(&str, Result<&str, &str>)>) -> String {
    let mut body = facts
        .iter()
        .map(|(name, value)| format!("{name}: {value}\n"))
        .collect::<String>();
    match log {
        Some((name, Ok(text))) => {
            let lines = tail(text, TAIL_LINES);
            body.push_str(&format!(
                "\nLast {} lines of {name}:\n{lines}",
                lines.lines().count()
            ));
        }
        Some((name, Err(error))) => {
            body.push_str(&format!("\n{name} could not be read: {error}\n"))
        }
        None => body.push_str("\nNo log file was found.\n"),
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}text\n{body}{fence}\n")
}

/// The last `count` lines of `text`.
fn tail(text: &str, count: usize) -> &str {
    if count == 0 {
        return "";
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    match text.rmatch_indices('\n').nth(count - 1) {
        Some((at, _)) => &text[at + 1..],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{report, tail};

    #[test]
    fn a_report_is_one_fence_the_log_cannot_close() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a\nb", 5), "a\nb");
        let log = (1..=250)
            .map(|n| format!("line {n}\n"))
            .chain(["```\n".to_string()])
            .collect::<String>();
        let report = report(
            &[("DBDelve", "1.2.3".to_string())],
            Some(("dbdelve.2026-11-08.log", Ok(&log))),
        );
        assert!(report.starts_with("````text\nDBDelve: 1.2.3\n"));
        assert!(report.ends_with("```\n````\n"));
        assert!(report.contains("Last 200 lines of dbdelve.2026-11-08.log:\nline 52\n"));
        assert!(!report.contains("line 51\n"));
    }
}

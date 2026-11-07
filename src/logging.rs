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
    path::PathBuf,
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
/// outranks it.
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

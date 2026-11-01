//! One plugin process and the threads that keep its pipes moving.
//!
//! The reader is the thread everything leans on: it never stops reading the
//! plugin's stdout, never decodes Arrow and never holds a lock across a read,
//! so credit can always come back and `cancel` can always be answered. Callers
//! and `cancel` hand frames to the writer thread and never block on a pipe the
//! plugin stopped reading.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dbdelve_plugin_protocol::limits::{CLOSE_GRACE, STDERR_RING, WINDOW};
use dbdelve_plugin_protocol::{Frame, Kind, Request, Response, read_frame, write_frame};

use crate::db::DbError;
use crate::db::plugin::Plugin;

/// What the reader hands the request a frame belongs to.
pub(super) enum Routed {
    Control(Response),
    /// One Arrow IPC message, undecoded.
    Data(Vec<u8>),
    /// The plugin is gone, and why, worded for a sentence of its own.
    Reset(String),
}

pub(super) struct Host {
    pub(super) plugin: &'static Plugin,
    shared: Arc<Shared>,
    writer: Sender<Vec<u8>>,
    next_id: AtomicU64,
    stderr: Arc<Mutex<Ring>>,
    stderr_reader: JoinHandle<()>,
}

/// What the reader thread shares with the handle.
struct Shared {
    name: String,
    routing: Mutex<Routing>,
    /// Taken by whoever reaps it: the reader when the plugin dies or breaks the
    /// protocol, or the handle's drop.
    process: Mutex<Option<Process>>,
}

/// One lock for both, so a request is either registered before the reset and
/// told about it, or refused after it: never registered and left waiting.
#[derive(Default)]
struct Routing {
    /// Removed on the request's terminal frame, so a frame for an id missing
    /// here is one for a request that is finished or was never sent.
    pending: HashMap<u64, Pending>,
    reset: Option<String>,
}

struct Pending {
    sender: Sender<Routed>,
    /// A query's data credit in payload bytes, `None` for a request that takes
    /// no data. Signed: a single frame larger than the window may go when
    /// nothing is outstanding, which takes it below zero until its grant.
    credit: Option<i64>,
}

struct Process {
    child: Child,
    /// Closing it kills the plugin, so it lives exactly as long as the child
    /// is unreaped.
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl Host {
    pub(super) fn start(plugin: &'static Plugin) -> Result<Arc<Self>, DbError> {
        Self::spawn(plugin, Command::new(&plugin.executable))
    }

    /// `command` runs the plugin's executable; the tests add to its
    /// environment. The environment is otherwise inherited, which ODBC needs:
    /// `ODBCSYSINI`, `ODBCINI` and the vendors' own variables.
    pub(super) fn spawn(
        plugin: &'static Plugin,
        mut command: Command,
    ) -> Result<Arc<Self>, DbError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        crate::db::ssh::unmask_signals(&mut command);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let could_not_start = |error: io::Error| {
            failure(format!(
                "The {} plugin could not start: {}: {error}",
                plugin.name,
                plugin.executable.display()
            ))
        };
        let spawned = {
            let _spawning = lock(&SPAWNING);
            command.spawn()
        };
        let mut child = spawned.map_err(could_not_start)?;
        #[cfg(windows)]
        let job = match crate::db::ssh::kill_on_close_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(could_not_start(error));
            }
        };
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let shared = Arc::new(Shared {
            name: plugin.name.clone(),
            routing: Mutex::default(),
            process: Mutex::new(Some(Process {
                child,
                #[cfg(windows)]
                _job: job,
            })),
        });
        let (writer, frames) = mpsc::channel();
        thread::spawn({
            let shared = Arc::clone(&shared);
            move || write(stdin, frames, &shared)
        });
        thread::spawn({
            let shared = Arc::clone(&shared);
            move || read(stdout, &shared)
        });
        let ring = Arc::new(Mutex::new(Ring::default()));
        let stderr_reader = thread::spawn({
            let ring = Arc::clone(&ring);
            move || collect(stderr, &ring)
        });
        Ok(Arc::new(Self {
            plugin,
            shared,
            writer,
            next_id: AtomicU64::new(1),
            stderr: ring,
            stderr_reader,
        }))
    }

    /// Registers a request and sends it. `Err` is the reset error when the
    /// plugin is already gone: nothing is sent, and nothing ever starts another.
    pub(super) fn request(&self, request: Request) -> Result<(u64, Receiver<Routed>), DbError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, answers) = mpsc::channel();
        let credit = matches!(request, Request::Query { .. }).then_some(WINDOW as i64);
        {
            let mut routing = lock(&self.shared.routing);
            if let Some(cause) = &routing.reset {
                return Err(reset_error(cause, false));
            }
            routing.pending.insert(id, Pending { sender, credit });
        }
        if let Err(error) = self.write(id, &request) {
            lock(&self.shared.routing).pending.remove(&id);
            return Err(error);
        }
        Ok((id, answers))
    }

    /// `stop`, `cancel` or `credit` for a running query. Nothing is sent once
    /// its terminal frame has arrived: there is nothing left for it to reach.
    pub(super) fn send(&self, id: u64, request: Request) {
        {
            let mut routing = lock(&self.shared.routing);
            let Some(pending) = routing.pending.get_mut(&id) else {
                return;
            };
            // Accounted before the frame leaves, so the plugin can never spend
            // a grant the reader has not yet counted.
            if let (Request::Credit { bytes }, Some(credit)) = (&request, &mut pending.credit) {
                *credit += *bytes as i64;
            }
        }
        let _ = self.write(id, &request);
    }

    fn write(&self, id: u64, request: &Request) -> Result<(), DbError> {
        let mut frame = Vec::new();
        write_frame(&mut frame, Kind::Control, id, &request.to_payload()).map_err(|_| {
            failure(format!(
                "The request is larger than the {} MiB one frame to the {} plugin can carry.",
                dbdelve_plugin_protocol::limits::MAX_FRAME >> 20,
                self.plugin.name
            ))
        })?;
        // A writer that has gone ended because the plugin stopped reading, and
        // the reader reports that as the reset.
        let _ = self.writer.send(frame);
        Ok(())
    }

    pub(super) fn is_reset(&self) -> bool {
        lock(&self.shared.routing).reset.is_some()
    }

    /// `running_query`: the request that met the reset was a statement, which
    /// may or may not have committed before the plugin went.
    pub(super) fn reset_error(&self, running_query: bool) -> DbError {
        match &lock(&self.shared.routing).reset {
            Some(cause) => reset_error(cause, running_query),
            None => reset_error(
                &format!("The {} plugin stopped.", self.plugin.name),
                running_query,
            ),
        }
    }

    /// The error for a plugin that went before the connection opened, which
    /// says more through its stderr than its exit status does: a driver
    /// manager that would not load, a missing library.
    pub(super) fn startup_error(&self) -> DbError {
        let deadline = Instant::now() + STDERR_SETTLE;
        while !self.stderr_reader.is_finished() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        let tail = lock(&self.stderr).tail();
        if !tail.is_empty() {
            return failure(format!(
                "The {} plugin could not start: {tail}",
                self.plugin.name
            ));
        }
        match &lock(&self.shared.routing).reset {
            Some(cause) => failure(cause.clone()),
            None => failure(format!("The {} plugin could not start.", self.plugin.name)),
        }
    }

    /// Ends the plugin without waiting for it; the reader then sees its stdout
    /// close and reports the reset.
    pub(super) fn kill(&self) {
        self.shared.kill();
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> Option<u32> {
        lock(&self.shared.process)
            .as_ref()
            .map(|process| process.child.id())
    }
}

/// A plugin exits on stdin EOF, which the writer's end gives it once the
/// fields drop after this; `close` first lets it say goodbye to its server.
/// The reaper is the backstop for one that does neither.
impl Drop for Host {
    fn drop(&mut self) {
        // Registered so its `closed` answer is routed rather than read as a
        // frame for a request nobody sent.
        let _ = self.request(Request::Close);
        if let Some(process) = lock(&self.shared.process).take() {
            thread::spawn(move || process.reap(CLOSE_GRACE));
        }
    }
}

/// How long a dead plugin's stderr may take to reach EOF before its error is
/// worded without the rest: a grandchild still holding the pipe keeps it open.
const STDERR_SETTLE: Duration = Duration::from_secs(1);

const POLL: Duration = Duration::from_millis(10);

/// macOS has no `pipe2`: std creates a child's pipes and only then marks them
/// close-on-exec, so a plugin another thread forks in between inherits them for
/// life, and the plugin whose stdout it holds never reads as dead. One spawn at
/// a time keeps plugins out of each other's pipes.
///
/// ponytail: other children DBDelve spawns (ssh's watchdog shell) can still
/// inherit a plugin's pipe; one lock around every spawn in the app closes that.
static SPAWNING: Mutex<()> = Mutex::new(());

impl Shared {
    fn route(&self, frame: Frame) -> Result<(), String> {
        let id = frame.request;
        match frame.kind {
            Kind::Control => {
                let response = Response::from_payload(&frame.payload).map_err(|v| v.to_string())?;
                let pending = lock(&self.routing).pending.remove(&id).ok_or_else(|| {
                    format!("an answer for request {id}, which is not waiting for one")
                })?;
                let _ = pending.sender.send(Routed::Control(response));
            }
            Kind::Data => {
                let size = frame.payload.len() as i64;
                let mut routing = lock(&self.routing);
                let pending = routing
                    .pending
                    .get_mut(&id)
                    .ok_or_else(|| format!("result data for request {id}, which is not running"))?;
                let credit = pending
                    .credit
                    .as_mut()
                    .ok_or_else(|| format!("result data for request {id}, which is not a query"))?;
                if size > *credit && *credit != WINDOW as i64 {
                    return Err(format!(
                        "{size} bytes of result data with {credit} bytes of credit left"
                    ));
                }
                *credit -= size;
                let _ = pending.sender.send(Routed::Data(frame.payload));
            }
        }
        Ok(())
    }

    /// `broke`: the protocol violation that ended it, `None` when its stdout
    /// simply closed.
    fn reset(&self, broke: Option<String>) {
        let process = lock(&self.process).take();
        let status = process.and_then(|process| match broke {
            Some(_) => process.reap(Duration::ZERO),
            None => process.reap(CLOSE_GRACE),
        });
        let name = &self.name;
        let cause = match (broke, status) {
            (Some(violation), _) => {
                format!("The {name} plugin broke protocol 1 ({violation}) and was stopped.")
            }
            (None, Some(status)) => format!("The {name} plugin stopped ({status})."),
            (None, None) => format!("The {name} plugin stopped."),
        };
        let mut routing = lock(&self.routing);
        for (_, pending) in routing.pending.drain() {
            let _ = pending.sender.send(Routed::Reset(cause.clone()));
        }
        routing.reset = Some(cause);
    }

    fn kill(&self) {
        if let Some(process) = lock(&self.process).as_mut() {
            let _ = process.child.kill();
        }
    }
}

impl Process {
    /// Waits up to `grace` for the plugin to exit on its own, then kills it.
    fn reap(mut self, grace: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + grace;
        while let Ok(None) = self.child.try_wait() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break;
            }
            thread::sleep(POLL);
        }
        self.child.wait().ok()
    }
}

fn read(stdout: ChildStdout, shared: &Shared) {
    let mut input = BufReader::with_capacity(64 << 10, stdout);
    let broke = loop {
        match read_frame(&mut input) {
            Ok(Some(frame)) => {
                if let Err(violation) = shared.route(frame) {
                    break Some(violation);
                }
            }
            Ok(None) => break None,
            Err(violation) => break Some(violation.to_string()),
        }
    };
    shared.reset(broke);
}

fn write(mut stdin: ChildStdin, frames: Receiver<Vec<u8>>, shared: &Shared) {
    for frame in frames {
        if stdin.write_all(&frame).is_err() {
            // A plugin that stopped reading its stdin can no longer hear
            // `cancel` or `close`; ending it lets the reader report the reset.
            shared.kill();
            return;
        }
    }
}

fn collect(mut stderr: ChildStderr, ring: &Mutex<Ring>) {
    let mut chunk = [0; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => lock(ring).push(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// The last [`STDERR_RING`] bytes a plugin wrote to stderr.
#[derive(Default)]
struct Ring(VecDeque<u8>);

impl Ring {
    fn push(&mut self, chunk: &[u8]) {
        let chunk = &chunk[chunk.len().saturating_sub(STDERR_RING)..];
        let over = (self.0.len() + chunk.len()).saturating_sub(STDERR_RING);
        self.0.drain(..over);
        self.0.extend(chunk);
    }

    fn tail(&self) -> String {
        let (front, back) = self.0.as_slices();
        String::from_utf8_lossy(&[front, back].concat())
            .trim()
            .to_owned()
    }
}

fn reset_error(cause: &str, running_query: bool) -> DbError {
    let unknown = match running_query {
        true => " Whether the statement that was running committed is unknown.",
        false => "",
    };
    failure(format!(
        "{cause} The connection was reset: any open transaction, temporary tables and session \
         settings are gone.{unknown}"
    ))
}

pub(super) fn failure(message: String) -> DbError {
    DbError {
        message,
        position: None,
    }
}

/// A panic elsewhere must not take the reader down with it: it is what reports
/// every other failure.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stderr_ring_keeps_only_the_tail() {
        let mut ring = Ring::default();
        ring.push(b"first ");
        ring.push(&vec![b'x'; STDERR_RING - 3]);
        assert_eq!(ring.0.len(), STDERR_RING);
        assert!(ring.tail().starts_with("st x"));

        ring.push(&[vec![b'y'; STDERR_RING * 2], b"last".to_vec()].concat());
        assert_eq!(ring.0.len(), STDERR_RING);
        assert!(ring.tail().ends_with("ylast"));
    }
}

//! One plugin process and the threads that keep its pipes moving.
//!
//! The reader is the thread everything leans on: it never stops reading the
//! plugin's stdout, never decodes Arrow and never holds a lock across a read,
//! so credit can always come back and `cancel` can always be answered. Callers
//! and `cancel` hand frames to the writer thread and never block on a pipe the
//! plugin stopped reading.
//!
//! The plugin's death is read from its exit, by the watcher, and not from its
//! stdout closing: anything that inherited the pipe's write end, a driver's
//! helper process or another child forked at the wrong moment, keeps stdout
//! open after the plugin is gone.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dbdelve_plugin_protocol::limits::{
    CANCEL_DEADLINE, CLOSE_GRACE, STDERR_RING, WINDOW, credit_cost,
};
use dbdelve_plugin_protocol::{Frame, Kind, Request, Response, Violation, read_frame, write_frame};

use crate::db::DbError;
use crate::db::plugin::Plugin;

/// What the reader hands the request a frame belongs to.
pub(super) enum Routed {
    Control(Response),
    /// One Arrow IPC message, undecoded.
    Data(Vec<u8>),
    /// The plugin is gone, and why, worded for a sentence of its own.
    Reset(
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "callers word it from the host's own record")
        )]
        String,
    ),
}

pub(super) struct Host {
    pub(super) plugin: &'static Plugin,
    shared: Arc<Shared>,
    writer: Sender<Vec<u8>>,
    next_id: AtomicU64,
    stderr: Arc<Mutex<Ring>>,
    stderr_reader: JoinHandle<()>,
    /// One statement or catalog call at a time, as every built-in
    /// connection's mutex does. `stop`, `cancel` and `credit` never take it.
    session: Mutex<()>,
    /// The query a `cancel` may reach.
    running: Mutex<Option<u64>>,
    /// A plugin said the session is gone while the process lives on.
    lost: AtomicBool,
}

/// What the reader thread shares with the handle.
struct Shared {
    name: String,
    routing: Mutex<Routing>,
    /// Taken by whoever reaps it: the watcher once the plugin has exited, or
    /// the handle's drop.
    process: Mutex<Option<Process>>,
    /// The reader has stopped: stdout closed, or the plugin broke the protocol.
    read_done: AtomicBool,
}

/// One lock for both, so a request is either registered before the reset and
/// told about it, or refused after it: never registered and left waiting.
#[derive(Default)]
struct Routing {
    /// Removed on the request's terminal frame, so a frame for an id missing
    /// here is one for a request that is finished or was never sent.
    pending: HashMap<u64, Pending>,
    reset: Option<String>,
    /// The most data any query has had outstanding: sent by the plugin and
    /// not yet granted back.
    #[cfg(test)]
    peak: i64,
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
        let mut child = crate::db::spawn(&mut command).map_err(could_not_start)?;
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
            read_done: AtomicBool::new(false),
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
        thread::spawn({
            let shared = Arc::clone(&shared);
            move || watch(&shared)
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
            session: Mutex::new(()),
            running: Mutex::new(None),
            lost: AtomicBool::new(false),
        }))
    }

    /// Registers a request and sends it. `Err` is the reset error when the
    /// plugin is already gone: nothing is sent, and nothing ever starts another.
    /// A Cancel can reach a query until the returned [`InFlight`] is dropped.
    pub(super) fn request(
        &self,
        request: Request,
    ) -> Result<(InFlight<'_>, Receiver<Routed>), DbError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, answers) = mpsc::channel();
        let query = matches!(request, Request::Query { .. });
        let credit = query.then_some(WINDOW as i64);
        {
            let mut routing = lock(&self.shared.routing);
            if let Some(cause) = &routing.reset {
                return Err(reset_error(cause, false));
            }
            // A driver may quietly reconnect a lost session, which would run
            // the next statement without the Read-only hold the workspace's
            // own reconnect re-applies.
            if self.lost.load(Ordering::Relaxed) && !matches!(request, Request::Close) {
                let cause = format!("The {} plugin lost its session.", self.plugin.name);
                return Err(reset_error(&cause, false));
            }
            routing.pending.insert(id, Pending { sender, credit });
        }
        let in_flight = InFlight { host: self, id };
        // A Cancel reads `running` and only then queues its frame, so setting
        // it and queuing the request under one lock keeps the cancel behind
        // the request it targets, never ahead of it.
        let written = match query {
            true => {
                let mut running = lock(&self.running);
                *running = Some(id);
                self.write(id, &request)
            }
            false => self.write(id, &request),
        };
        if let Err(error) = written {
            lock(&self.shared.routing).pending.remove(&id);
            return Err(error);
        }
        Ok((in_flight, answers))
    }

    /// The request `id` has returned to its caller: a Cancel now has nothing
    /// to reach.
    fn done(&self, id: u64) {
        let mut running = lock(&self.running);
        if *running == Some(id) {
            *running = None;
        }
    }

    /// Cancels query `id` if it is the one running, and nothing otherwise. A
    /// plugin that has not answered it within [`CANCEL_DEADLINE`] is ended,
    /// which resets the connection: a Cancel that cannot land must still give
    /// the user their connection back. The deadline is met when the request's
    /// terminal frame arrives, not when its caller takes it, so a consumer
    /// that stopped taking rows never trips it.
    pub(super) fn cancel(&self, id: u64) {
        if *lock(&self.running) != Some(id) {
            return;
        }
        self.send(id, Request::Cancel);
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            thread::sleep(CANCEL_DEADLINE);
            if lock(&shared.routing).pending.contains_key(&id) {
                shared.end(format!(
                    "The {} plugin did not answer a cancel within {} s and was stopped.",
                    shared.name,
                    CANCEL_DEADLINE.as_secs()
                ));
            }
        });
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

    pub(super) fn session(&self) -> MutexGuard<'_, ()> {
        lock(&self.session)
    }

    pub(super) fn is_reset(&self) -> bool {
        lock(&self.shared.routing).reset.is_some()
    }

    pub(super) fn is_lost(&self) -> bool {
        self.is_reset() || self.lost.load(Ordering::Relaxed)
    }

    pub(super) fn lose_session(&self) {
        self.lost.store(true, Ordering::Relaxed);
    }

    /// Something the caller found in what the plugin sent, past what the
    /// reader checks: data that does not decode, an answer out of place. The
    /// plugin is ended and the reset error returned at once, worded for the
    /// violation rather than the kill.
    pub(super) fn violated(&self, violation: &str, running_query: bool) -> DbError {
        let cause = self
            .shared
            .end(broke_protocol(&self.shared.name, violation));
        reset_error(&cause, running_query)
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

    /// Ends the plugin without waiting for it; the watcher then sees it exit
    /// and reports the reset.
    pub(super) fn kill(&self) {
        self.shared.kill();
    }

    #[cfg(test)]
    pub(super) fn running(&self) -> Option<u64> {
        *lock(&self.running)
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self, id: u64) -> bool {
        lock(&self.shared.routing).pending.contains_key(&id)
    }

    #[cfg(test)]
    pub(super) fn peak(&self) -> i64 {
        lock(&self.shared.routing).peak
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> Option<u32> {
        lock(&self.shared.process)
            .as_ref()
            .map(|process| process.child.id())
    }
}

/// A request a Cancel can reach until this drops, unwinding included, so a
/// Cancel never lands on a request whose caller has gone.
pub(super) struct InFlight<'a> {
    host: &'a Host,
    pub(super) id: u64,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.host.done(self.id);
    }
}

/// `close` lets the plugin say goodbye to its server, and stdin stays open
/// until it answers or [`CLOSE_GRACE`] passes: a plugin exits on stdin EOF,
/// and one that read EOF straight after `close` would go before its driver
/// had disconnected or rolled back. Then EOF, and the kill for a plugin that
/// has not gone by the end of the grace.
impl Drop for Host {
    fn drop(&mut self) {
        // Registered so its `closed` answer is routed rather than read as a
        // frame for a request nobody sent.
        let closing = self
            .request(Request::Close)
            .ok()
            .map(|(_, answers)| answers);
        let Some(mut process) = lock(&self.shared.process).take() else {
            return;
        };
        let writer = self.writer.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + CLOSE_GRACE;
            if let Some(answers) = closing {
                while Instant::now() < deadline
                    && matches!(answers.recv_timeout(POLL), Err(RecvTimeoutError::Timeout))
                    && matches!(process.child.try_wait(), Ok(None))
                {}
            }
            drop(writer);
            process.reap(deadline.saturating_duration_since(Instant::now()));
        });
    }
}

/// How long a dead plugin's stderr may take to reach EOF before its error is
/// worded without the rest: a grandchild still holding the pipe keeps it open.
const STDERR_SETTLE: Duration = Duration::from_secs(1);

/// How long the reader has, once the plugin has exited, to route what it wrote
/// before it went. Past it a grandchild holding stdout is taken to be keeping
/// the pipe open, and the reset goes out without the rest.
const EXIT_SETTLE: Duration = Duration::from_millis(500);

const POLL: Duration = Duration::from_millis(10);

/// ponytail: the watcher polls; a wait on the process itself (`waitid` with
/// `WNOWAIT`, `WaitForSingleObject`) would wake only on the exit.
const WATCH: Duration = Duration::from_millis(50);

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
                if frame.payload.is_empty() {
                    return Err(format!("an empty data frame for request {id}"));
                }
                let size = credit_cost(frame.payload.len()) as i64;
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
                        "result data costing {size} with {credit} of credit left"
                    ));
                }
                *credit -= size;
                #[cfg(test)]
                let outstanding = WINDOW as i64 - *credit;
                let _ = pending.sender.send(Routed::Data(frame.payload));
                #[cfg(test)]
                {
                    routing.peak = routing.peak.max(outstanding);
                }
            }
        }
        Ok(())
    }

    /// Fails every pending request, with how the plugin exited unless it
    /// was ended for a cause of its own.
    fn reset(&self, status: Option<ExitStatus>) {
        let name = &self.name;
        let cause = match status {
            Some(status) => format!("The {name} plugin stopped ({status})."),
            None => format!("The {name} plugin stopped."),
        };
        let mut routing = lock(&self.routing);
        // A cause given when the plugin was ended on purpose says more than
        // the kill's exit status.
        let cause = routing.reset.get_or_insert(cause).clone();
        for (_, pending) in routing.pending.drain() {
            let _ = pending.sender.send(Routed::Reset(cause.clone()));
        }
    }

    /// Ends the plugin for `cause`, which the reset then reports; the cause
    /// already recorded, if one was.
    fn end(&self, cause: String) -> String {
        let cause = lock(&self.routing).reset.get_or_insert(cause).clone();
        self.kill();
        cause
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
            // A plugin that dies mid-write leaves half a frame: a crash, which
            // its exit status says more about than the frame does.
            Ok(None) | Err(Violation::Truncated) => break None,
            Err(violation) => break Some(violation.to_string()),
        }
    };
    if let Some(violation) = broke {
        shared.end(broke_protocol(&shared.name, &violation));
    }
    shared.read_done.store(true, Ordering::Release);
}

/// Reaps the plugin once it exits and only then fails what is pending, so a
/// pipe something else still holds open cannot leave a caller waiting. A
/// plugin that closed its stdout and lives on is given [`CLOSE_GRACE`].
fn watch(shared: &Shared) {
    let mut closed_at = None;
    let status = loop {
        thread::sleep(WATCH);
        let mut process = lock(&shared.process);
        // The handle's drop took it to reap: nobody is left to tell.
        let Some(running) = process.as_mut() else {
            return;
        };
        match running.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                process.take();
                break Some(status);
            }
            Err(_) => {
                break process
                    .take()
                    .and_then(|process| process.reap(Duration::ZERO));
            }
        }
        if shared.read_done.load(Ordering::Acquire)
            && closed_at.get_or_insert_with(Instant::now).elapsed() >= CLOSE_GRACE
        {
            let _ = running.child.kill();
        }
    };
    let settle = Instant::now() + EXIT_SETTLE;
    while !shared.read_done.load(Ordering::Acquire) && Instant::now() < settle {
        thread::sleep(POLL);
    }
    shared.reset(status);
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

fn broke_protocol(name: &str, violation: &str) -> String {
    format!("The {name} plugin broke protocol 1 ({violation}) and was stopped.")
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

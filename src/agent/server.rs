//! The app side of the socket: an accept thread, and one thread per
//! connection. No async runtime and nothing from the window: a connection
//! thread sends [`Inbound`] on a channel and blocks until the window answers.
//!
//! Every connection is admitted by its peer's uid before a byte is read, then
//! must open with one frame inside a time and a size. What does not is closed
//! and is neither a call nor a presence.

use std::{
    fs::File,
    io::{self, BufReader, Read, Write},
    net::Shutdown,
    os::unix::{fs::FileTypeExt, fs::MetadataExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use tokio::sync::mpsc::UnboundedSender;

use super::{CallArgs, CallId, ConnId, Declined, Inbound, Reply, Status, ipc, tools::Tool, wire};

// ponytail: a first frame is a few bytes written the moment the shim
// connects, so five seconds is patience, not a measurement; a slower peer is
// closed and tries again.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

// ponytail: one agent asks one thing at a time and a person answers each, so
// eight waiting at once is already more than anyone can read; the ninth is
// told so at once. Phase 6 may tune it.
const CALL_CAP: usize = 8;

// ponytail: one presence per attached shim, and an agent runs a handful of
// them; sixteen leaves room without letting a loop hold a thread each forever.
const PRESENCE_CAP: usize = 16;

// ponytail: a connection holds a thread from its accept to its first frame,
// at most `FIRST_FRAME_TIMEOUT`, before either cap counts it; this bounds how
// many do at once, with room for every call and presence to reconnect.
const PENDING_CAP: usize = 2 * (CALL_CAP + PRESENCE_CAP);

// ponytail: an answer is at most `wire::REPLY_LIMIT`, more than a socket
// buffer holds on macOS, so a caller that stops reading would park the writer
// and keep its call's place; a live shim reads it at once.
const REPLY_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

struct Shared {
    stop: AtomicBool,
    /// Numbers every connection. Calls are numbered from it too.
    next_id: AtomicU64,
    calls: AtomicUsize,
    presences: AtomicUsize,
    /// Connections accepted that have not yet sent their first frame.
    pending: AtomicUsize,
    first_frame_timeout: Duration,
    /// A handle to every connection that is not a call and has not ended, so
    /// stopping can wake the readers parked on them. A call's stream is never
    /// here: its own thread closes it after its answer, and closing it first
    /// would show the caller an end with no answer.
    open: Mutex<Vec<(ConnId, UnixStream)>>,
}

impl Shared {
    fn open(&self) -> std::sync::MutexGuard<'_, Vec<(ConnId, UnixStream)>> {
        // A thread that panicked holding it left a list that is still a list.
        self.open.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// An entry in [`Shared::open`], removed when it goes out of scope.
struct Registered<'a> {
    shared: &'a Shared,
    conn: ConnId,
}

impl<'a> Registered<'a> {
    fn new(shared: &'a Shared, conn: ConnId, stream: &UnixStream) -> Option<Self> {
        let handle = stream.try_clone().ok()?;
        shared.open().push((conn, handle));
        Some(Self { shared, conn })
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.shared.open().retain(|(conn, _)| *conn != self.conn);
    }
}

/// One of a counted kind of connection, released when it goes out of scope.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    /// A place among at most `cap`, or `None` when they are all taken.
    fn take(counter: &'a AtomicUsize, cap: usize) -> Option<Self> {
        let mut open = counter.load(Ordering::SeqCst);
        loop {
            if open >= cap {
                return None;
            }
            match counter.compare_exchange_weak(open, open + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Self(counter)),
                Err(now) => open = now,
            }
        }
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The listening socket. Dropping it stops the accept thread, wakes every
/// connection that is not waiting on an answer, and removes the socket file.
pub(crate) struct Listener {
    path: PathBuf,
    shared: Arc<Shared>,
    /// The serving lock. A field, so it is let go only after `drop` has run:
    /// the path is this window's own for as long as the removal takes.
    _lock: File,
}

/// Bind `path` and serve it, sending what arrives on `inbound`.
#[cfg(test)]
pub(crate) fn listen(path: &Path, inbound: UnboundedSender<Inbound>) -> Result<Listener, Declined> {
    listen_if(path, inbound, || true)
}

/// Bind `path` and serve it, sending what arrives on `inbound`, only if
/// `permitted` still holds once the serving lock is taken; see
/// [`ipc::bind_if`].
pub(crate) fn listen_if(
    path: &Path,
    inbound: UnboundedSender<Inbound>,
    permitted: impl FnOnce() -> bool,
) -> Result<Listener, Declined> {
    listen_with(path, inbound, FIRST_FRAME_TIMEOUT, permitted)
}

/// [`listen`] with the first-frame wait given, so a test need not sit through
/// the production one.
fn listen_with(
    path: &Path,
    inbound: UnboundedSender<Inbound>,
    first_frame_timeout: Duration,
    permitted: impl FnOnce() -> bool,
) -> Result<Listener, Declined> {
    let ipc::Bound { listener, lock } = ipc::bind_if(path, permitted)?;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        calls: AtomicUsize::new(0),
        presences: AtomicUsize::new(0),
        pending: AtomicUsize::new(0),
        first_frame_timeout,
        open: Mutex::new(Vec::new()),
    });
    let accepting = shared.clone();
    thread::Builder::new()
        .name("dbdelve-agent-accept".to_string())
        .spawn(move || accept(listener, accepting, inbound))
        .map_err(|error| {
            let _ = std::fs::remove_file(path);
            Declined::Failed(format!(
                "The agent socket's accept thread could not start: {error}."
            ))
        })?;
    Ok(Listener {
        path: path.to_path_buf(),
        shared,
        _lock: lock,
    })
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // A listener cannot be closed from another thread, and `accept` does
        // not return for a flag: one connection of our own wakes it, and it
        // sees the flag before it spawns anything for it. The lock is still
        // held, so no other window has bound the path this connects to.
        let _ = UnixStream::connect(&self.path);
        // The lock keeps every other window off the path, so a socket here is
        // this one's; a file that is not a socket of this user's is still left.
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.uid() == ipc::current_euid()
        {
            let _ = std::fs::remove_file(&self.path);
        }
        for (_, stream) in self.shared.open().iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn accept(listener: UnixListener, shared: Arc<Shared>, inbound: UnboundedSender<Inbound>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            // Out of descriptors, most likely: waiting is what lets one close.
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        // Over the cap the stream is dropped, which closes it; the place is
        // taken here, before a thread exists, and handed to it.
        let Some(place) = Counted::take(&shared.pending, PENDING_CAP) else {
            continue;
        };
        std::mem::forget(place);
        let serving = shared.clone();
        let inbound = inbound.clone();
        // A connection that cannot get a thread is dropped, which closes it.
        if thread::Builder::new()
            .name("dbdelve-agent-connection".to_string())
            .spawn(move || serve(stream, &serving, &inbound))
            .is_err()
        {
            shared.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// One connection, from its first frame to its end.
fn serve(stream: UnixStream, shared: &Shared, inbound: &UnboundedSender<Inbound>) {
    // The place `accept` took for this connection, given back once the first
    // frame is in or the connection ends without one.
    let pending = Counted(&shared.pending);
    if !ipc::admit(&stream, ipc::current_euid()) {
        return;
    }
    let conn = ConnId(shared.next_id.fetch_add(1, Ordering::SeqCst));
    let Some(registered) = Registered::new(shared, conn, &stream) else {
        return;
    };
    // Registered before the flag is read: a listener being dropped either
    // sweeps this stream or is seen here.
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    let mut reading = BufReader::new(Deadline {
        stream: &stream,
        until: Instant::now() + shared.first_frame_timeout,
    });
    let Ok(Some(line)) = wire::read_frame(&mut reading, wire::FRAME_LIMIT) else {
        return;
    };
    drop(reading);
    drop(pending);
    let _ = stream.set_read_timeout(None);
    match wire::decode_hello(&line) {
        Ok(wire::Hello::Call {
            tool,
            connection,
            sql,
            schema,
            table,
            analyze,
            query,
            routine,
            ..
        }) => {
            drop(registered);
            let args = CallArgs {
                sql,
                schema,
                table,
                analyze,
                query,
                routine,
            };
            call(
                &stream,
                CallId(conn.0),
                tool,
                connection,
                args,
                shared,
                inbound,
            );
        }
        Ok(wire::Hello::Presence { .. }) => presence(&stream, conn, shared, inbound),
        Err(_) => {}
    }
}

/// Reads a stream until a moment, not for a while per read: before each read
/// the stream's timeout is set to what is left, so a peer sending a byte at a
/// time is closed when the first frame's time is up, as a silent one is.
struct Deadline<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        // A zero timeout means none at all to `set_read_timeout`.
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the first frame did not arrive in time",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut reader = self.stream;
        reader.read(buf)
    }
}

fn call(
    stream: &UnixStream,
    id: CallId,
    tool: Tool,
    connection: Option<String>,
    args: CallArgs,
    shared: &Shared,
    inbound: &UnboundedSender<Inbound>,
) {
    let Some(_open) = Counted::take(&shared.calls, CALL_CAP) else {
        answer(
            stream,
            &Reply {
                status: Status::RefusedByGate,
                text: format!(
                    "Too many agent calls are waiting in DBDelve; the limit is {CALL_CAP}."
                ),
            },
        );
        return;
    };
    let (reply, answers) = mpsc::sync_channel(1);
    // Made before the call is sent and shared with the window, so a window
    // that has not yet heard the hangup can still know the caller left.
    let gone = Arc::new(AtomicBool::new(false));
    let call = Inbound::Call {
        id,
        tool,
        connection,
        args,
        reply,
        gone: gone.clone(),
    };
    if inbound.send(call).is_err() {
        return;
    }
    watch_for_hangup(stream, id, &gone, inbound);
    // A dropped sender is a call that ended without an answer: nothing is
    // written, and the stream closing is what the caller sees.
    if let Ok(reply) = answers.recv()
        && !deliver(stream, &reply, &gone, REPLY_WRITE_TIMEOUT)
    {
        let _ = inbound.send(Inbound::Undelivered { id });
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Tell the window when the caller goes, after setting `gone` for the writer.
/// The caller sends nothing after its first frame, so any result of a read, a
/// byte, an end or an error, means it has gone; and the thread's own end comes
/// when the call's stream is closed. A call this cannot watch still ends by
/// its expiry.
fn watch_for_hangup(
    stream: &UnixStream,
    id: CallId,
    gone: &Arc<AtomicBool>,
    inbound: &UnboundedSender<Inbound>,
) {
    let Ok(watching) = stream.try_clone() else {
        return;
    };
    let inbound = inbound.clone();
    let gone = gone.clone();
    let _ = thread::Builder::new()
        .name("dbdelve-agent-hangup".to_string())
        .spawn(move || {
            let mut reader = &watching;
            let _ = reader.read(&mut [0u8; 1]);
            // Before the window hears of it, so an answer the window gives in
            // between is known to have no one to go to.
            gone.store(true, Ordering::Release);
            let _ = inbound.send(Inbound::Hangup { id });
        });
}

/// Write the answer, unless the caller is known to have left. Answers whether
/// it reached a caller that was still there.
///
/// There are two ways to know. `gone` is what the hangup reader saw, and it
/// spares the write. The write itself is the other: a caller that left after
/// the flag was read has closed its end, and Rust ignores `SIGPIPE`, so the
/// write fails with `EPIPE` instead of ending the process.
///
/// The stream stays blocking and is never read here: the hangup reader holds a
/// handle on the same open file description, so changing how this one blocks
/// would change that read too, and it would return at once and read as a
/// hangup. Each send is made non-blocking by its own flag instead
/// ([`write_before`]); an answer that has not all gone within `within` is one
/// that did not arrive.
fn deliver(stream: &UnixStream, reply: &Reply, gone: &AtomicBool, within: Duration) -> bool {
    if gone.load(Ordering::Acquire) {
        return false;
    }
    write_before(
        stream,
        wire::encode(reply).as_bytes(),
        Instant::now() + within,
    )
}

/// Write all of `bytes` by `deadline`, whatever pace the caller reads at.
///
/// Not a send timeout: that bounds each wait for buffer space, and a caller
/// reading a little at a time ends every such wait, so one blocking write
/// could last for ever. Each send is one that cannot wait ([`send_now`]),
/// which leaves the descriptor's own mode, and so the hangup reader, alone,
/// and the only wait is a `poll` for room given what is left of the one
/// deadline.
fn write_before(stream: &UnixStream, bytes: &[u8], deadline: Instant) -> bool {
    use std::os::fd::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut rest = bytes;
    while !rest.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let mut room = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let millis = left.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: one valid `pollfd`, alive for the call.
        if unsafe { libc::poll(&mut room, 1, millis) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return false;
        }
        match usize::try_from(send_now(fd, rest)) {
            Ok(0) => return false,
            Ok(sent) => rest = &rest[sent..],
            // No room yet, or a signal: the deadline is checked again first.
            Err(_) => match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {}
                _ => return false,
            },
        }
    }
    true
}

/// Send what of `bytes` fits now, without waiting for room: `send`'s count,
/// with `EWOULDBLOCK` when nothing fits.
///
/// `MSG_DONTWAIT` says so on Linux. Apple's kernel ignores it on a send, and a
/// blocking one waits until every byte is in, so there the send carries no
/// more than the low-water mark: `poll` gives `POLLOUT` only once that much
/// room is free, so it goes in whole without a wait.
fn send_now(fd: std::os::fd::RawFd, bytes: &[u8]) -> isize {
    #[cfg(target_vendor = "apple")]
    let (len, flags) = (bytes.len().min(low_water_mark(fd)), 0);
    #[cfg(not(target_vendor = "apple"))]
    let (len, flags) = (bytes.len(), libc::MSG_DONTWAIT);
    // SAFETY: `bytes` is valid for `len <= bytes.len()` bytes for the call.
    unsafe { libc::send(fd, bytes.as_ptr().cast(), len, flags) }
}

/// The socket's send low-water mark: the room `poll` waits for before it
/// says the socket is writable. One byte when it cannot be read, which still
/// cannot wait.
#[cfg(target_vendor = "apple")]
fn low_water_mark(fd: std::os::fd::RawFd) -> usize {
    let mut mark: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `mark` and `len` are valid for writes, and `len` is `mark`'s size.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDLOWAT,
            (&raw mut mark).cast(),
            &mut len,
        )
    };
    if read == 0 {
        usize::try_from(mark).unwrap_or(1).max(1)
    } else {
        1
    }
}

/// A refusal written before the call was ever handed to the window, so there is
/// no window to tell if nobody reads it.
fn answer(stream: &UnixStream, reply: &Reply) {
    let mut writer = stream;
    // The caller may have hung up already, and then nobody reads this.
    let _ = writer.write_all(wire::encode(reply).as_bytes());
    let _ = stream.shutdown(Shutdown::Both);
}

/// Hold a presence open until the peer or the listener ends it.
fn presence(
    stream: &UnixStream,
    conn: ConnId,
    shared: &Shared,
    inbound: &UnboundedSender<Inbound>,
) {
    let Some(_open) = Counted::take(&shared.presences, PRESENCE_CAP) else {
        return;
    };
    if inbound.send(Inbound::PresenceOpened { conn }).is_err() {
        return;
    }
    // Anything the peer sends, an end, or an error: all mean it is done.
    let mut reader = stream;
    let _ = reader.read(&mut [0u8; 1]);
    let _ = inbound.send(Inbound::PresenceClosed { conn });
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, sync::mpsc::RecvTimeoutError};

    use super::*;
    use crate::agent::tests::{frame, short_socket_path};

    const QUIET: Duration = Duration::from_millis(300);

    /// A listener on a short path, with what it sends on drained by a plain
    /// thread: `blocking_recv` needs no runtime, so no window is involved.
    struct Rig {
        path: PathBuf,
        listener: Option<Listener>,
        inbound: mpsc::Receiver<Inbound>,
    }

    impl Rig {
        fn new(first_frame_timeout: Duration) -> Self {
            let path = short_socket_path();
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let Ok(listener) = listen_with(&path, sender, first_frame_timeout, || true) else {
                panic!("the socket binds");
            };
            let (forward, inbound) = mpsc::channel();
            thread::spawn(move || {
                while let Some(message) = receiver.blocking_recv() {
                    if forward.send(message).is_err() {
                        break;
                    }
                }
            });
            Self {
                path,
                listener: Some(listener),
                inbound,
            }
        }

        fn client(&self) -> UnixStream {
            let stream = UnixStream::connect(&self.path).expect("the socket takes a client");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("the timeout is set");
            stream
        }

        /// Everything the server wrote to `stream` before it closed it. A reset
        /// counts as a close: the server shut a stream with bytes still unread.
        fn closed_with(stream: &mut UnixStream) -> Vec<u8> {
            let mut bytes = Vec::new();
            match stream.read_to_end(&mut bytes) {
                Ok(_) => {}
                Err(error) => assert_eq!(error.kind(), ErrorKind::ConnectionReset),
            }
            bytes
        }

        fn nothing_arrives(&self) {
            assert!(matches!(
                self.inbound.recv_timeout(QUIET),
                Err(RecvTimeoutError::Timeout)
            ));
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.listener.take();
            let _ = std::fs::remove_dir_all(self.path.parent().expect("the path has a directory"));
        }
    }

    #[test]
    fn a_connection_that_sends_no_frame_is_neither_a_call_nor_a_presence() {
        let rig = Rig::new(Duration::from_millis(200));
        let mut idle = rig.client();
        assert!(Rig::closed_with(&mut idle).is_empty());
        for sent in ["\n", "not json\n"] {
            let mut client = rig.client();
            client
                .write_all(sent.as_bytes())
                .expect("the bytes are sent");
            assert!(Rig::closed_with(&mut client).is_empty(), "{sent:?}");
        }
        rig.nothing_arrives();
    }

    #[test]
    fn an_oversized_first_frame_closes_the_connection() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let mut client = rig.client();
        // The server may close before the last byte is written.
        let _ = client.write_all(&vec![b'a'; wire::FRAME_LIMIT as usize + 1]);
        assert!(Rig::closed_with(&mut client).is_empty());
        rig.nothing_arrives();
    }

    #[test]
    fn a_call_over_the_cap_is_refused_by_gate_naming_the_cap() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let call = frame(r#""kind":"call","tool":"list_connections""#);
        let call = call.as_bytes();
        let mut held = Vec::new();
        let mut unanswered = Vec::new();
        for _ in 0..CALL_CAP {
            let mut client = rig.client();
            client.write_all(call).expect("the frame is sent");
            client.write_all(b"\n").expect("the frame is ended");
            held.push(client);
            match rig.inbound.recv_timeout(Duration::from_secs(5)) {
                Ok(Inbound::Call { reply, .. }) => unanswered.push(reply),
                _ => panic!("a call within the cap reaches the window"),
            }
        }

        let mut over = rig.client();
        over.write_all(call).expect("the frame is sent");
        over.write_all(b"\n").expect("the frame is ended");
        assert_eq!(
            String::from_utf8(Rig::closed_with(&mut over)).expect("the answer is text"),
            "{\"status\":\"refused_by_gate\",\"text\":\"Too many agent calls are waiting in DBDelve; the limit is 8.\"}\n"
        );

        // A presence is not a call, and it is still taken; the refused call
        // sent nothing on, so the presence is the next thing to arrive.
        let mut presence = rig.client();
        presence
            .write_all(format!("{}\n", frame(r#""kind":"presence""#)).as_bytes())
            .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::PresenceOpened { .. })
        ));
        drop((held, unanswered));
    }

    #[test]
    fn a_presence_connection_opens_and_closes_without_counting_as_a_call() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let presence_line = format!("{}\n", frame(r#""kind":"presence""#));
        let presence_line = presence_line.as_bytes();
        let mut client = rig.client();
        client.write_all(presence_line).expect("the frame is sent");
        let Ok(Inbound::PresenceOpened { conn }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the presence opens");
        };
        drop(client);
        let Ok(Inbound::PresenceClosed { conn: closed }) =
            rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the presence closes");
        };
        assert_eq!(closed, conn);

        // More presences than the call cap leave every call a place.
        let mut presences = Vec::new();
        for _ in 0..=CALL_CAP {
            let mut presence = rig.client();
            presence
                .write_all(presence_line)
                .expect("the frame is sent");
            presences.push(presence);
            assert!(matches!(
                rig.inbound.recv_timeout(Duration::from_secs(5)),
                Ok(Inbound::PresenceOpened { .. })
            ));
        }
        let mut call = rig.client();
        call.write_all(
            format!("{}\n", frame(r#""kind":"call","tool":"list_connections""#)).as_bytes(),
        )
        .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::Call { .. })
        ));
    }

    #[test]
    fn an_answer_written_to_a_caller_that_left_reports_undelivered() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let mut client = rig.client();
        client
            .write_all(
                format!("{}\n", frame(r#""kind":"call","tool":"list_connections""#)).as_bytes(),
            )
            .expect("the frame is sent");
        let Ok(Inbound::Call { id, reply, .. }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the call reaches the window");
        };

        // Closed, not half-closed: the caller is gone. The reader sets the
        // flag before it sends the hangup, so once the window has heard it the
        // answer is known to have no one to go to, and none is written.
        drop(client);
        let Ok(Inbound::Hangup { id: hung_up }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the hangup reaches the window");
        };
        assert_eq!(hung_up, id);
        reply
            .send(Reply {
                status: Status::Executed,
                text: "late".to_string(),
            })
            .expect("the writer still waits for its answer");
        let Ok(Inbound::Undelivered { id: undelivered }) =
            rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the window is told the answer found no one");
        };
        assert_eq!(undelivered, id);
    }

    #[cfg(unix)]
    #[test]
    fn a_call_carries_the_flag_its_hangup_sets() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let client = rig.client();
        let mut writer = &client;
        writer
            .write_all(
                format!("{}\n", frame(r#""kind":"call","tool":"list_connections""#)).as_bytes(),
            )
            .expect("the frame is sent");
        // The reply is held: dropping it ends the call and closes the stream,
        // which would set the flag without the caller leaving.
        let Ok(Inbound::Call {
            id, gone, reply, ..
        }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the call reaches the window");
        };
        assert!(!gone.load(Ordering::Acquire), "the caller is still there");

        drop(client);
        let Ok(Inbound::Hangup { id: hung_up }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the hangup reaches the window");
        };
        assert_eq!(hung_up, id);
        assert!(
            gone.load(Ordering::Acquire),
            "the window's own copy of the flag says the caller left"
        );
        drop(reply);
    }

    #[test]
    fn an_answer_to_a_closed_peer_is_not_delivered() {
        let reply = Reply {
            status: Status::Executed,
            text: "answer".to_string(),
        };
        let frame = wire::encode(&reply);

        // Open peer, flag clear: written, and the peer reads the frame.
        let (server, mut peer) = UnixStream::pair().expect("a pair of sockets");
        let gone = AtomicBool::new(false);
        assert!(deliver(&server, &reply, &gone, REPLY_WRITE_TIMEOUT));
        let mut read = vec![0u8; frame.len()];
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the timeout is set");
        peer.read_exact(&mut read)
            .expect("the peer reads the frame");
        assert_eq!(read, frame.as_bytes());

        // Peer gone, flag clear: the write itself fails.
        drop(peer);
        assert!(!deliver(&server, &reply, &gone, REPLY_WRITE_TIMEOUT));

        // Flag set, peer open: nothing is written.
        let (server, mut peer) = UnixStream::pair().expect("a pair of sockets");
        let gone = AtomicBool::new(true);
        assert!(!deliver(&server, &reply, &gone, REPLY_WRITE_TIMEOUT));
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("the timeout is set");
        let silent = peer.read(&mut [0u8; 1]).expect_err("nothing was written");
        assert!(matches!(
            silent.kind(),
            ErrorKind::WouldBlock | ErrorKind::TimedOut
        ));
    }

    #[test]
    fn an_answer_the_caller_does_not_read_gives_up_at_its_deadline() {
        // Far more than any socket buffer holds, so the write must wait.
        let reply = Reply {
            status: Status::Executed,
            text: "a".repeat(8 * 1024 * 1024),
        };
        let (server, _silent) = UnixStream::pair().expect("a pair of sockets");
        let delivered = ended_within(Duration::from_secs(5), move || {
            deliver(
                &server,
                &reply,
                &AtomicBool::new(false),
                Duration::from_millis(200),
            )
        });
        assert!(!delivered);
    }

    /// What `work` gave, so long as it ended within `limit`. A write that
    /// waits on the caller would otherwise hold the test, and the suite, for
    /// as long as the caller does.
    fn ended_within<T: Send + 'static>(
        limit: Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (done, ended) = std::sync::mpsc::channel();
        thread::spawn(move || _ = done.send(work()));
        ended
            .recv_timeout(limit)
            .unwrap_or_else(|_| panic!("it was still waiting after {limit:?}"))
    }

    #[test]
    fn an_answer_read_a_little_at_a_time_still_gives_up_at_its_deadline() {
        let (server, mut trickle) = UnixStream::pair().expect("a pair of sockets");
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            // Every write makes some progress, well inside any one timeout.
            while trickle.read(&mut chunk).is_ok_and(|read| read > 0) {
                thread::sleep(Duration::from_millis(5));
            }
        });
        let written = ended_within(Duration::from_secs(3), move || {
            write_before(
                &server,
                &vec![b'a'; 64 * 1024 * 1024],
                Instant::now() + Duration::from_millis(300),
            )
        });
        assert!(!written);
    }

    #[test]
    fn connections_waiting_for_a_first_frame_are_capped() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let silent: Vec<_> = (0..PENDING_CAP).map(|_| rig.client()).collect();
        // Accepted after every silent one, so it finds the places taken and
        // is closed at once, long before the first-frame wait.
        let started = Instant::now();
        let mut over = rig.client();
        assert!(Rig::closed_with(&mut over).is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));

        // A place comes back when a waiting connection ends.
        drop(silent);
        let deadline = Instant::now() + Duration::from_secs(5);
        let presence_line = format!("{}\n", frame(r#""kind":"presence""#));
        loop {
            let mut client = rig.client();
            let _ = client.write_all(presence_line.as_bytes());
            if let Ok(Inbound::PresenceOpened { .. }) =
                rig.inbound.recv_timeout(Duration::from_millis(200))
            {
                break;
            }
            assert!(Instant::now() < deadline, "a place was never given back");
        }
    }

    #[test]
    fn a_first_frame_sent_a_byte_at_a_time_is_closed_when_its_time_is_up() {
        let rig = Rig::new(Duration::from_millis(300));
        let mut slow = rig.client();
        let started = Instant::now();
        // Each byte comes well inside the limit; the frame as a whole does not.
        for byte in frame(r#""kind":"presence""#).as_bytes() {
            if slow.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(Rig::closed_with(&mut slow).is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
        rig.nothing_arrives();
    }

    #[test]
    fn stopping_wakes_the_listener_and_removes_the_socket() {
        let mut rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let mut presence = rig.client();
        presence
            .write_all(format!("{}\n", frame(r#""kind":"presence""#)).as_bytes())
            .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::PresenceOpened { .. })
        ));

        let stopped = std::time::Instant::now();
        rig.listener.take();
        // The channel closes once the accept thread and every connection
        // thread have let go of it.
        loop {
            match rig.inbound.recv_timeout(Duration::from_secs(1)) {
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("the listener did not stop in time"),
            }
        }
        assert!(stopped.elapsed() < Duration::from_secs(1));
        assert!(!rig.path.exists());
        assert!(Rig::closed_with(&mut presence).is_empty());
    }
}

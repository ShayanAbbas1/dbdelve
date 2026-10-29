//! A stand-in for ClickHouse's HTTP interface, so the connection is tested in
//! a plain `cargo test` with no server and no Docker.
//!
//! Modelled on `snowflake/mock.rs`: one request per connection, answered and
//! closed, with the requests seen kept for asserts. Responses are scripted
//! per statement. On top of that it keeps the three pieces of server state the
//! driver's behaviour depends on: a registry of running queries (what a `KILL`
//! finds, and when), sessions (expiry, locking, the timeout ceiling), and a
//! read-only user who may not change named settings.
//!
//! A test needs no `http://` host: the profile is `sslmode=disable` on
//! [`Mock::server`]'s port.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::{ServerConfig, SslMode};

/// What the mock says it is, in the version suffix of its errors.
const VERSION: &str = "26.8.1.1";

/// How often a response waiting out a delay looks for a `KILL`.
const POLL: Duration = Duration::from_millis(10);

/// How much of a body goes out before the connection is closed.
#[derive(Clone, Debug)]
enum Delivery {
    Whole,
    /// All of it, after a pause: a server slow to answer.
    Delayed(Duration),
    Truncated(usize),
    Stalled(usize, Duration),
    /// No byte at all: the connection is closed once the request is read.
    Dropped,
    /// All of it, once the test calls [`Mock::release`] with this name.
    Held(String),
}

/// What a response does when a `KILL` for its query arrives while it waits.
#[derive(Clone, Copy, Debug)]
pub enum Aftermath {
    /// Ends with `QUERY_WAS_CANCELLED` (394), the way a killed query's body
    /// does.
    Cancelled,
    /// The connection just closes.
    Cut,
}

/// One response. A route answers with its responses in turn and repeats the
/// last.
#[derive(Clone, Debug)]
pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Appended after the body, as a server that fails once it has begun to
    /// answer does: the tagged frame, or plain text on an untagged mock.
    late_exception: Option<String>,
    delivery: Delivery,
    chunked: bool,
    on_kill: Option<Aftermath>,
}

impl Response {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            late_exception: None,
            delivery: Delivery::Whole,
            chunked: false,
            on_kill: None,
        }
    }

    /// A result: names, types and rows, each field already in TSV's spelling.
    pub fn rows(columns: &[(&str, &str)], rows: &[&[&str]]) -> Self {
        let line = |fields: &mut dyn Iterator<Item = &str>| {
            let mut line = fields.collect::<Vec<_>>().join("\t");
            line.push('\n');
            line
        };
        let mut body = line(&mut columns.iter().map(|(name, _)| *name));
        body.push_str(&line(&mut columns.iter().map(|(_, kind)| *kind)));
        for row in rows {
            body.push_str(&line(&mut row.iter().copied()));
        }
        let mut response = Self::new(200, body.into_bytes());
        response.headers.push((
            "X-ClickHouse-Format".into(),
            "TabSeparatedWithNamesAndTypes".into(),
        ));
        response
    }

    /// A body written out exactly.
    pub fn body(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, body.into())
    }

    /// What a command answers: success and nothing to say.
    pub fn empty() -> Self {
        Self::new(200, Vec::new())
    }

    /// A body that is not the server's, such as a proxy's bare 503.
    pub fn text(status: u16, body: &str) -> Self {
        Self::new(status, body.as_bytes().to_vec())
    }

    /// An error before any body: the status, the code header, and the server's
    /// text.
    pub fn error(status: u16, code: u32, name: &str, message: &str) -> Self {
        let mut response = Self::new(status, exception_text(code, name, message).into_bytes());
        response
            .headers
            .push(("X-ClickHouse-Exception-Code".into(), code.to_string()));
        response
    }

    pub fn status(self, status: u16) -> Self {
        Self { status, ..self }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// `X-ClickHouse-Summary`, a JSON object of strings.
    pub fn summary(self, json: &str) -> Self {
        self.header("X-ClickHouse-Summary", json)
    }

    /// The format the server says it used (`X-ClickHouse-Format`).
    pub fn format(mut self, format: &str) -> Self {
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("X-ClickHouse-Format"));
        self.header("X-ClickHouse-Format", format)
    }

    /// The code header on a response that is otherwise a 200.
    pub fn code(self, code: u32) -> Self {
        self.header("X-ClickHouse-Exception-Code", &code.to_string())
    }

    /// The server fails after it began to answer.
    pub fn then_fails(self, code: u32, name: &str, message: &str) -> Self {
        Self {
            late_exception: Some(exception_text(code, name, message)),
            ..self
        }
    }

    /// Sent as a chunked body, as a streamed result is.
    pub fn chunked(self) -> Self {
        Self {
            chunked: true,
            ..self
        }
    }

    /// The whole length promised and only `sent` bytes of it delivered.
    pub fn truncated(self, sent: usize) -> Self {
        Self {
            delivery: Delivery::Truncated(sent),
            ..self
        }
    }

    /// Nothing for `pause`, then the whole response.
    pub fn delayed(self, pause: Duration) -> Self {
        Self {
            delivery: Delivery::Delayed(pause),
            ..self
        }
    }

    /// Nothing until the test calls [`Mock::release`] with `gate`, then the
    /// whole response: a statement the test lets finish when it is ready.
    pub fn held(self, gate: &str) -> Self {
        Self {
            delivery: Delivery::Held(gate.into()),
            ..self
        }
    }

    /// The request read and its query registered, then the connection closed
    /// before any answer, as a proxy that lost the server after forwarding
    /// the request does.
    pub fn dropped() -> Self {
        Self {
            delivery: Delivery::Dropped,
            ..Self::empty()
        }
    }

    /// `sent` bytes delivered, then nothing for `pause`, then the connection
    /// closed without the rest.
    pub fn stalled(self, sent: usize, pause: Duration) -> Self {
        Self {
            delivery: Delivery::Stalled(sent, pause),
            ..self
        }
    }

    /// Ends early, in `aftermath`'s way, when a `KILL` for the request's
    /// `query_id` lands during a delay or a stall. Chunked, because a body
    /// that promised a length cannot grow an error.
    pub fn killed_by(self, aftermath: Aftermath) -> Self {
        Self {
            on_kill: Some(aftermath),
            chunked: true,
            ..self
        }
    }
}

fn exception_text(code: u32, name: &str, message: &str) -> String {
    format!("Code: {code}. DB::Exception: {message} ({name}) (version {VERSION} (official build))")
}

/// A request as the mock received it.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The URL's parameters, decoded.
    pub params: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    /// The body, which is the statement byte for byte.
    pub statement: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn query_id(&self) -> Option<&str> {
        self.param("query_id")
    }
}

/// When a running query becomes something a `KILL` can find, which is the
/// gap the driver's retry loop exists for.
#[derive(Clone, Copy, Debug, Default)]
pub enum Registration {
    #[default]
    Immediately,
    /// This long after the request arrived.
    After(Duration),
    /// Only when the test calls [`Mock::register`].
    Signal,
    /// At the `n`th `KILL` for it, or the first after the request arrived,
    /// whichever is later. Counted by the mock, so no test thread has to win a
    /// race to register it in time.
    OnKill(usize),
}

struct Route {
    /// Matched against the statement by substring; empty matches anything.
    statement: String,
    responses: VecDeque<Response>,
}

struct State {
    routes: Vec<Route>,
    requests: Vec<Request>,
    tagged: bool,
    tags_issued: u64,
    registration: Registration,
    /// Query ids a `KILL` finds, from the moment given.
    registered: HashMap<String, Instant>,
    killed: Vec<String>,
    kill_status: String,
    /// Sessions that exist, and whether a request is running in them.
    sessions: HashMap<String, bool>,
    max_session_timeout: u32,
    /// Settings this user may not change.
    readonly: HashSet<String>,
    /// A body longer than this is never read.
    swallow_over: Option<usize>,
    swallowed: usize,
    /// The gates [`Mock::release`] opened.
    released: HashSet<String>,
}

pub struct Mock {
    port: u16,
    state: Arc<Mutex<State>>,
}

impl Mock {
    /// Listening on a free loopback port, tagging its responses as a server
    /// from 25.11 does.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("bound").port();
        let state = Arc::new(Mutex::new(State {
            routes: Vec::new(),
            requests: Vec::new(),
            tagged: true,
            tags_issued: 0,
            registration: Registration::default(),
            registered: HashMap::new(),
            killed: Vec::new(),
            kill_status: "finished".into(),
            sessions: HashMap::new(),
            max_session_timeout: 3600,
            readonly: HashSet::new(),
            swallow_over: None,
            swallowed: 0,
            released: HashSet::new(),
        }));
        let shared = Arc::clone(&state);
        // ponytail: never joined. The listener lives until the test binary
        // exits, which is sooner than it could matter.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self { port, state }
    }

    /// A profile pointed at this mock.
    pub fn server(&self) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".into(),
            port: Some(self.port),
            user: "default".into(),
            sslmode: SslMode::Disable,
            ..Default::default()
        }
    }

    /// Answer a statement containing `statement` with `responses` in turn. The
    /// newest route that matches wins.
    pub fn on_statement(&self, statement: &str, responses: impl IntoIterator<Item = Response>) {
        let responses: VecDeque<_> = responses.into_iter().collect();
        assert!(!responses.is_empty(), "a route needs a response");
        self.state().routes.push(Route {
            statement: statement.into(),
            responses,
        });
    }

    /// Whether responses carry the exception tag and late errors the tagged
    /// frame (a server from 25.11), or neither (older).
    pub fn tagged(&self, tagged: bool) {
        self.state().tagged = tagged;
    }

    pub fn registration(&self, registration: Registration) {
        self.state().registration = registration;
    }

    /// The server has registered `query_id`, so a `KILL` finds it from now.
    pub fn register(&self, query_id: &str) {
        self.state()
            .registered
            .insert(query_id.into(), Instant::now());
    }

    /// What a found query's `kill_status` says: `finished` unless set.
    pub fn kill_status(&self, status: &str) {
        self.state().kill_status = status.into();
    }

    /// The query ids a `KILL` found, in order.
    pub fn killed(&self) -> Vec<String> {
        self.state().killed.clone()
    }

    /// The server forgets `session`: with `session_check=1` its next request
    /// is `SESSION_NOT_FOUND`, without it a new session is quietly made.
    pub fn expire_session(&self, session: &str) {
        self.state().sessions.remove(session);
    }

    pub fn expire_sessions(&self) {
        self.state().sessions.clear();
    }

    /// `max_session_timeout`: a `session_timeout` above it fails creating the
    /// session with `INVALID_SESSION_TIMEOUT`.
    pub fn max_session_timeout(&self, seconds: u32) {
        self.state().max_session_timeout = seconds;
    }

    /// The user is read-only for these settings: sending one as a parameter
    /// is `READONLY`.
    pub fn readonly(&self, settings: &[&str]) {
        self.state()
            .readonly
            .extend(settings.iter().map(|name| name.to_string()));
    }

    /// A request whose body is longer than `bytes` is left unread and its
    /// connection open, as a server or a proxy that stopped reading does. It
    /// is never seen, registered or answered.
    pub fn swallow_bodies_over(&self, bytes: usize) {
        self.state().swallow_over = Some(bytes);
    }

    /// How many requests [`Self::swallow_bodies_over`] has swallowed.
    pub fn swallowed(&self) -> usize {
        self.state().swallowed
    }

    /// Lets every response [`Response::held`] at `gate` go out.
    pub fn release(&self, gate: &str) {
        self.state().released.insert(gate.into());
    }

    /// Every request so far, in the order they arrived.
    pub fn requests(&self) -> Vec<Request> {
        self.state().requests.clone()
    }

    /// How many requests carried a statement containing `statement`.
    pub fn hits(&self, statement: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.statement.contains(statement))
            .count()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What serving a request decided, before any byte is written.
struct Decision {
    response: Response,
    tag: Option<String>,
    /// Held until execution ends, before a complete response is observable.
    session: Option<String>,
    query_id: Option<String>,
}

fn decide(request: &Request, state: &mut State) -> Decision {
    let query_id = request.query_id().map(str::to_string);
    state.tags_issued += 1;
    let tag = state.tagged.then(|| {
        format!(
            "{:016x}",
            state.tags_issued.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        )
    });
    let mut decision = Decision {
        response: Response::empty(),
        tag,
        session: None,
        query_id: query_id.clone(),
    };
    let refuse = |code, name: &str, message: String| Response::error(500, code, name, &message);

    if let Some(session) = request.param("session_id") {
        let checked = request.param("session_check") == Some("1");
        match state.sessions.get(session) {
            Some(true) => {
                decision.response = refuse(
                    373,
                    "SESSION_IS_LOCKED",
                    format!("Session {session} is locked by a concurrent client."),
                );
                return decision;
            }
            None if checked => {
                decision.response = refuse(
                    372,
                    "SESSION_NOT_FOUND",
                    format!("Session {session} not found."),
                );
                return decision;
            }
            None => {
                let timeout = request.param("session_timeout");
                let valid = timeout.is_none_or(|timeout| {
                    timeout
                        .parse::<u32>()
                        .is_ok_and(|seconds| seconds <= state.max_session_timeout)
                });
                if !valid {
                    decision.response = refuse(
                        374,
                        "INVALID_SESSION_TIMEOUT",
                        format!(
                            "Invalid session timeout: '{}', maximum: '{}'.",
                            timeout.unwrap_or_default(),
                            state.max_session_timeout
                        ),
                    );
                    return decision;
                }
            }
            Some(false) => {}
        }
        state.sessions.insert(session.to_string(), true);
        decision.session = Some(session.to_string());
    }

    for (name, _) in &request.params {
        if state.readonly.contains(name) {
            decision.response = refuse(
                164,
                "READONLY",
                format!("Cannot modify '{name}' setting in readonly mode."),
            );
            return decision;
        }
    }

    if let Some(id) = &query_id {
        let effective = match state.registration {
            Registration::Immediately => Some(Instant::now()),
            Registration::After(delay) => Some(Instant::now() + delay),
            Registration::Signal | Registration::OnKill(_) => None,
        };
        if let Some(effective) = effective {
            state.registered.entry(id.clone()).or_insert(effective);
        }
    }

    if let Some(target) = kill_target(&request.statement) {
        if let Registration::OnKill(nth) = state.registration {
            let arrived = state.requests.iter().any(|sent| {
                sent.query_id() == Some(&target) && kill_target(&sent.statement).is_none()
            });
            let kills = state
                .requests
                .iter()
                .filter(|sent| kill_target(&sent.statement).as_deref() == Some(&target))
                .count();
            if arrived && kills >= nth {
                state
                    .registered
                    .entry(target.clone())
                    .or_insert_with(Instant::now);
            }
        }
        let found = state
            .registered
            .get(&target)
            .is_some_and(|since| *since <= Instant::now());
        decision.response = match found {
            true => {
                state.killed.push(target.clone());
                Response::rows(
                    &[
                        ("kill_status", "String"),
                        ("query_id", "String"),
                        ("user", "String"),
                        ("query", "String"),
                    ],
                    &[&[&state.kill_status, &target, "default", "SELECT 1"]],
                )
            }
            false => Response::empty(),
        };
        return decision;
    }

    let route = state
        .routes
        .iter_mut()
        .rev()
        .find(|route| request.statement.contains(&route.statement));
    decision.response = match route {
        Some(route) if route.responses.len() == 1 => route.responses[0].clone(),
        Some(route) => route.responses.pop_front().expect("not empty"),
        // Loud rather than a hang: the message is what the failing test shows.
        None => Response::text(404, &format!("mock: no route for {:?}", request.statement)),
    };
    decision
}

/// The id a `KILL QUERY WHERE query_id = '<id>'` names.
fn kill_target(statement: &str) -> Option<String> {
    let upper = statement.trim_start().to_ascii_uppercase();
    if !upper.starts_with("KILL QUERY") {
        return None;
    }
    let (_, rest) = statement.split_once("query_id = '")?;
    Some(rest.split_once('\'')?.0.to_string())
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Some(request) = read_request(&stream, state) else {
        return;
    };
    let mut decision = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requests.push(request.clone());
        decide(&request, &mut state)
    };
    let _ = write_response(stream, &mut decision, state);
    release_session(&mut decision.session, state);
}

fn release_session(session: &mut Option<String>, state: &Mutex<State>) {
    if let Some(session) = session.take() {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = state.sessions.get_mut(&session) {
            *held = false;
        }
    }
}

fn read_request(stream: &TcpStream, state: &Mutex<State>) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params = url::form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((name.to_string(), value.trim().to_string()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let swallowed = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let over = state.swallow_over.is_some_and(|over| length > over);
        state.swallowed += usize::from(over);
        over
    };
    if swallowed {
        std::thread::sleep(Duration::from_secs(60));
        return None;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;

    Some(Request {
        method,
        path: path.to_string(),
        params,
        headers,
        statement: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Waits `pause`, or until a `KILL` for `query_id` lands and the response
/// reacts to one. Gives what the response does about it.
fn wait(
    pause: Duration,
    response: &Response,
    query_id: Option<&str>,
    state: &Mutex<State>,
) -> Option<Aftermath> {
    let until = Instant::now() + pause;
    while Instant::now() < until {
        std::thread::sleep(POLL.min(until - Instant::now()));
        if let (Some(aftermath), Some(id)) = (response.on_kill, query_id) {
            let killed = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .killed
                .iter()
                .any(|killed| killed == id);
            if killed {
                return Some(aftermath);
            }
        }
    }
    None
}

/// What goes after the body when the server fails late.
fn late_exception(message: &str, tag: Option<&str>) -> Vec<u8> {
    match tag {
        Some(tag) => format!(
            "\r\n__exception__\r\n{tag}\r\n{message}\r\n{} {tag}\r\n__exception__\r\n",
            message.len()
        )
        .into_bytes(),
        None => format!("{message}\n").into_bytes(),
    }
}

fn write_response(
    mut stream: TcpStream,
    decision: &mut Decision,
    state: &Mutex<State>,
) -> std::io::Result<()> {
    let response = &decision.response;
    let query_id = decision.query_id.as_deref();
    let tag = decision.tag.as_deref();
    let mut aftermath = None;
    if let Delivery::Dropped = response.delivery {
        release_session(&mut decision.session, state);
        return stream.shutdown(Shutdown::Both);
    }
    if let Delivery::Delayed(pause) = response.delivery {
        aftermath = wait(pause, response, query_id, state);
    }
    if let Delivery::Held(gate) = &response.delivery {
        while !state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .released
            .contains(gate)
        {
            std::thread::sleep(POLL);
        }
    }

    let mut body = response.body.clone();
    if let Some(message) = &response.late_exception {
        body.extend(late_exception(message, tag));
    }
    let mut head = format!("HTTP/1.1 {} Mock\r\nConnection: close\r\n", response.status);
    if let Some(tag) = tag {
        head.push_str(&format!("X-ClickHouse-Exception-Tag: {tag}\r\n"));
    }
    if let Some(id) = query_id {
        head.push_str(&format!("X-ClickHouse-Query-Id: {id}\r\n"));
    }
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match response.chunked {
        true => head.push_str("Transfer-Encoding: chunked\r\n"),
        false => head.push_str(&format!("Content-Length: {}\r\n", body.len())),
    }
    head.push_str("\r\n");
    // A client can send its next request as soon as Content-Length bytes
    // arrive (just the headers for an empty body), before this thread runs
    // again. A completed execution must already have released its session.
    let complete = match response.delivery {
        Delivery::Stalled(sent, _) => !response.chunked && sent >= body.len(),
        _ => true,
    };
    if complete || aftermath.is_some() {
        release_session(&mut decision.session, state);
    }
    stream.write_all(head.as_bytes())?;

    let write = |stream: &mut TcpStream, bytes: &[u8]| -> std::io::Result<()> {
        match response.chunked {
            true if bytes.is_empty() => Ok(()),
            true => {
                stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
                stream.write_all(bytes)?;
                stream.write_all(b"\r\n")
            }
            false => stream.write_all(bytes),
        }
    };
    let finish = |stream: &mut TcpStream| -> std::io::Result<()> {
        if response.chunked {
            stream.write_all(b"0\r\n\r\n")?;
        }
        stream.flush()?;
        stream.shutdown(Shutdown::Both)
    };

    if let Some(aftermath) = aftermath {
        return end_killed(stream, aftermath, tag, write, finish);
    }
    match response.delivery {
        Delivery::Whole | Delivery::Delayed(_) | Delivery::Held(_) | Delivery::Dropped => {
            write(&mut stream, &body)?;
            finish(&mut stream)
        }
        Delivery::Truncated(sent) => {
            write(&mut stream, &body[..sent.min(body.len())])?;
            stream.flush()?;
            stream.shutdown(Shutdown::Both)
        }
        Delivery::Stalled(sent, pause) => {
            write(&mut stream, &body[..sent.min(body.len())])?;
            stream.flush()?;
            if let Some(aftermath) = wait(pause, response, query_id, state) {
                release_session(&mut decision.session, state);
                return end_killed(stream, aftermath, tag, write, finish);
            }
            release_session(&mut decision.session, state);
            stream.shutdown(Shutdown::Both)
        }
    }
}

fn end_killed(
    mut stream: TcpStream,
    aftermath: Aftermath,
    tag: Option<&str>,
    write: impl Fn(&mut TcpStream, &[u8]) -> std::io::Result<()>,
    finish: impl Fn(&mut TcpStream) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match aftermath {
        Aftermath::Cancelled => {
            let message = exception_text(394, "QUERY_WAS_CANCELLED", "Query was cancelled.");
            write(&mut stream, &late_exception(&message, tag))?;
            finish(&mut stream)
        }
        Aftermath::Cut => {
            stream.flush()?;
            stream.shutdown(Shutdown::Both)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::clickhouse::http::{Bounds, Scheme, agent};
    use crate::db::clickhouse::tsv::{Framed, Reader, is_cut};
    use std::io::BufReader;
    use ureq::Agent;

    fn client(mock: &Mock) -> Agent {
        agent(&mock.server(), None, Scheme::Http, Bounds::user(0)).unwrap()
    }

    fn post(mock: &Mock, query: &str, statement: &str) -> ureq::http::Response<ureq::Body> {
        client(mock)
            .post(&format!("http://127.0.0.1:{}/?{query}", mock.port))
            .send(statement)
            .unwrap()
    }

    fn text(mut response: ureq::http::Response<ureq::Body>) -> String {
        response.body_mut().read_to_string().unwrap()
    }

    fn failure(mock: &Mock, query: &str) -> (u16, String) {
        let mut response = post(mock, query, "SELECT 1");
        let status = response.status().as_u16();
        (status, response.body_mut().read_to_string().unwrap())
    }

    #[test]
    fn a_scripted_result_is_served_and_the_request_kept() {
        let mock = Mock::start();
        mock.on_statement(
            "SELECT 1",
            [Response::rows(&[("1", "UInt8")], &[&["1"]]).summary(r#"{"read_rows":"1"}"#)],
        );
        let response = post(&mock, "query_id=q1&default_format=TSV", "SELECT 1;");
        assert_eq!(
            response.headers().get("X-ClickHouse-Summary").unwrap(),
            r#"{"read_rows":"1"}"#
        );
        assert!(
            response
                .headers()
                .contains_key("X-ClickHouse-Exception-Tag")
        );
        assert_eq!(text(response), "1\nUInt8\n1\n");
        let seen = &mock.requests()[0];
        assert_eq!(seen.statement, "SELECT 1;");
        assert_eq!(seen.query_id(), Some("q1"));
        assert_eq!(seen.param("default_format"), Some("TSV"));
        assert_eq!(mock.hits("SELECT"), 1);
    }

    #[test]
    fn a_route_repeats_its_last_response() {
        let mock = Mock::start();
        mock.on_statement("x", [Response::body("a"), Response::body("b")]);
        let got: Vec<_> = (0..3).map(|_| text(post(&mock, "", "x"))).collect();
        assert_eq!(got, ["a", "b", "b"]);
    }

    #[test]
    fn an_unscripted_statement_is_a_loud_404() {
        let mock = Mock::start();
        assert_eq!(failure(&mock, "").0, 404);
    }

    #[test]
    fn a_late_error_is_a_tagged_frame_the_reader_fails_on() {
        let mock = Mock::start();
        mock.on_statement(
            "s",
            [Response::rows(&[("n", "UInt8")], &[&["1"], &["2"]])
                .chunked()
                .then_fails(395, "FUNCTION_THROW_IF_VALUE_IS_NON_ZERO", "boom")],
        );
        let mut response = post(&mock, "", "s");
        let tag = response
            .headers()
            .get("X-ClickHouse-Exception-Tag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let framed = Framed::tagged(response.body_mut().as_reader(), &tag, "s".into());
        let mut reader = Reader::open(BufReader::new(framed), None).unwrap();
        let error = reader.drain().unwrap_err();
        assert!(error.message.starts_with("boom"), "{error}");
    }

    #[test]
    fn an_untagged_mock_appends_the_plain_text_instead() {
        let mock = Mock::start();
        mock.tagged(false);
        mock.on_statement(
            "s",
            [Response::rows(&[("n", "UInt8")], &[&["1"]]).then_fails(395, "X", "boom")],
        );
        let response = post(&mock, "", "s");
        assert!(
            !response
                .headers()
                .contains_key("X-ClickHouse-Exception-Tag")
        );
        let body = text(response);
        assert!(body.starts_with("n\nUInt8\n1\nCode: 395. DB::Exception: boom (X)"));
    }

    #[test]
    fn a_truncated_body_is_a_cut_not_a_short_result() {
        let mock = Mock::start();
        mock.on_statement(
            "s",
            [Response::rows(&[("n", "UInt8")], &[&["1"], &["2"], &["3"]]).truncated(12)],
        );
        let mut response = post(&mock, "", "s");
        let mut reader = Reader::open(
            BufReader::new(Framed::plain(response.body_mut().as_reader())),
            None,
        );
        let error = reader
            .as_mut()
            .map_or_else(|error| error.clone(), |reader| reader.drain().unwrap_err());
        assert!(is_cut(&error), "{error}");
    }

    #[test]
    fn a_chunked_body_cut_before_its_last_chunk_is_an_error_too() {
        let mock = Mock::start();
        mock.on_statement(
            "s",
            [Response::rows(&[("n", "UInt8")], &[&["1"], &["2"]])
                .chunked()
                .truncated(10)],
        );
        let mut response = post(&mock, "", "s");
        assert!(response.body_mut().read_to_string().is_err());
    }

    #[test]
    fn a_stalled_response_is_slow_to_end() {
        let mock = Mock::start();
        let pause = Duration::from_millis(200);
        mock.on_statement("s", [Response::body("0123456789").stalled(4, pause)]);
        let started = Instant::now();
        let mut response = post(&mock, "", "s");
        assert!(response.body_mut().read_to_string().is_err());
        assert!(started.elapsed() >= pause);
    }

    #[test]
    fn a_kill_finds_nothing_until_the_query_is_registered() {
        let mock = Mock::start();
        mock.registration(Registration::Signal);
        mock.on_statement("long", [Response::body("x")]);
        post(&mock, "query_id=abc", "long");
        let kill = "KILL QUERY WHERE query_id = 'abc' SYNC";

        assert_eq!(text(post(&mock, "", kill)), "");
        assert!(mock.killed().is_empty());

        mock.register("abc");
        let found = text(post(&mock, "", kill));
        assert!(found.starts_with("kill_status\tquery_id"), "{found}");
        assert!(found.contains("finished\tabc\t"), "{found}");
        assert_eq!(mock.killed(), ["abc"]);
    }

    #[test]
    fn a_registration_can_come_after_a_delay() {
        let mock = Mock::start();
        mock.registration(Registration::After(Duration::from_millis(150)));
        mock.on_statement("long", [Response::body("x")]);
        post(&mock, "query_id=abc", "long");
        let kill = "KILL QUERY WHERE query_id = 'abc' SYNC";
        assert_eq!(text(post(&mock, "", kill)), "");
        std::thread::sleep(Duration::from_millis(200));
        assert!(text(post(&mock, "", kill)).contains("finished"));
        mock.kill_status("cant_cancel");
        assert!(text(post(&mock, "", kill)).contains("cant_cancel"));
    }

    #[test]
    fn a_kill_ends_a_waiting_response_in_the_way_it_was_told() {
        let mock = Mock::start();
        mock.on_statement(
            "long",
            [Response::rows(&[("n", "UInt8")], &[&["1"]])
                .stalled(8, Duration::from_secs(5))
                .killed_by(Aftermath::Cancelled)],
        );
        let id = "abc";
        let handle = {
            let state = Arc::clone(&mock.state);
            let port = mock.port;
            std::thread::spawn(move || {
                let mock = Mock { port, state };
                let mut response = post(&mock, &format!("query_id={id}"), "long");
                let tag = response
                    .headers()
                    .get("X-ClickHouse-Exception-Tag")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let framed = Framed::tagged(response.body_mut().as_reader(), &tag, "long".into());
                let opened = Reader::open(BufReader::new(framed), None);
                opened
                    .map(|mut reader| reader.drain())
                    .and_then(|drained| drained)
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        text(post(&mock, "", "KILL QUERY WHERE query_id = 'abc' SYNC"));
        let error = handle.join().unwrap().unwrap_err();
        assert!(error.message.contains("Query was cancelled"), "{error}");
    }

    #[test]
    fn a_second_request_in_a_busy_session_is_locked_out() {
        let mock = Mock::start();
        mock.on_statement(
            "slow",
            [Response::body("x").delayed(Duration::from_millis(300))],
        );
        mock.on_statement("fast", [Response::body("y")]);
        let state = Arc::clone(&mock.state);
        let port = mock.port;
        let first =
            std::thread::spawn(move || text(post(&Mock { port, state }, "session_id=s1", "slow")));
        std::thread::sleep(Duration::from_millis(100));
        let (status, body) = failure_for(&mock, "session_id=s1", "fast");
        assert_eq!(status, 500);
        assert!(body.contains("SESSION_IS_LOCKED"), "{body}");
        first.join().unwrap();
        assert_eq!(text(post(&mock, "session_id=s1", "fast")), "y");
    }

    #[test]
    fn a_stall_after_a_complete_body_does_not_hold_the_session() {
        for body in ["", "done"] {
            let mock = Mock::start();
            mock.on_statement(
                "complete",
                [Response::body(body).stalled(body.len(), Duration::from_secs(10))],
            );
            mock.on_statement("next", [Response::body("ready")]);
            assert_eq!(text(post(&mock, "session_id=s1", "complete")), body);
            let (status, body) = failure_for(&mock, "session_id=s1", "next");
            assert_eq!((status, body.as_str()), (200, "ready"));
        }
    }

    fn failure_for(mock: &Mock, query: &str, statement: &str) -> (u16, String) {
        let mut response = post(mock, query, statement);
        let status = response.status().as_u16();
        (status, response.body_mut().read_to_string().unwrap())
    }

    #[test]
    fn an_expired_session_is_not_found_only_when_checked() {
        let mock = Mock::start();
        mock.on_statement("q", [Response::body("ok")]);
        post(&mock, "session_id=s", "q");
        mock.expire_session("s");

        let (status, body) = failure_for(&mock, "session_id=s&session_check=1", "q");
        assert_eq!(status, 500);
        assert!(body.contains("SESSION_NOT_FOUND"), "{body}");
        assert!(body.starts_with("Code: 372."), "{body}");
        assert_eq!(text(post(&mock, "session_id=s", "q")), "ok");
        mock.expire_sessions();
        assert_eq!(text(post(&mock, "session_id=s", "q")), "ok");
    }

    #[test]
    fn a_session_timeout_over_the_ceiling_is_refused_on_creation() {
        let mock = Mock::start();
        mock.max_session_timeout(60);
        mock.on_statement("q", [Response::body("ok")]);
        let (_, body) = failure_for(&mock, "session_id=s&session_timeout=3600", "q");
        assert!(body.starts_with("Code: 374."), "{body}");
        assert_eq!(
            text(post(&mock, "session_id=s&session_timeout=60", "q")),
            "ok"
        );
        // An existing session is not asked again.
        assert_eq!(
            text(post(
                &mock,
                "session_id=s&session_timeout=3600&session_check=1",
                "q"
            )),
            "ok"
        );
    }

    #[test]
    fn a_readonly_user_may_not_send_the_settings_named() {
        let mock = Mock::start();
        mock.readonly(&["max_execution_time"]);
        mock.on_statement("q", [Response::body("ok")]);
        let (status, body) = failure_for(&mock, "max_execution_time=5", "q");
        assert_eq!(status, 500);
        assert!(body.starts_with("Code: 164."), "{body}");
        assert!(body.contains("'max_execution_time'"), "{body}");
        assert_eq!(text(post(&mock, "enable_http_compression=1", "q")), "ok");
    }

    #[test]
    fn an_error_response_carries_its_code_header_and_a_200_can_too() {
        let mock = Mock::start();
        mock.on_statement("a", [Response::error(400, 62, "SYNTAX_ERROR", "bad")]);
        mock.on_statement("b", [Response::body("").code(60)]);
        let a = client(&mock)
            .post(&format!("http://127.0.0.1:{}/", mock.port))
            .send("a")
            .unwrap();
        assert_eq!(a.status().as_u16(), 400);
        assert_eq!(
            a.headers().get("X-ClickHouse-Exception-Code").unwrap(),
            "62"
        );
        let b = post(&mock, "", "b");
        assert_eq!(b.status().as_u16(), 200);
        assert_eq!(
            b.headers().get("X-ClickHouse-Exception-Code").unwrap(),
            "60"
        );
    }
}

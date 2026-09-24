//! A stand-in for Snowflake's SQL REST API, so the connection is tested in a
//! plain `cargo test` with no account and no network.
//!
//! It replays responses recorded from a real account, kept under
//! `dev/snowflake/fixtures`, and speaks just enough HTTP/1.1 for ureq: one
//! request per connection, answered and closed. Nothing is checked about the
//! token; the unit tests above already verify it against the key.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::SnowflakeConfig;

/// Every path the connection asks for is under this, so routes and the
/// requests seen are written without it: `?async=true`, `/{handle}`.
const PREFIX: &str = "/api/v2/statements";

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/dev/snowflake/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// How much of a body goes out before the connection is closed.
#[derive(Clone, Debug)]
enum Delivery {
    Whole,
    Truncated(usize),
    Stalled(usize, Duration),
}

/// One response. A route answers with its responses in turn and repeats the last.
#[derive(Clone, Debug)]
pub struct Response {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
    gzip: bool,
    delivery: Delivery,
}

impl Response {
    /// A recorded body. A `.gz` fixture goes out compressed, as the server
    /// sends a result partition.
    pub fn fixture(status: u16, name: &str) -> Self {
        Self {
            status,
            body: fixture(name),
            content_type: match name.ends_with(".html") {
                true => "text/html",
                false => "application/json",
            },
            gzip: name.ends_with(".gz"),
            delivery: Delivery::Whole,
        }
    }

    /// A body that was never recorded, such as a proxy's bare 503.
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
            content_type: "text/plain",
            gzip: false,
            delivery: Delivery::Whole,
        }
    }

    /// The whole length promised and only `sent` bytes of it delivered.
    pub fn truncated(self, sent: usize) -> Self {
        Self {
            delivery: Delivery::Truncated(sent),
            ..self
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
}

/// A request as the mock received it.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Path and query below [`PREFIX`].
    pub target: String,
    pub headers: Vec<(String, String)>,
    /// `Null` when there was none or it was not JSON.
    pub body: Value,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Route {
    method: String,
    target: String,
    /// Matched against the submitted statement by substring; empty matches
    /// anything, a GET included.
    statement: String,
    responses: VecDeque<Response>,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    requests: Vec<Request>,
}

pub struct Mock {
    address: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    /// Listening on a free loopback port, and already answering the
    /// `SELECT CURRENT_VERSION()` that [`super::Connection::open`] sends.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = format!("http://{}", listener.local_addr().expect("bound"));
        let state = Arc::<Mutex<State>>::default();
        let shared = Arc::clone(&state);
        // ponytail: never joined. The listener lives until the test binary
        // exits, which is sooner than it could matter.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        let mock = Self { address, state };
        mock.answer("SELECT CURRENT_VERSION()", "version");
        mock
    }

    /// A profile pointed at this mock.
    pub fn config(&self) -> SnowflakeConfig {
        SnowflakeConfig {
            account: "myorg-myaccount".into(),
            host: Some(self.address.clone()),
            user: "tim".into(),
            private_key: format!(
                "{}/dev/snowflake/test-key-2048.p8",
                env!("CARGO_MANIFEST_DIR")
            ),
            database: "DBDELVE_TEST".into(),
            warehouse: Some("COMPUTE_WH".into()),
            ..Default::default()
        }
    }

    /// Answer `method target` with `responses` in turn. The newest route that
    /// matches wins, so a test can change the answer while a query is polling.
    pub fn on(&self, method: &str, target: &str, responses: impl IntoIterator<Item = Response>) {
        self.route(method, target, "", responses);
    }

    /// Answer a submission to `target` (`""` synchronous, `"?async=true"`
    /// not) whose statement contains `statement`.
    pub fn on_statement(
        &self,
        target: &str,
        statement: &str,
        responses: impl IntoIterator<Item = Response>,
    ) {
        self.route("POST", target, statement, responses);
    }

    fn route(
        &self,
        method: &str,
        target: &str,
        statement: &str,
        responses: impl IntoIterator<Item = Response>,
    ) {
        let responses: VecDeque<_> = responses.into_iter().collect();
        assert!(!responses.is_empty(), "a route needs a response");
        self.state.lock().expect("unpoisoned").routes.push(Route {
            method: method.into(),
            target: target.into(),
            statement: statement.into(),
            responses,
        });
    }

    /// A recorded asynchronous exchange: `{name}_submit.json` accepts the
    /// statement and `{name}_poll.json` finishes it. Returns the handle, for a
    /// test that goes on to route the poll differently.
    pub fn answer(&self, statement: &str, name: &str) -> String {
        let handle = self.accept(statement, name);
        self.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(200, &format!("{name}_poll.json"))],
        );
        handle
    }

    /// Only the accepting half of [`Mock::answer`]; the poll is the caller's.
    pub fn accept(&self, statement: &str, name: &str) -> String {
        let submit = format!("{name}_submit.json");
        self.on_statement("?async=true", statement, [Response::fixture(202, &submit)]);
        fixture_json(&submit)["statementHandle"]
            .as_str()
            .expect("an accepted statement names its handle")
            .to_string()
    }

    /// Every request so far, in the order they arrived.
    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().expect("unpoisoned").requests.clone()
    }

    /// How many requests `method target` has had.
    pub fn hits(&self, method: &str, target: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == method && request.target == target)
            .count()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let response = {
        let mut state = state.lock().expect("unpoisoned");
        state.requests.push(request.clone());
        let statement = request.body["statement"].as_str().unwrap_or_default();
        state
            .routes
            .iter_mut()
            .rev()
            .find(|route| {
                route.method == request.method
                    && route.target == request.target
                    && statement.contains(&route.statement)
            })
            .map(|route| match route.responses.len() {
                1 => route.responses[0].clone(),
                _ => route.responses.pop_front().expect("not empty"),
            })
    };
    let response = response.unwrap_or_else(|| {
        // Loud rather than a hang: the message is what the failing test shows.
        let message = format!("mock: no route for {} {}", request.method, request.target);
        Response::text(404, &serde_json::json!({ "message": message }).to_string())
    });
    let _ = write_response(stream, &response);
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let target = target.strip_prefix(PREFIX).unwrap_or(target).to_string();

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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;

    Some(Request {
        method,
        target,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn write_response(mut stream: TcpStream, response: &Response) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} Mock\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    if response.gzip {
        head.push_str("Content-Encoding: gzip\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;

    let sent = match response.delivery {
        Delivery::Whole => response.body.len(),
        Delivery::Truncated(sent) | Delivery::Stalled(sent, _) => sent.min(response.body.len()),
    };
    stream.write_all(&response.body[..sent])?;
    stream.flush()?;
    if let Delivery::Stalled(_, pause) = response.delivery {
        std::thread::sleep(pause);
    }
    stream.shutdown(std::net::Shutdown::Both)
}

mod tests {
    use super::*;
    use std::time::Instant;

    fn get(mock: &Mock, target: &str) -> String {
        let address = mock.address.trim_start_matches("http://");
        let mut stream = TcpStream::connect(address).expect("listening");
        write!(
            stream,
            "GET {PREFIX}{target} HTTP/1.1\r\nHost: mock\r\n\r\n"
        )
        .expect("sent");
        let mut received = String::new();
        stream.read_to_string(&mut received).expect("read");
        received
    }

    #[test]
    fn a_cut_response_promises_more_than_it_delivers() {
        let mock = Mock::start();
        let body = || Response::text(200, "0123456789");
        mock.on("GET", "/cut", [body().truncated(4)]);
        let pause = Duration::from_millis(200);
        mock.on("GET", "/stall", [body().stalled(4, pause)]);

        for (target, least) in [("/cut", Duration::ZERO), ("/stall", pause)] {
            let started = Instant::now();
            let received = get(&mock, target);
            assert!(received.contains("Content-Length: 10\r\n"), "{received}");
            assert!(received.ends_with("\r\n\r\n0123"), "{received}");
            assert!(started.elapsed() >= least, "{target}");
        }
    }
}

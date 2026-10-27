//! The HTTP side of ClickHouse: the agent and its TLS, the SSH tunnel as a
//! resolver, the transport that lets one thread end another's read or write,
//! and the server's errors.

use std::cell::RefCell;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ureq::config::Config;
use ureq::http::HeaderMap;
use ureq::tls::{Certificate, RootCerts, TlsConfig};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
};
use ureq::{Agent, Error, Timeout};

use crate::db::ssh::Tunnel;
use crate::db::{DbError, ServerConfig, plain_error};
use crate::tls::{self, SslMode};

/// The HTTP interface's plaintext port and its TLS one.
const PLAIN_PORT: u16 = super::DEFAULT_PORT;
const TLS_PORT: u16 = 8443;

/// Cloud services wake on their first query.
const CONNECT_WITHIN: Duration = Duration::from_secs(30);

/// How often a blocked read looks at its abort flag and idle bound.
const TICK: Duration = Duration::from_millis(250);

/// What DBDelve's own catalog and structure requests wait for.
const INTERNAL_BOUND: Duration = Duration::from_secs(120);

/// A user's timeout and the client's slack around it (design 4.5). Short under
/// test, where the idle bound is waited out for real.
const SLACK: Duration = match cfg!(test) {
    true => Duration::from_millis(500),
    false => Duration::from_secs(30),
};

/// The HTTP interface's own split: 8123 in the clear, 8443 under TLS. Prefer
/// is the first, as the server answers the plain port to it first.
pub fn port(server: &ServerConfig) -> u16 {
    server.port.unwrap_or(match server.sslmode {
        SslMode::Disable | SslMode::Prefer => PLAIN_PORT,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => TLS_PORT,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    /// What a mode asks for first; `prefer` falling back to `Http` is the
    /// caller's, once, at open.
    pub fn of(mode: SslMode) -> Self {
        match mode {
            SslMode::Disable => Self::Http,
            _ => Self::Https,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// How long a request may wait, one number for both of the client's bounds: no
/// response for this long, and no byte for this long. A `KILL`'s also covers
/// the whole request, the connect included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds(Option<Duration>, Option<Duration>);

impl Bounds {
    /// A user's statements: the profile's timeout plus slack, or none when it
    /// has none, and Cancel is the remedy.
    pub fn user(statement_timeout: u32) -> Self {
        Self(
            (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout)) + SLACK),
            None,
        )
    }

    /// DBDelve's own catalog and structure requests.
    pub fn internal() -> Self {
        Self(Some(INTERNAL_BOUND), None)
    }

    /// The whole request, from the connect to the body's end.
    pub fn within(limit: Duration) -> Self {
        Self(Some(limit), Some(limit))
    }

    pub fn limit(self) -> Option<Duration> {
        self.0
    }

    pub fn whole(self) -> Option<Duration> {
        self.1
    }
}

/// The TLS a mode asks for. `verify-ca` is verified as `verify-full`: ureq
/// takes no custom verifier, and stricter is allowed (hard rule 6).
pub fn tls_config(mode: SslMode, root_certificate: Option<&str>) -> Result<TlsConfig, DbError> {
    let builder = TlsConfig::builder();
    let config = match mode {
        SslMode::Disable => builder,
        SslMode::Prefer | SslMode::Require => builder.disable_verification(true),
        SslMode::VerifyCa | SslMode::VerifyFull => {
            let certificates = tls::root_certificates(root_certificate)
                .map_err(|error| plain_error(format!("Could not configure TLS: {error}")))?
                .iter()
                .map(|der| Certificate::from_der(der.as_ref()).to_owned())
                .collect::<Vec<_>>();
            builder.root_certs(RootCerts::new_with_certs(&certificates))
        }
    };
    Ok(config.build())
}

/// An agent for `server`: bounded by `bounds`, dialling through `tunnel` when
/// there is one, and wrapped in [`Watched`] under TLS.
pub fn agent(
    server: &ServerConfig,
    tunnel: Option<Arc<Tunnel>>,
    scheme: Scheme,
    bounds: Bounds,
) -> Result<Agent, DbError> {
    let tls = match scheme {
        Scheme::Http => TlsConfig::default(),
        Scheme::Https => tls_config(server.sslmode, server.root_certificate.as_deref())?,
    };
    let config = Config::builder()
        // The server explains a failed statement in the body.
        .http_status_as_error(false)
        .max_redirects(0)
        // Only the connectors below are in the chain, which a proxy setting
        // from the environment would otherwise promise.
        .proxy(None)
        .timeout_connect(Some(CONNECT_WITHIN))
        .timeout_recv_response(bounds.limit())
        .user_agent(concat!("dbdelve/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
        .build();
    // The watched TCP, then TLS: the watch sees ciphertext arrive, which is
    // the only idleness there is to measure.
    let connector = ().chain(WatchedConnector).chain(RustlsConnector::default());
    Ok(match tunnel {
        Some(tunnel) => Agent::with_parts(
            config,
            connector,
            TunnelResolver::new(move || tunnel.dial()),
        ),
        None => Agent::with_parts(config, connector, DefaultResolver::default()),
    })
}

/// Resolves every name to the tunnel's local end, asked afresh per socket so
/// a tunnel that has since closed fails the dial with ssh's own words. The
/// request's URI keeps the real host, so SNI and the hostname check see it.
pub struct TunnelResolver {
    dial: Box<dyn Fn() -> Result<SocketAddr, DbError> + Send + Sync>,
}

impl TunnelResolver {
    pub fn new(dial: impl Fn() -> Result<SocketAddr, DbError> + Send + Sync + 'static) -> Self {
        Self {
            dial: Box::new(dial),
        }
    }
}

impl fmt::Debug for TunnelResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TunnelResolver")
    }
}

impl Resolver for TunnelResolver {
    fn resolve(
        &self,
        _: &ureq::http::Uri,
        _: &Config,
        _: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let address = (self.dial)().map_err(|error| Error::Other(Box::new(error)))?;
        let mut addresses = self.empty();
        addresses.push(address);
        Ok(addresses)
    }
}

/// Why a [`Watch`] ended a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Aborted,
    Idle,
}

/// What a blocked read or write checks between ticks: whether another thread
/// asked it to end, and how long it may wait for a byte to move.
///
/// ureq offers only total timeouts and reads on the calling thread, so the
/// watch reaches the transport through a thread-local set around each call.
#[derive(Clone)]
pub struct Watch {
    abort: Arc<AtomicBool>,
    idle: Option<Duration>,
    stopped: Arc<Mutex<Option<Stop>>>,
    /// A byte of the request went out, so the server may be running it.
    sent: Arc<AtomicBool>,
}

thread_local! {
    static WATCHING: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

/// Puts the previous watch back.
pub struct Watching(Option<Watch>);

impl Drop for Watching {
    fn drop(&mut self) {
        WATCHING.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

impl Watch {
    pub fn new(abort: Arc<AtomicBool>, idle: Option<Duration>) -> Self {
        Self {
            abort,
            idle,
            stopped: Arc::default(),
            sent: Arc::default(),
        }
    }

    /// Reads on this thread are watched until the guard drops.
    pub fn enter(&self) -> Watching {
        Watching(WATCHING.with(|slot| slot.borrow_mut().replace(self.clone())))
    }

    /// Ends the read or write in progress, within a tick, and every later one.
    pub fn abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    /// Whether the request began to go out, past any TLS handshake.
    pub fn sent(&self) -> bool {
        self.sent.load(Ordering::SeqCst)
    }

    /// What ended a read, if one was ended.
    pub fn stopped(&self) -> Option<Stop> {
        *self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stop(&self, why: Stop) {
        *self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(why);
    }
}

/// Opens the TCP socket itself, so [`Watched`] owns it: ureq's own TCP
/// transport writes with one blocking `write_all`, which no other thread can
/// end and whose partial progress it does not tell.
#[derive(Debug)]
struct WatchedConnector;

impl Connector for WatchedConnector {
    type Out = Watched;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> Result<Option<Watched>, Error> {
        let config = details.config;
        let started = Instant::now();
        let budget = (!details.timeout.after.is_not_happening()).then(|| *details.timeout.after);
        let mut failed = None;
        // The budget is shared out evenly, so a first address that never
        // answers leaves time for the next.
        for (at, address) in details.addrs.iter().enumerate() {
            let attempt = match budget {
                Some(budget) => {
                    let left = budget.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        break;
                    }
                    let share = left / u32::try_from(details.addrs.len() - at).unwrap_or(1);
                    TcpStream::connect_timeout(address, share.max(Duration::from_millis(10)))
                }
                None => TcpStream::connect(address),
            };
            match attempt {
                Ok(stream) => {
                    if config.no_delay() {
                        stream.set_nodelay(true)?;
                    }
                    let buffers =
                        LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size());
                    return Ok(Some(Watched {
                        stream,
                        buffers,
                        timeouts: (None, None),
                        ended: false,
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    failed = Some(Error::Timeout(details.timeout.reason));
                }
                Err(error) => failed = Some(Error::Io(error)),
            }
        }
        Err(failed.unwrap_or(Error::Timeout(details.timeout.reason)))
    }
}

/// A TCP transport that waits in ticks, so the thread-local [`Watch`] is
/// looked at between them, whether it is reading or writing.
struct Watched {
    stream: TcpStream,
    buffers: LazyBuffers,
    /// The read and write timeouts the socket has now, to set them only when
    /// they change.
    timeouts: (Option<Duration>, Option<Duration>),
    /// Ended by the watch or mid-write, so the stream is in no state to be
    /// pooled.
    ended: bool,
}

impl fmt::Debug for Watched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watched")
            .field("peer", &self.stream.peer_addr().ok())
            .finish()
    }
}

impl Watched {
    /// How long the next blocking step may take: a tick at most, less when
    /// the idle bound or the call's budget is nearer. An error when the watch
    /// was aborted or either has run out.
    fn step(
        &mut self,
        watch: Option<&Watch>,
        idle_since: Instant,
        called: Instant,
        timeout: NextTimeout,
    ) -> Result<Duration, Error> {
        if let Some(watch) = watch
            && watch.abort.load(Ordering::SeqCst)
        {
            self.ended = true;
            watch.stop(Stop::Aborted);
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "The request was ended from another thread.",
            )));
        }
        let idle_left = watch
            .and_then(|watch| watch.idle)
            .map(|idle| idle.saturating_sub(idle_since.elapsed()));
        if let Some(watch) = watch
            && idle_left == Some(Duration::ZERO)
        {
            self.ended = true;
            watch.stop(Stop::Idle);
            return Err(Error::Timeout(timeout.reason));
        }
        let budget_left = match timeout.after.is_not_happening() {
            true => None,
            false => Some(timeout.after.saturating_sub(called.elapsed())),
        };
        if budget_left == Some(Duration::ZERO) {
            return Err(Error::Timeout(timeout.reason));
        }
        Ok([Some(TICK), idle_left, budget_left]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(TICK))
    }
}

/// A wait that ran out, which a tick is expected to.
fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

impl Transport for Watched {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let watch = WATCHING.with(|slot| slot.borrow().clone());
        // A write on the connect's budget is the TLS handshake; any other is
        // the request, which the server may act on once it is whole.
        if let Some(watch) = &watch
            && timeout.reason != Timeout::Connect
        {
            watch.sent.store(true, Ordering::SeqCst);
        }
        let called = Instant::now();
        let mut progressed = called;
        let mut written = 0;
        while written < amount {
            let wait = self.step(watch.as_ref(), progressed, called, timeout)?;
            if self.timeouts.1 != Some(wait) {
                self.stream.set_write_timeout(Some(wait))?;
                self.timeouts.1 = Some(wait);
            }
            match self.stream.write(&self.buffers.output()[written..amount]) {
                Ok(0) => {
                    self.ended = true;
                    return Err(Error::Io(io::ErrorKind::WriteZero.into()));
                }
                Ok(sent) => {
                    written += sent;
                    progressed = Instant::now();
                }
                Err(error) if timed_out(&error) => {}
                Err(error) => {
                    self.ended = true;
                    return Err(Error::Io(error));
                }
            }
        }
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let watch = WATCHING.with(|slot| slot.borrow().clone());
        let called = Instant::now();
        loop {
            let wait = self.step(watch.as_ref(), called, called, timeout)?;
            if self.timeouts.0 != Some(wait) {
                self.stream.set_read_timeout(Some(wait))?;
                self.timeouts.0 = Some(wait);
            }
            match self.stream.read(self.buffers.input_append_buf()) {
                Ok(read) => {
                    self.buffers.input_appended(read);
                    return Ok(read > 0);
                }
                Err(error) if timed_out(&error) => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
    }

    /// Never when the watch ended it; otherwise open while nothing is
    /// waiting to be read and the peer has not closed it, as ureq's own TCP
    /// transport checks.
    fn is_open(&mut self) -> bool {
        if self.ended || self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let idle = matches!(
            self.stream.read(&mut [0]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        );
        idle && self.stream.set_nonblocking(false).is_ok()
    }
}

const CODE_HEADER: &str = "X-ClickHouse-Exception-Code";

pub fn exception_code(headers: &HeaderMap) -> Option<i64> {
    headers
        .get(CODE_HEADER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|code| *code != 0)
}

/// Whether a response is an error before its body: a status outside 2xx, or a
/// nonzero exception code whatever the status, as a proxy may rewrite it.
pub fn failed(status: u16, headers: &HeaderMap) -> bool {
    !(200..300).contains(&status) || exception_code(headers).is_some()
}

/// The error a failed response carries. `statement` is what the server was
/// sent, which its error positions are checked against.
pub fn exception(status: u16, headers: &HeaderMap, body: &[u8], statement: &str) -> DbError {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if let Some(error) = late_error(text) {
        return server_error(error, statement);
    }
    let code = exception_code(headers).map(|code| format!(", exception code {code}"));
    let code = code.as_deref().unwrap_or_default();
    // A proxy's page or a server's bare line.
    let shown: String = text.chars().take(500).collect();
    plain_error(match shown.is_empty() {
        true => format!("The server answered HTTP {status}{code} with no explanation."),
        false => format!("The server answered HTTP {status}{code}: {shown}"),
    })
}

/// The server's error in a failed response's body: all of it, or the last
/// line of one that began with rows. A response the server compresses is held
/// back until it ends, so an error after the first rows still has the 500
/// status, and the rows go before the error in the same body.
fn late_error(text: &str) -> Option<&str> {
    let starts_error = |line: &str| {
        line.strip_prefix("Code: ")
            .and_then(|rest| rest.split_once(". DB::Exception: "))
            .is_some_and(|(digits, _)| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
    };
    if starts_error(text) {
        return Some(text);
    }
    text.match_indices("\nCode: ")
        .map(|(at, _)| &text[at + 1..])
        .filter(|rest| starts_error(rest))
        .last()
}

/// A server's `Code: N. DB::Exception: <message> (NAME) (version V)` as the
/// message alone, with the position it names checked against `statement`.
pub fn server_error(text: &str, statement: &str) -> DbError {
    let mut message = text.trim();
    if let Some(rest) = message.strip_prefix("Code: ")
        && let Some((digits, rest)) = rest.split_once(". DB::Exception: ")
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        message = rest;
    }
    if message.ends_with(')')
        && let Some(at) = message.rfind(" (version ")
    {
        message = message[..at].trim_end();
    }
    DbError {
        message: message.to_string(),
        position: position(message, statement),
    }
}

/// "failed at position N" counts bytes from 1, over the statement as sent.
fn position(message: &str, statement: &str) -> Option<usize> {
    let (_, rest) = message.split_once("failed at position ")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let at = rest[..digits].parse::<usize>().ok()?.checked_sub(1)?;
    (at < statement.len() && statement.is_char_boundary(at)).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use ureq::http::HeaderValue;

    fn server(port: u16, mode: SslMode) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".into(),
            port: Some(port),
            sslmode: mode,
            ..Default::default()
        }
    }

    const ROOT: &str = "-----BEGIN CERTIFICATE-----
MIIBjjCCATWgAwIBAgIUBuhL8FCVGB6Rc6TMbB/gIZpeTAMwCgYIKoZIzj0EAwIw
HDEaMBgGA1UEAwwRZGJkZWx2ZSB0ZXN0IHJvb3QwIBcNMjYxMDI2MTMzMDIxWhgP
MjEyNjEwMDIxMzMwMjFaMBwxGjAYBgNVBAMMEWRiZGVsdmUgdGVzdCByb290MFkw
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEwDEHQbaeiNwohuF3x79jrvaETTsdgm+J
8FYBAMuI9G+QQSxuVQmWUEOOqCgEUe4AyrsWZeNni925PvemJPZmkaNTMFEwHQYD
VR0OBBYEFGr0CZJbiJK9O+/la7tE6Q7n1S3BMB8GA1UdIwQYMBaAFGr0CZJbiJK9
O+/la7tE6Q7n1S3BMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDRwAwRAIg
eHr3rg6UeZ4jDdWtiMUw9mFv3GLdfl9ELVIcZTQCa44CIBgeILwM5ixSWOgVrWx7
EHziqoisXzppWwn2vAWvoTkc
-----END CERTIFICATE-----
";

    /// Written once: the tests that need it run in parallel.
    fn root_file() -> &'static str {
        static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            let path =
                std::env::temp_dir().join(format!("dbdelve-ch-root-{}.pem", std::process::id()));
            std::fs::write(&path, ROOT).unwrap();
            path.to_string_lossy().into_owned()
        })
    }

    #[test]
    fn a_blank_port_follows_the_mode() {
        let blank = |mode| ServerConfig {
            sslmode: mode,
            ..Default::default()
        };
        assert_eq!(port(&blank(SslMode::Disable)), 8123);
        assert_eq!(port(&blank(SslMode::Prefer)), 8123);
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(port(&blank(mode)), 8443, "{mode:?}");
        }
        assert_eq!(port(&server(9999, SslMode::Require)), 9999);
    }

    #[test]
    fn a_mode_picks_the_scheme_and_what_is_verified() {
        assert_eq!(Scheme::of(SslMode::Disable), Scheme::Http);
        for mode in [
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(Scheme::of(mode), Scheme::Https);
        }
        let root = root_file();
        let unverified = |mode| tls_config(mode, None).unwrap().disable_verification();
        assert!(unverified(SslMode::Prefer));
        assert!(unverified(SslMode::Require));
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let config = tls_config(mode, Some(root)).unwrap();
            assert!(!config.disable_verification(), "{mode:?}");
            assert!(matches!(config.root_certs(), RootCerts::Specific(certs) if certs.len() == 1));
        }
    }

    #[test]
    fn verify_ca_is_verified_exactly_as_verify_full_is() {
        let root = root_file();
        let ca = tls_config(SslMode::VerifyCa, Some(root)).unwrap();
        let full = tls_config(SslMode::VerifyFull, Some(root)).unwrap();
        assert_eq!(ca.disable_verification(), full.disable_verification());
        assert!(matches!((ca.root_certs(), full.root_certs()),
                (RootCerts::Specific(a), RootCerts::Specific(b)) if a.len() == b.len()));
    }

    #[test]
    fn a_root_file_that_is_missing_fails_instead_of_falling_back() {
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let error = tls_config(mode, Some("/nonexistent/ca.pem")).err().unwrap();
            assert!(error.message.contains("/nonexistent/ca.pem"), "{error}");
        }
    }

    #[test]
    fn bounds_are_the_timeout_plus_slack_or_none() {
        assert_eq!(Bounds::user(0).limit(), None);
        assert_eq!(
            Bounds::user(10).limit(),
            Some(Duration::from_secs(10) + SLACK)
        );
        assert_eq!(Bounds::internal().limit(), Some(Duration::from_secs(120)));
    }

    /// Accepts one connection, reads the request, then runs `serve`.
    ///
    /// The whole request, body and all: Windows answers a close with unread
    /// data waiting by resetting the connection, which fails a client that
    /// would otherwise have read the answer `serve` wrote.
    fn listener(serve: impl FnOnce(std::net::TcpStream) + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            serve(stream);
        });
        port
    }

    /// Reads one request through to the end of its body, or until the peer
    /// sends something that is not HTTP (a TLS hello, which waits for an
    /// answer rather than finishing), stops, or goes quiet. Returns what it
    /// read.
    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut seen = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            if let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if seen.len() >= end + 4 + length {
                    return seen;
                }
            } else if seen.first().is_some_and(|byte| !byte.is_ascii_uppercase()) {
                return seen;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return seen,
                Ok(read) => seen.extend_from_slice(&chunk[..read]),
            }
        }
    }

    const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

    fn post(agent: &Agent, port: u16) -> Result<String, Error> {
        let mut response = agent
            .post(&format!("http://127.0.0.1:{port}/"))
            .send("SELECT 1")?;
        response.body_mut().read_to_string()
    }

    fn plain_agent(port: u16, bounds: Bounds) -> Agent {
        agent(&server(port, SslMode::Disable), None, Scheme::Http, bounds).unwrap()
    }

    #[test]
    fn a_stalled_server_is_ended_by_the_idle_bound() {
        let port = listener(|stream| {
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let watch = Watch::new(Arc::default(), Some(Duration::from_millis(400)));
        let _watching = watch.enter();
        let started = Instant::now();
        let result = post(&plain_agent(port, Bounds::user(0)), port);
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(watch.stopped(), Some(Stop::Idle));
    }

    #[test]
    fn an_abort_from_another_thread_ends_the_read_within_a_tick() {
        let port = listener(|stream| {
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let watch = Watch::new(Arc::default(), None);
        let other = watch.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            other.abort();
        });
        let _watching = watch.enter();
        let started = Instant::now();
        assert!(post(&plain_agent(port, Bounds::user(0)), port).is_err());
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_millis(300) + TICK * 2, "{took:?}");
        assert_eq!(watch.stopped(), Some(Stop::Aborted));
    }

    #[test]
    fn a_slow_answer_inside_the_idle_bound_still_arrives() {
        let port = listener(|mut stream| {
            std::thread::sleep(TICK * 2 + Duration::from_millis(100));
            stream.write_all(OK).unwrap();
        });
        let watch = Watch::new(Arc::default(), Some(Duration::from_secs(5)));
        let _watching = watch.enter();
        assert_eq!(
            post(&plain_agent(port, Bounds::user(0)), port).unwrap(),
            "ok"
        );
        assert_eq!(watch.stopped(), None);
    }

    #[test]
    fn the_response_bound_still_applies_under_the_watch() {
        let port = listener(|stream| {
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let agent = agent(
            &server(port, SslMode::Disable),
            None,
            Scheme::Http,
            Bounds(Some(Duration::from_millis(500)), None),
        )
        .unwrap();
        let watch = Watch::new(Arc::default(), None);
        let _watching = watch.enter();
        let started = Instant::now();
        assert!(matches!(post(&agent, port), Err(Error::Timeout(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Accepts one connection and neither reads nor answers it.
    fn silent_listener() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(10));
            drop(stream);
        });
        port
    }

    /// More than any socket buffer takes before the peer reads.
    fn upload(agent: &Agent, port: u16) -> Result<(), Error> {
        let body = vec![b'x'; 64 * 1024 * 1024];
        agent
            .post(&format!("http://127.0.0.1:{port}/"))
            .send(&body[..])
            .map(drop)
    }

    #[test]
    fn an_upload_the_server_stops_reading_is_ended_by_an_abort() {
        let port = silent_listener();
        let watch = Watch::new(Arc::default(), None);
        let other = watch.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            other.abort();
        });
        let _watching = watch.enter();
        let started = Instant::now();
        assert!(upload(&plain_agent(port, Bounds::user(0)), port).is_err());
        let took = started.elapsed();
        assert!(took < Duration::from_millis(300) + TICK * 2, "{took:?}");
        assert_eq!(watch.stopped(), Some(Stop::Aborted));
    }

    #[test]
    fn an_upload_the_server_stops_reading_is_ended_by_the_idle_bound() {
        let port = silent_listener();
        let watch = Watch::new(Arc::default(), Some(Duration::from_millis(400)));
        let _watching = watch.enter();
        let started = Instant::now();
        assert!(upload(&plain_agent(port, Bounds::user(0)), port).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(watch.stopped(), Some(Stop::Idle));
    }

    #[test]
    fn a_tls_handshake_is_not_the_request_going_out() {
        let port = listener(drop);
        let agent = agent(
            &server(port, SslMode::Require),
            None,
            Scheme::Https,
            Bounds::user(0),
        )
        .unwrap();
        let watch = Watch::new(Arc::default(), None);
        let _watching = watch.enter();
        assert!(
            agent
                .post(&format!("https://127.0.0.1:{port}/"))
                .send("SELECT 1")
                .is_err()
        );
        assert!(!watch.sent());

        let port = listener(drop);
        let watch = Watch::new(Arc::default(), None);
        let _watching = watch.enter();
        assert!(post(&plain_agent(port, Bounds::user(0)), port).is_err());
        assert!(watch.sent());
    }

    #[test]
    fn a_request_is_read_through_a_body_that_arrives_in_parts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .write_all(b"POST / HTTP/1.1\r\nContent-Length: 8\r\n\r\nSELE")
                .unwrap();
            std::thread::sleep(Duration::from_millis(100));
            stream.write_all(b"CT 1").unwrap();
            stream
        });
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_request(&mut stream).ends_with(b"\r\n\r\nSELECT 1"));
        drop(client.join().unwrap());
    }

    #[test]
    fn a_tunnel_resolver_dials_what_it_is_given_and_not_the_name() {
        let port = listener(|mut stream| stream.write_all(OK).unwrap());
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let agent = Agent::with_parts(
            Config::builder().proxy(None).build(),
            ().chain(WatchedConnector),
            TunnelResolver::new(move || Ok(address)),
        );
        let mut response = agent
            .post("http://clickhouse.invalid:8123/")
            .send("SELECT 1")
            .unwrap();
        assert_eq!(response.body_mut().read_to_string().unwrap(), "ok");
    }

    #[test]
    fn a_tunnel_that_has_closed_fails_the_dial_in_its_own_words() {
        let agent = Agent::with_parts(
            Config::builder().proxy(None).build(),
            ().chain(WatchedConnector),
            TunnelResolver::new(|| Err(plain_error("SSH tunnel through bastion closed.".into()))),
        );
        let error = agent
            .post("http://clickhouse.invalid:8123/")
            .send("SELECT 1")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("SSH tunnel through bastion closed.")
        );
    }

    fn headers(code: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CODE_HEADER, HeaderValue::from_str(code).unwrap());
        headers
    }

    #[test]
    fn a_nonzero_exception_code_is_a_failure_whatever_the_status() {
        assert!(failed(200, &headers("62")));
        assert!(failed(500, &HeaderMap::new()));
        assert!(!failed(200, &headers("0")));
        assert!(!failed(200, &HeaderMap::new()));
    }

    const SYNTAX: &str = "Code: 62. DB::Exception: Syntax error: failed at position 13 ('FRM') (line 1, col 13): FRM t. Expected one of: token, Comma (SYNTAX_ERROR) (version 25.8.1.5101 (official build))\n";

    #[test]
    fn the_prefix_and_the_version_are_dropped_and_the_name_kept() {
        let error = exception(400, &headers("62"), SYNTAX.as_bytes(), "SELECT 'é' FRM t");
        assert_eq!(
            error.message,
            "Syntax error: failed at position 13 ('FRM') (line 1, col 13): FRM t. Expected one of: token, Comma (SYNTAX_ERROR)"
        );
    }

    #[test]
    fn a_position_is_a_one_based_byte_offset_made_zero_based() {
        let statement = "SELECT 'é' FRM t";
        assert_eq!(&statement[12..15], "FRM");
        let error = server_error(SYNTAX, statement);
        assert_eq!(error.position, Some(12));
    }

    #[test]
    fn a_position_outside_the_statement_or_inside_a_character_is_dropped() {
        let statement = "SELECT 'é' FRM t";
        let at = |n: usize| {
            server_error(
                &format!("Code: 62. DB::Exception: failed at position {n} (X) (version 1)"),
                statement,
            )
            .position
        };
        assert_eq!(at(1), Some(0));
        assert_eq!(at(99), None);
        assert_eq!(at(statement.len() + 1), None);
        // Byte 9 is the second byte of the é.
        assert_eq!(at(10), None);
        assert_eq!(at(0), None);
    }

    #[test]
    fn a_body_that_is_not_the_servers_is_reported_with_its_status() {
        let error = exception(
            502,
            &HeaderMap::new(),
            b"<html>Bad Gateway</html>",
            "SELECT 1",
        );
        assert_eq!(
            error.message,
            "The server answered HTTP 502: <html>Bad Gateway</html>"
        );
        let error = exception(200, &headers("241"), b"", "SELECT 1");
        assert_eq!(
            error.message,
            "The server answered HTTP 200, exception code 241 with no explanation."
        );
    }

    #[test]
    fn a_server_error_on_a_200_is_read_like_any_other() {
        let error = exception(
            200,
            &headers("60"),
            b"Code: 60. DB::Exception: Table t doesn't exist (UNKNOWN_TABLE) (version 26.8.1.1)",
            "SELECT 1",
        );
        assert_eq!(error.message, "Table t doesn't exist (UNKNOWN_TABLE)");
    }
    #[test]
    fn a_recorded_early_exception_is_read_without_its_code_prefix_or_version() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/dev/clickhouse/fixtures");
        let body = std::fs::read(format!("{dir}/early_exception.tsv")).unwrap();
        let error = exception(500, &headers("395"), &body, "SELECT throwIf(1, 'boom')");
        assert!(
            error
                .message
                .starts_with("boom: while executing 'FUNCTION throwIf")
                && error
                    .message
                    .ends_with("(FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)"),
            "{}",
            error.message
        );
    }
    #[test]
    fn a_recorded_error_after_buffered_rows_is_the_last_line() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/dev/clickhouse/fixtures");
        let body = std::fs::read(format!("{dir}/buffered_exception.tsv")).unwrap();
        let error = exception(500, &headers("394"), &body, "SELECT sleepEachRow(1)");
        assert_eq!(error.message, "Query was cancelled. (QUERY_WAS_CANCELLED)");
    }

    #[test]
    fn a_body_with_text_that_only_looks_like_an_error_is_not_one() {
        let body = b"a\nCode: x. DB::Exception: no\nCode: 5 DB::Exception: no";
        let error = exception(500, &headers("5"), body, "SELECT 1");
        assert!(
            error.message.starts_with("The server answered HTTP 500"),
            "{}",
            error.message
        );
    }
}

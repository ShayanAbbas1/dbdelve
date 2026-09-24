//! Snowflake, over its SQL REST API.
//!
//! There is no driver here because Snowflake publishes none for Rust, and the
//! community ones are tokio futures, which panic on GPUI's executor. What is
//! left is the documented HTTP API and a blocking client, which suits the rest
//! of this directory better than it sounds: every value arrives as a JSON
//! string, so hard rule 4's rendered text is most of the way there on arrival.
//!
//! The cost is that there is no session. Nothing set in one submission reaches
//! the next, an open transaction included, and the API refuses a `USE` outright
//! ("Command not supported by SQL API: USE") rather than running one it would
//! then forget. That is the engine's behaviour and dbdelve does not paper over
//! it: names are qualified, or resolve against the profile's database.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{
    CancelToken, Cancelling, Catalog, Cell, Column, DbError, Engine, ForeignKey, NamedDefinition,
    QueryResult, Structure, assemble_catalog, assemble_structure, plain_error,
};

/// What it takes to reach one database in one Snowflake account.
///
/// Its own struct rather than a `ServerConfig`: there is no port, no password
/// and no `sslmode` -- the API is HTTPS and always verified, so there is
/// nothing to weaken (hard rule 7, satisfied by having no field to set).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnowflakeConfig {
    /// The account identifier, which the token names regardless of which host
    /// the request is sent to.
    pub account: String,
    /// Blank derives the host from the account. Privatelink, a regional domain
    /// or a proxy is what fills it in.
    pub host: Option<String>,
    pub user: String,
    /// Absolute path to an unencrypted private key. A path and not a secret,
    /// so it lives in the profile like `root_certificate` does and the
    /// Keychain holds nothing for this engine. Absolute because a relative one
    /// resolves against wherever the app was launched from, which for an app
    /// opened from Finder is `/`.
    pub private_key: String,
    /// One database per profile, as with Postgres. Every request carries it,
    /// which is why generated names stay two-part.
    pub database: String,
    /// Blank leaves the user's default in force, as does a blank role.
    pub warehouse: Option<String>,
    pub role: Option<String>,
    /// Seconds, or 0 for the account's own limit. Sent as a field of each
    /// request, never as SQL.
    pub statement_timeout: u32,
}

/// The domain an account's own host is under, when the profile names no other.
const ACCOUNT_DOMAIN: &str = ".snowflakecomputing.com";

/// The account identifier out of whatever was pasted for it. People have the
/// URL they sign in at far more often than the identifier inside it, and the
/// one is the other with a scheme in front and the domain behind.
pub fn account_identifier(input: &str) -> String {
    let input = input.trim();
    let host = input
        .split_once("://")
        .map_or(input, |(_, rest)| rest)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or_default();
    host.strip_suffix(ACCOUNT_DOMAIN)
        .unwrap_or(host)
        .to_string()
}

impl SnowflakeConfig {
    /// The database as the server stores its name, which is what a quoted
    /// identifier and a `SHOW` row both have to match.
    ///
    /// The profile holds what was typed, and the request's own `database`
    /// field takes that as SQL would: a bare name folded to upper case. So
    /// `analytics` and `ANALYTICS` are one database there, and would be two
    /// here without the same folding. A name that could not be written bare
    /// was necessarily created quoted, and is taken as it is.
    fn stored_database(&self) -> String {
        let bare = self
            .database
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_$".contains(character))
            && !self
                .database
                .starts_with(|first: char| first.is_ascii_digit());
        match bare {
            true => self.database.to_ascii_uppercase(),
            false => self.database.clone(),
        }
    }

    /// The host requests go to. The derived name is the service's documented
    /// default, overridable like any driver's default port.
    pub fn host(&self) -> String {
        match &self.host {
            Some(host) => host.clone(),
            None => format!("{}{ACCOUNT_DOMAIN}", self.account),
        }
    }
}

/// The token is good for an hour at most, and the server rejects one that
/// claims longer. A minute short of that leaves room for a clock that is not
/// quite the server's.
const TOKEN_LIFETIME: u64 = 59 * 60;

/// The private key a profile names, read fresh each time so replacing the file
/// takes effect without a reconnect.
///
/// An encrypted key is refused by name rather than prompted for.
///
/// ponytail: `ring` does not decrypt PKCS#8, and doing it means a PBES2
/// implementation, a KDF and AES beside it. The ceiling is an account whose
/// policy requires a passphrase; the upgrade path is the `pkcs8` crate's
/// `encryption` feature and a Keychain item for the passphrase.
fn key_pair(config: &SnowflakeConfig) -> Result<RsaKeyPair, DbError> {
    let path = &config.private_key;
    let text = std::fs::read_to_string(path).map_err(|error| {
        plain_error(format!("The private key at {path} was not read: {error}."))
    })?;
    let source = format!("The private key at {path}");
    if text.contains("ENCRYPTED PRIVATE KEY") {
        return Err(plain_error(format!("{source} is encrypted.")));
    }

    let der =
        key_der(&text).ok_or_else(|| plain_error(format!("{source} is not a private key.")))?;
    // PKCS#8 is what Snowflake's instructions produce; PKCS#1 is what
    // `BEGIN RSA PRIVATE KEY` holds, and `ring` reads either.
    RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|error| plain_error(format!("{source} is not an RSA key: {error}.")))
}

/// The DER inside a key file however the key was written to it: as PEM, as
/// its base64 body alone, or as the whole PEM base64-encoded once more, which
/// is how a key comes out of an environment variable or a secrets store.
///
/// A PEM reader would refuse two of those three, and all three are the same
/// bytes. So the armour lines are dropped, what is left is decoded, and a
/// result that turns out to be PEM itself goes round once more.
fn key_der(text: &str) -> Option<Vec<u8>> {
    let body: String = text
        .split("-----")
        .filter(|part| !part.contains("PRIVATE KEY"))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect();
    let decoded = STANDARD.decode(body).ok()?;
    match std::str::from_utf8(&decoded) {
        Ok(inner) if inner.contains("-----BEGIN") => key_der(inner),
        _ => Some(decoded),
    }
}

/// A DER length: one byte up to 127, and above that a count of the bytes that
/// follow. A 2048-bit key is already past the short form.
fn der_length(length: usize) -> Vec<u8> {
    if length < 0x80 {
        return vec![length as u8];
    }
    let bytes = length.to_be_bytes();
    let significant = &bytes[bytes.iter().take_while(|byte| **byte == 0).count()..];
    let mut encoded = vec![0x80 | significant.len() as u8];
    encoded.extend_from_slice(significant);
    encoded
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut encoded = vec![tag];
    encoded.extend(der_length(content.len()));
    encoded.extend_from_slice(content);
    encoded
}

/// `SHA256:` and the digest of the public key, which is how the server finds
/// the key the token claims to be signed with.
///
/// The server hashes a SubjectPublicKeyInfo, and `ring` hands out the bare
/// PKCS#1 key inside one -- so the wrapper is rebuilt here: the RSA algorithm
/// identifier, then the key as a bit string with no unused bits.
fn fingerprint(key: &RsaKeyPair) -> String {
    // OID 1.2.840.113549.1.1.1 (rsaEncryption) with its NULL parameters.
    const RSA_ALGORITHM: [u8; 15] = [
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let mut bits = vec![0];
    bits.extend_from_slice(key.public_key().as_ref());
    let mut info = RSA_ALGORITHM.to_vec();
    info.extend(der(0x03, &bits));

    let digest = ring::digest::digest(&ring::digest::SHA256, &der(0x30, &info));
    format!("SHA256:{}", STANDARD.encode(digest))
}

/// The account as a token names it: upper case, and without the region a
/// legacy locator carries after its first dot.
fn token_account(account: &str) -> String {
    account
        .split('.')
        .next()
        .unwrap_or(account)
        .to_ascii_uppercase()
}

/// A key-pair token for one request, valid from `now`.
///
/// Minted per request rather than cached: a signature costs about a
/// millisecond, and a cache is a token that expires mid-poll on the one day
/// the clock was adjusted. `now` is a parameter so a test can name the instant.
fn token(config: &SnowflakeConfig, now: u64) -> Result<String, DbError> {
    let key = key_pair(config)?;
    let subject = format!(
        "{}.{}",
        token_account(&config.account),
        config.user.to_ascii_uppercase()
    );
    let claims = json!({
        "iss": format!("{subject}.{}", fingerprint(&key)),
        "sub": subject,
        "iat": now,
        "exp": now + TOKEN_LIFETIME,
    });

    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|error| plain_error(format!("The token was not signed: {error}.")))?;

    Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// One column of a result, as the API describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
struct RowType {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    precision: Option<u32>,
    #[serde(default)]
    scale: Option<u32>,
}

/// The type the way someone writing Snowflake SQL spells it, since the wire
/// names are the storage classes behind them: every integer and decimal is
/// `fixed`, every string `text`.
fn data_type(row_type: &RowType) -> String {
    match row_type.kind.as_str() {
        "fixed" => match (row_type.precision, row_type.scale) {
            (Some(precision), Some(scale)) => format!("number({precision},{scale})"),
            _ => "number".to_string(),
        },
        "real" => "float".to_string(),
        "text" => "varchar".to_string(),
        other => other.to_string(),
    }
}

/// Days since 1970-01-01 as a calendar date, by Howard Hinnant's
/// `civil_from_days`. Written out because `time` and `chrono` are both only
/// transitive here, and neither is worth naming for one conversion.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// `seconds.fraction` as whole nanoseconds. Read as decimal text rather than
/// through an `f64`, which holds about sixteen digits and an epoch with nine
/// fractional ones needs nineteen.
fn nanoseconds(value: &str) -> Option<i128> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if fraction.len() > 9 {
        return None;
    }
    let whole: i128 = whole.parse().ok()?;
    let fraction: i128 = format!("{fraction:0<9}").parse().ok()?;
    let magnitude = whole * 1_000_000_000 + fraction;
    Some(if negative { -magnitude } else { magnitude })
}

/// `HH:MM:SS` and as many fractional digits as the column declares.
fn clock(nanos_of_day: i128, scale: u32) -> String {
    let seconds = nanos_of_day / 1_000_000_000;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    if scale > 0 {
        let fraction = format!("{:09}", nanos_of_day % 1_000_000_000);
        text.push('.');
        text.push_str(&fraction[..scale.min(9) as usize]);
    }
    text
}

fn date_and_clock(nanos: i128, scale: u32) -> String {
    const DAY: i128 = 86_400 * 1_000_000_000;
    let (year, month, day) = civil_date(nanos.div_euclid(DAY) as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {}",
        clock(nanos.rem_euclid(DAY), scale)
    )
}

/// A cell as text a person can read.
///
/// Most values arrive that way already. The temporal ones arrive as counts
/// from the epoch whatever output format the session asks for, so they are
/// rendered here -- the grid is never shown a number of days and left to guess
/// (hard rule 4). A value that is not the shape its type promises is passed
/// through as it came rather than dropped: wrong-looking beats missing.
fn render(value: &str, row_type: &RowType) -> String {
    let scale = row_type.scale.unwrap_or(9);
    let rendered = match row_type.kind.as_str() {
        "date" => value.parse().ok().map(|days| {
            let (year, month, day) = civil_date(days);
            format!("{year:04}-{month:02}-{day:02}")
        }),
        "time" => nanoseconds(value).map(|nanos| clock(nanos, scale)),
        "timestamp_ntz" => nanoseconds(value).map(|nanos| date_and_clock(nanos, scale)),
        // An instant, and the API carries no session time zone to show it in,
        // so it is shown in the one zone that needs none and says so.
        "timestamp_ltz" => {
            nanoseconds(value).map(|nanos| format!("{}Z", date_and_clock(nanos, scale)))
        }
        // The instant in UTC, then the zone's offset in minutes, biased by a
        // day so that it is never negative on the wire.
        "timestamp_tz" => value.split_once(' ').and_then(|(instant, offset)| {
            let offset = offset.parse::<i128>().ok()? - 1_440;
            let local = nanoseconds(instant)? + offset * 60 * 1_000_000_000;
            Some(format!(
                "{} {}{:02}:{:02}",
                date_and_clock(local, scale),
                if offset < 0 { '-' } else { '+' },
                offset.abs() / 60,
                offset.abs() % 60
            ))
        }),
        _ => None,
    };
    rendered.unwrap_or_else(|| value.to_string())
}

/// What one request holds besides the statement. Blank fields are left out
/// rather than sent empty, so the user's own defaults stay in force.
///
/// Everything here is a field of the request and none of it is SQL: the
/// statement goes out exactly as it was written (hard rule 1).
fn request_body(config: &SnowflakeConfig, sql: &str) -> Value {
    let mut body = json!({
        "statement": sql,
        "database": config.database,
        // Without this the API refuses any submission holding more than one
        // statement, which every other engine here accepts. Zero is "however
        // many there are".
        "parameters": { "MULTI_STATEMENT_COUNT": "0" },
    });
    for (field, value) in [("warehouse", &config.warehouse), ("role", &config.role)] {
        if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
            body[field] = json!(value);
        }
    }
    if config.statement_timeout > 0 {
        body["timeout"] = json!(config.statement_timeout);
    }
    body
}

/// Whether a submission has to be stoppable from the moment it is sent, which
/// is what the extra round trip of an asynchronous submit is for.
/// `Yes` carries the run it is stoppable under.
#[derive(Clone, Copy)]
enum Cancellable<'a> {
    Yes(&'a CancelToken),
    No,
}

/// What a status code and its body add up to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reply {
    /// Accepted and still going: ask again.
    Running,
    Finished,
}

/// The server's own words where it sent any, and the status where it did not
/// -- a proxy's HTML error page has no `message` to quote (hard rule 6).
fn reply(host: &str, status: u16, body: &Value) -> Result<Reply, DbError> {
    match status {
        200 => Ok(Reply::Finished),
        202 => Ok(Reply::Running),
        _ => Err(refusal(host, status, body)),
    }
}

fn refusal(host: &str, status: u16, body: &Value) -> DbError {
    plain_error(match body["message"].as_str() {
        Some(message) => message.to_string(),
        None => format!("{host} answered HTTP {status}."),
    })
}

/// Throttling, or a server or gateway that failed this once.
fn transient(status: u16) -> bool {
    status == 429 || status >= 500
}

/// The pause before the first retry of a [`Connection::fetch`], doubling for
/// each of [`RETRIES`]: seven and a half seconds in all.
const RETRY_PAUSE: Duration = if cfg!(test) {
    Duration::from_millis(1)
} else {
    Duration::from_millis(500)
};
const RETRIES: u32 = 4;

/// The most one response body may take to arrive, once its headers have. A
/// body that stops arriving would otherwise hang the query past the reach of
/// Cancel. ponytail: a total rather than an idle bound, which ureq does not
/// offer; ten minutes is a compressed partition over a slow link many times
/// over.
const RECV_BODY: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(600)
};

/// The statement whose rows a finished response stands for.
///
/// A submission of several statements answers with a handle per statement and
/// a placeholder result of its own; the one shown is the last, as it is for a
/// multi-statement submission on every other engine.
fn last_child(body: &Value) -> Option<&str> {
    body["statementHandles"].as_array()?.last()?.as_str()
}

fn row_types(body: &Value) -> Result<Vec<RowType>, DbError> {
    serde_json::from_value(body["resultSetMetaData"]["rowType"].clone()).map_err(|error| {
        plain_error(format!(
            "The result's column description was not understood: {error}."
        ))
    })
}

/// A response with no partition list has the one it arrived in.
fn partition_count(body: &Value) -> usize {
    body["resultSetMetaData"]["partitionInfo"]
        .as_array()
        .map_or(1, Vec::len)
}

/// One partition's rows, rendered. A JSON `null` is SQL NULL and stays
/// distinct from the empty string.
fn rows(body: &Value, row_types: &[RowType]) -> Vec<Vec<Cell>> {
    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    data.iter()
        .filter_map(Value::as_array)
        .map(|row| {
            row.iter()
                .zip(row_types)
                .map(|(cell, row_type)| cell.as_str().map(|value| render(value, row_type)))
                .collect()
        })
        .collect()
}

/// The rows a write touched, where the response counts them. A `SELECT`
/// carries no such counts and answers `None`.
fn rows_affected(body: &Value) -> Option<u64> {
    let stats = body["stats"].as_object()?;
    Some(
        [
            "numRowsInserted",
            "numRowsUpdated",
            "numRowsDeleted",
            "numDuplicateRowsUpdated",
        ]
        .iter()
        .filter_map(|field| stats.get(*field)?.as_u64())
        .sum(),
    )
}

/// A connection in name only: there is no socket to keep, so this is the
/// profile's settings and a client.
///
/// No mutex around it, unlike its three siblings, because there is nothing to
/// serialise -- a catalog load does not queue behind a slow query here.
#[derive(Clone)]
pub struct Connection {
    config: Arc<SnowflakeConfig>,
    agent: ureq::Agent,
}

/// Takes a handle back out of its run's list however `query` leaves.
///
/// A [`CancelToken`] here holds the handles of the statements submitted under
/// it and not yet finished, which is what [`Connection::cancel`] stops. It is
/// locked for a push, a removal or a clone and never across a request, so a
/// cancel waits on nothing a query holds.
struct RunningGuard<'a> {
    running: &'a Mutex<Cancelling>,
    handle: String,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            running.handles.retain(|handle| *handle != self.handle);
        }
    }
}

impl Connection {
    pub fn open(config: &SnowflakeConfig) -> Result<Self, DbError> {
        let agent = ureq::Agent::config_builder()
            // A 422 is the server explaining a SQL error, and the explanation
            // is in the body ureq would otherwise discard.
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            // Bounds a server that accepts and then says nothing. Not a bound
            // on the statement: that is polled, a short request at a time.
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(RECV_BODY))
            .user_agent(concat!("dbdelve/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let connection = Self {
            config: Arc::new(config.clone()),
            agent,
        };
        // Connecting is the connection test. This needs no warehouse, so it
        // proves the key, the account and the network without starting one.
        connection.query("SELECT CURRENT_VERSION()")?;
        Ok(connection)
    }

    fn url(&self, path: &str) -> String {
        let host = self.config.host();
        // The offline tests' mock speaks plain HTTP on loopback. Only a test
        // build reads a scheme out of the host; anywhere else one is part of
        // an unreachable name, so a profile cannot opt out of TLS.
        #[cfg(test)]
        if host.starts_with("http://") {
            return format!("{host}/api/v2/statements{path}");
        }
        format!("https://{host}/api/v2/statements{path}")
    }

    /// One exchange: a fresh token, the request, and the body as JSON whatever
    /// the status was.
    fn exchange(&self, url: &str, body: Option<&Value>) -> Result<(u16, Value), DbError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let authorization = format!("Bearer {}", token(&self.config, now)?);
        let headers = [
            ("Authorization", authorization.as_str()),
            ("X-Snowflake-Authorization-Token-Type", "KEYPAIR_JWT"),
            ("Accept", "application/json"),
        ];

        let sent = match body {
            Some(body) => {
                let mut request = self.agent.post(url);
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                request.send_json(body)
            }
            None => {
                let mut request = self.agent.get(url);
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                request.call()
            }
        };
        let host = self.config.host();
        let mut response =
            sent.map_err(|error| plain_error(format!("{host} was not reached: {error}.")))?;
        let status = response.status().as_u16();
        // No size limit: a partition is as large as the server made it, and
        // the default cap is smaller than one.
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|error| {
                plain_error(format!(
                    "The answer from {host} was not read whole: {error}."
                ))
            })?;
        let body = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            // A proxy's error page is not JSON, and its status is the message.
            Err(_) if !matches!(status, 200 | 202) => Value::Null,
            Err(error) => {
                return Err(plain_error(format!(
                    "The answer from {host} was not understood: {error}."
                )));
            }
        };
        Ok((status, body))
    }

    /// A GET, asked again while its failure is one a moment might fix. Only
    /// reads are repeated: a submit that failed may still have reached the
    /// server, and sending it again could run the statement twice.
    fn fetch(&self, url: &str) -> Result<(u16, Value), DbError> {
        let mut pause = RETRY_PAUSE;
        for _ in 0..RETRIES {
            match self.exchange(url, None) {
                Ok((status, _)) if transient(status) => {}
                Err(_) => {}
                answer => return answer,
            }
            std::thread::sleep(pause);
            pause *= 2;
        }
        self.exchange(url, None)
    }

    /// Ask the server to stop one statement.
    fn stop(&self, handle: &str) -> Result<(), DbError> {
        let url = self.url(&format!("/{handle}/cancel"));
        let (status, body) = self.exchange(&url, Some(&json!({})))?;
        reply(&self.config.host(), status, &body).map(|_| ())
    }

    /// A poll that gave up leaves the statement running where nobody is
    /// watching it, free to commit a write the user will run again. So it is
    /// stopped on the way out, and the error says whether that worked.
    fn abandon(&self, handle: &str, error: DbError) -> DbError {
        let outcome = match self.stop(handle) {
            Ok(()) => "It was asked to stop rather than left running unseen.".to_string(),
            Err(stop) => format!(
                "It may still be running: asking it to stop failed too ({}).",
                stop.message
            ),
        };
        plain_error(format!("{} {outcome}", error.message))
    }

    /// Run one submission verbatim and return its last result.
    ///
    /// Submitted asynchronously, which costs a round trip: a synchronous submit
    /// does not give up its handle until the statement ends or 45 seconds pass,
    /// and the handle is what Cancel needs from the first moment. Measured at
    /// 885ms against 315ms for three `SELECT 1`s, and that is the price of the
    /// button working on the statement that needs it.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.query_with(sql, &CancelToken::default())
    }

    /// The same, stoppable by a [`Connection::cancel`] given `cancel`.
    pub fn query_with(&self, sql: &str, cancel: &CancelToken) -> Result<QueryResult, DbError> {
        self.submit(sql, Cancellable::Yes(cancel))
    }

    /// The same, for a statement dbdelve wrote and the user cannot see.
    ///
    /// Submitted synchronously: nothing offers to cancel a catalog load, and
    /// the handle a cancel would need is the only thing the extra round trip
    /// buys. A structure load is four of these, so it is most of a second each
    /// time a relation is opened.
    fn internal_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        self.submit(sql, Cancellable::No)
    }

    fn submit(&self, sql: &str, cancellable: Cancellable) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let host = self.config.host();
        let body = request_body(&self.config, sql);

        // A synchronous submit answers with the result itself, and only hands
        // back a handle when the statement outlives the API's own 45-second
        // window -- so both paths have to be read here, whichever was asked for.
        let url = match cancellable {
            Cancellable::Yes(_) => self.url("?async=true"),
            Cancellable::No => self.url(""),
        };
        let (status, accepted) = self.exchange(&url, Some(&body))?;
        let mut answer = reply(&host, status, &accepted)?;
        let handle = accepted["statementHandle"].as_str().unwrap_or_default();
        // Nothing to cancel once the statement is over, and a synchronous
        // submit that finished is over.
        let handle = match handle.is_empty() {
            true if answer == Reply::Running => {
                return Err(plain_error(format!(
                    "{host} accepted a statement and named no handle."
                )));
            }
            _ => handle.to_string(),
        };
        let _running = match cancellable {
            Cancellable::Yes(CancelToken(running)) if answer == Reply::Running => {
                let asked = running.lock().is_ok_and(|mut running| {
                    running.handles.push(handle.clone());
                    running.asked
                });
                let guard = RunningGuard {
                    running,
                    handle: handle.clone(),
                };
                // Cancel was pressed while the submit was on its way, found no
                // handle and answered that nothing was running; the button is
                // spent, so it is carried out here.
                if asked && let Err(error) = self.stop(&handle) {
                    return Err(plain_error(format!(
                        "Cancel came before {host} named the statement, and stopping it \
                         once named failed ({}). It may still be running.",
                        error.message
                    )));
                }
                Some(guard)
            }
            _ => None,
        };

        let mut finished = accepted;
        let mut pause = Duration::from_millis(100);
        while answer == Reply::Running {
            let (status, body) = match self.fetch(&self.url(&format!("/{handle}"))) {
                Ok((status, body)) if transient(status) => {
                    return Err(self.abandon(&handle, refusal(&host, status, &body)));
                }
                Err(error) => return Err(self.abandon(&handle, error)),
                Ok(polled) => polled,
            };
            answer = reply(&host, status, &body)?;
            finished = body;
            if answer == Reply::Running {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_secs(2));
            }
        }

        let mut handle = handle;
        if let Some(child) = last_child(&finished).map(str::to_string) {
            let (status, body) = self.fetch(&self.url(&format!("/{child}")))?;
            reply(&host, status, &body)?;
            finished = body;
            handle = child;
        }

        let row_types = row_types(&finished)?;
        let mut result = QueryResult {
            columns: row_types
                .iter()
                .map(|row_type| Column {
                    name: row_type.name.clone(),
                    data_type: Some(data_type(row_type)),
                })
                .collect(),
            rows: rows(&finished, &row_types),
            rows_affected: rows_affected(&finished),
            ..Default::default()
        };
        // The whole result is fetched before any of it is shown, which is the
        // ceiling the Postgres path has too.
        for partition in 1..partition_count(&finished) {
            let (status, body) =
                self.fetch(&self.url(&format!("/{handle}?partition={partition}")))?;
            reply(&host, status, &body)?;
            result.rows.extend(rows(&body, &row_types));
        }

        result.bytes = result
            .rows
            .iter()
            .flatten()
            .flatten()
            .map(String::len)
            .sum();
        result.elapsed = started.elapsed();
        Ok(result)
    }

    /// Stop the statements running under `cancel`, and nobody else's: the
    /// connection is shared by every tab and by the catalog loads.
    ///
    /// The running statement ends as an ordinary error out of `query`, in the
    /// server's words. Nothing in flight is not an error, and a statement still
    /// waiting for its handle is stopped when the handle arrives.
    pub fn cancel(&self, cancel: &CancelToken) -> Result<(), DbError> {
        let handles = cancel
            .0
            .lock()
            .map(|mut running| {
                running.asked = true;
                running.handles.clone()
            })
            .unwrap_or_default();
        // Every one is asked, and the first refusal reported after: one that
        // failed is no reason to leave the others running.
        handles
            .iter()
            .map(|handle| self.stop(handle))
            .fold(Ok(()), Result::and)
    }

    /// Read through `INFORMATION_SCHEMA`, which needs a running warehouse --
    /// so connecting resumes one that was suspended, and so does opening a
    /// Structure tab. That is the price of the idiomatic catalog, and it is
    /// the server's message the user sees when there is no warehouse to run.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        assemble_catalog(self.internal_query(RELATIONS_SQL)?, QueryResult::default())
    }

    /// The slow half here, and by a distance: `INFORMATION_SCHEMA.FUNCTIONS`
    /// and `PROCEDURES` took four and eight seconds to report that this
    /// database had neither, where the relations were in hand after two. It is
    /// asked for on its own so the explorer does not wait on it, and the two
    /// views are asked separately so it costs the slower rather than both.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        let [functions, procedures] =
            self.at_once([FUNCTIONS_SQL.into(), PROCEDURES_SQL.into()])?;
        assemble_catalog(QueryResult::default(), appended(functions, procedures)?)
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let literal = |value: &str| Engine::Snowflake.quote_literal(value);

        // `INFORMATION_SCHEMA` names a constraint and its type but has no view
        // of the columns in it, so the keys come from `SHOW`.
        //
        // ponytail: asked of the schema and narrowed here rather than asked of
        // the relation, because `IN TABLE` is an error for a view and this has
        // to answer for any relation. `SHOW` stops at 10 000 rows, so the
        // ceiling is a schema with more key columns than that, where some keys
        // go unlisted; the upgrade path is asking the catalog for the kind and
        // using `IN TABLE` for tables.
        //
        // Named from the database down: a `SHOW` does not resolve a schema
        // against the request's `database` the way a query does, and refuses
        // one written without it.
        let database = self.config.stored_database();
        let within = Engine::Snowflake.qualified(&database, schema);
        let [columns, primary, unique, imported] = self.at_once([
            COLUMNS_SQL
                .replace("{schema}", &literal(schema))
                .replace("{relation}", &literal(relation)),
            format!("SHOW PRIMARY KEYS IN SCHEMA {within}"),
            format!("SHOW UNIQUE KEYS IN SCHEMA {within}"),
            format!("SHOW IMPORTED KEYS IN SCHEMA {within}"),
        ])?;

        let mut structure =
            assemble_structure(columns, QueryResult::default(), QueryResult::default())?;
        structure.constraints = [
            key_definitions(
                &primary,
                "table_name",
                relation,
                "constraint_name",
                "PRIMARY KEY",
            ),
            key_definitions(&unique, "table_name", relation, "constraint_name", "UNIQUE"),
            key_definitions(
                &imported,
                "fk_table_name",
                relation,
                "fk_name",
                "FOREIGN KEY",
            ),
        ]
        .concat();
        structure.foreign_keys = foreign_keys(&imported, relation, &database);
        Ok(structure)
    }

    /// Run statements dbdelve wrote all at the same time, in their own order.
    ///
    /// One thread each, because there is no connection to serialise them on:
    /// this engine is an HTTP client and each statement is its own request. It
    /// is what keeps a structure load at the cost of its slowest statement
    /// rather than the sum of four, measured at 1.3s against 3.6s.
    ///
    /// The first error wins, and by position rather than by whichever thread
    /// failed first, so the same broken catalog always reports the same way.
    fn at_once<const N: usize>(
        &self,
        statements: [String; N],
    ) -> Result<[QueryResult; N], DbError> {
        let mut results: [Result<QueryResult, DbError>; N] =
            std::array::from_fn(|_| Ok(QueryResult::default()));
        std::thread::scope(|scope| {
            let mut threads = Vec::with_capacity(N);
            for sql in &statements {
                let connection = self.clone();
                threads.push(scope.spawn(move || connection.internal_query(sql)));
            }
            for (slot, thread) in results.iter_mut().zip(threads) {
                *slot = thread.join().unwrap_or_else(|_| {
                    Err(plain_error("A catalog query did not finish.".to_string()))
                });
            }
        });

        let mut done = Vec::with_capacity(N);
        for result in results {
            done.push(result?);
        }
        Ok(done.try_into().unwrap_or_else(|_| unreachable!()))
    }
}

/// Aliased in double quotes throughout: an unquoted alias comes back folded to
/// upper case, and the assemblers look these names up exactly.
const RELATIONS_SQL: &str = r#"
SELECT TABLE_SCHEMA AS "schema_name",
       TABLE_NAME AS "relation_name",
       CASE TABLE_TYPE
           WHEN 'VIEW' THEN 'view'
           WHEN 'MATERIALIZED VIEW' THEN 'materialized_view'
           WHEN 'EXTERNAL TABLE' THEN 'foreign_table'
           -- Temporary, transient, dynamic, event, hybrid and Iceberg tables
           -- are all browsed the way a table is.
           ELSE 'table'
       END AS "relation_kind"
FROM INFORMATION_SCHEMA.TABLES
WHERE TABLE_SCHEMA <> 'INFORMATION_SCHEMA'
ORDER BY 1, 2"#;

const FUNCTIONS_SQL: &str = r#"
SELECT FUNCTION_SCHEMA AS "schema_name",
       FUNCTION_NAME AS "routine_name",
       'function' AS "routine_kind",
       COALESCE(ARGUMENT_SIGNATURE, '') AS "identity_arguments",
       COALESCE(DATA_TYPE, '') AS "result_type",
       COALESCE(FUNCTION_LANGUAGE, '') AS "language",
       COALESCE(FUNCTION_DEFINITION, '') AS "definition"
FROM INFORMATION_SCHEMA.FUNCTIONS
ORDER BY 1, 2"#;

const PROCEDURES_SQL: &str = r#"
SELECT PROCEDURE_SCHEMA AS "schema_name",
       PROCEDURE_NAME AS "routine_name",
       'procedure' AS "routine_kind",
       COALESCE(ARGUMENT_SIGNATURE, '') AS "identity_arguments",
       COALESCE(DATA_TYPE, '') AS "result_type",
       COALESCE(PROCEDURE_LANGUAGE, '') AS "language",
       COALESCE(PROCEDURE_DEFINITION, '') AS "definition"
FROM INFORMATION_SCHEMA.PROCEDURES
ORDER BY 1, 2"#;

/// Two results of the same shape as one, for an assembler that takes one.
///
/// The columns are compared rather than assumed: the two are separate
/// statements, and a row read under the wrong header is worse than an error.
fn appended(mut first: QueryResult, second: QueryResult) -> Result<QueryResult, DbError> {
    if first.columns != second.columns {
        return Err(plain_error(
            "Two catalog queries answered with different columns.".to_string(),
        ));
    }
    first.rows.extend(second.rows);
    Ok(first)
}

/// The type is spelled the way a result column's is, lower case with its
/// precision, so a relation reads the same in its Structure tab and its grid.
const COLUMNS_SQL: &str = r#"
SELECT COLUMN_NAME AS "column_name",
       LOWER(CASE
           WHEN DATA_TYPE = 'NUMBER'
               THEN 'NUMBER(' || NUMERIC_PRECISION || ',' || NUMERIC_SCALE || ')'
           -- A VARCHAR declared without a length reports the most the account
           -- allows: 16 MB, or 128 MB on a newer one. Nobody chose that number,
           -- and repeated down a column list it buries the ones somebody did.
           WHEN DATA_TYPE = 'TEXT' AND CHARACTER_MAXIMUM_LENGTH IN (16777216, 134217728)
               THEN 'VARCHAR'
           WHEN DATA_TYPE = 'TEXT'
               THEN 'VARCHAR(' || CHARACTER_MAXIMUM_LENGTH || ')'
           ELSE DATA_TYPE
       END) AS "data_type",
       LOWER(IS_NULLABLE) AS "nullable",
       COALESCE(COLUMN_DEFAULT, '') AS "column_default"
FROM INFORMATION_SCHEMA.COLUMNS
WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {relation}
ORDER BY ORDINAL_POSITION"#;

/// A cell of a `SHOW` result by its column's fixed name.
fn shown<'a>(result: &'a QueryResult, row: &'a [Cell], name: &str) -> Option<&'a str> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)?;
    row.get(index)?.as_deref()
}

/// The rows of a key `SHOW` that belong to `relation`, in key order.
fn key_rows<'a>(result: &'a QueryResult, table_column: &str, relation: &str) -> Vec<&'a Vec<Cell>> {
    let mut rows: Vec<_> = result
        .rows
        .iter()
        .filter(|row| shown(result, row, table_column) == Some(relation))
        .collect();
    rows.sort_by_key(|row| {
        shown(result, row, "key_sequence")
            .and_then(|sequence| sequence.parse::<u32>().ok())
            .unwrap_or(0)
    });
    rows
}

/// One definition per constraint, its columns in key order -- a composite key
/// arrives as a row per column and is one constraint.
fn key_definitions(
    result: &QueryResult,
    table_column: &str,
    relation: &str,
    name_column: &str,
    keyword: &str,
) -> Vec<NamedDefinition> {
    let quote = |name: &str| Engine::Snowflake.quote_identifier(name);
    let foreign = keyword == "FOREIGN KEY";
    let column = if foreign {
        "fk_column_name"
    } else {
        "column_name"
    };

    let mut constraints = std::collections::BTreeMap::<&str, Vec<&Vec<Cell>>>::new();
    for row in key_rows(result, table_column, relation) {
        let name = shown(result, row, name_column).unwrap_or_default();
        constraints.entry(name).or_default().push(row);
    }

    constraints
        .into_iter()
        .map(|(name, rows)| {
            let list = |column: &str| {
                rows.iter()
                    .filter_map(|row| shown(result, row, column))
                    .map(quote)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut definition = format!("{keyword} ({})", list(column));
            if foreign && let Some(first) = rows.first() {
                definition.push_str(&format!(
                    " REFERENCES {} ({})",
                    Engine::Snowflake.qualified(
                        shown(result, first, "pk_schema_name").unwrap_or_default(),
                        shown(result, first, "pk_table_name").unwrap_or_default(),
                    ),
                    list("pk_column_name")
                ));
            }
            NamedDefinition {
                name: name.to_string(),
                definition,
            }
        })
        .collect()
}

/// The keys that can be followed. One that points into another database is
/// left out: a [`ForeignKey`] names a schema and a table, a profile is bound to
/// one database, and following it would filter a table of the same name here.
fn foreign_keys(imported: &QueryResult, relation: &str, database: &str) -> Vec<ForeignKey> {
    key_rows(imported, "fk_table_name", relation)
        .into_iter()
        .filter(|row| shown(imported, row, "pk_database_name") == Some(database))
        .filter_map(|row| {
            Some(ForeignKey {
                column: shown(imported, row, "fk_column_name")?.to_string(),
                referenced_schema: shown(imported, row, "pk_schema_name")?.to_string(),
                referenced_table: shown(imported, row, "pk_table_name")?.to_string(),
                referenced_column: shown(imported, row, "pk_column_name")?.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway keys that protect nothing, generated for these tests with
    /// `openssl genpkey -algorithm RSA`.
    fn test_key(name: &str) -> String {
        format!("{}/dev/snowflake/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn config(key: &str) -> SnowflakeConfig {
        SnowflakeConfig {
            account: "myorg-myaccount".into(),
            user: "tim".into(),
            private_key: test_key(key),
            ..Default::default()
        }
    }

    #[test]
    fn the_fingerprint_is_the_one_openssl_computes() {
        // The expected values are from
        //   openssl rsa -in KEY -pubout -outform DER \
        //     | openssl dgst -sha256 -binary | openssl enc -base64
        // which is the command Snowflake's own documentation gives, so this is
        // checked against the server's arithmetic and not against our own.
        let key = key_pair(&config("test-key-2048.p8")).expect("the key loads");
        assert_eq!(
            fingerprint(&key),
            "SHA256:4/76NAyPR/D6nlGOKDw+h7DNn+cNUUuXMPNDC7pyVXs="
        );
        // Twice the size pushes both DER lengths past 255, into two bytes.
        let key = key_pair(&config("test-key-4096.p8")).expect("the key loads");
        assert_eq!(
            fingerprint(&key),
            "SHA256:cGqHm+uApyCk8eJ2AGO6ZdmR9wGFHHouRS3w/eFRGgQ="
        );
    }

    #[test]
    fn a_der_length_takes_the_long_form_past_127() {
        assert_eq!(der_length(0x7f), [0x7f]);
        assert_eq!(der_length(0x80), [0x81, 0x80]);
        assert_eq!(der_length(0x0126), [0x82, 0x01, 0x26]);
    }

    fn claims(token: &str) -> serde_json::Value {
        let payload = token.split('.').nth(1).expect("three parts");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url"))
            .expect("claims are JSON")
    }

    #[test]
    fn the_claims_name_the_account_and_user_in_upper_case() {
        let token = token(&config("test-key-2048.p8"), 1_700_000_000).expect("signed");
        let claims = claims(&token);
        assert_eq!(claims["sub"], "MYORG-MYACCOUNT.TIM");
        assert_eq!(
            claims["iss"],
            "MYORG-MYACCOUNT.TIM.SHA256:4/76NAyPR/D6nlGOKDw+h7DNn+cNUUuXMPNDC7pyVXs="
        );
        assert_eq!(claims["iat"], 1_700_000_000_u64);
        assert_eq!(claims["exp"], 1_700_000_000_u64 + 59 * 60);
    }

    #[test]
    fn a_legacy_locator_loses_its_region_in_the_token() {
        // `xy12345.eu-central-1` is a host prefix; the account it names is the
        // part before the dot, and a token naming the whole of it is refused.
        let mut config = config("test-key-2048.p8");
        config.account = "xy12345.eu-central-1".into();
        let token = token(&config, 0).expect("signed");
        assert_eq!(claims(&token)["sub"], "XY12345.TIM");
    }

    #[test]
    fn the_signature_verifies_against_the_public_key() {
        let token = token(&config("test-key-2048.p8"), 0).expect("signed");
        let (message, signature) = token.rsplit_once('.').expect("three parts");
        let key = key_pair(&config("test-key-2048.p8")).expect("the key loads");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            key.public_key().as_ref(),
        )
        .verify(
            message.as_bytes(),
            &URL_SAFE_NO_PAD.decode(signature).expect("base64url"),
        )
        .expect("the signature is over the header and claims");
    }

    #[test]
    fn an_encrypted_key_is_refused_by_name() {
        let error = token(&config("test-key-encrypted.p8"), 0).expect_err("refused");
        assert!(error.message.ends_with("is encrypted."), "{error}");
        assert!(error.message.contains("test-key-encrypted.p8"), "{error}");
    }

    #[test]
    fn a_missing_key_names_its_path() {
        let error = token(&config("no-such-key.p8"), 0).expect_err("refused");
        assert!(error.message.contains("no-such-key.p8"), "{error}");
    }

    fn column(kind: &str, scale: Option<u32>) -> RowType {
        RowType {
            name: "c".into(),
            kind: kind.into(),
            precision: None,
            scale,
        }
    }

    #[test]
    fn a_date_is_days_either_side_of_the_epoch() {
        let date = column("date", None);
        assert_eq!(render("0", &date), "1970-01-01");
        assert_eq!(render("-1", &date), "1969-12-31");
        assert_eq!(render("19000", &date), "2022-01-08");
        // A leap day, and the day the 400-year rule keeps.
        assert_eq!(render("19782", &date), "2024-02-29");
        assert_eq!(render("11016", &date), "2000-02-29");
    }

    #[test]
    fn a_time_keeps_as_many_digits_as_its_column_declares() {
        assert_eq!(
            render("82919.123456789", &column("time", Some(9))),
            "23:01:59.123456789"
        );
        assert_eq!(
            render("82919.123000000", &column("time", Some(3))),
            "23:01:59.123"
        );
        assert_eq!(
            render("82919.000000000", &column("time", Some(0))),
            "23:01:59"
        );
    }

    #[test]
    fn a_timestamp_without_a_zone_renders_without_one() {
        assert_eq!(
            render("1616173619.000000000", &column("timestamp_ntz", Some(0))),
            "2021-03-19 17:06:59"
        );
        assert_eq!(
            render("1616173619.250000000", &column("timestamp_ltz", Some(3))),
            "2021-03-19 17:06:59.250Z"
        );
    }

    #[test]
    fn an_instant_before_the_epoch_keeps_its_fraction_the_right_way_round() {
        // Half a second before midnight, not half a second after the second
        // before it: the fraction of a negative count points backwards.
        assert_eq!(
            render("-0.500000000", &column("timestamp_ntz", Some(3))),
            "1969-12-31 23:59:59.500"
        );
        assert_eq!(
            render("-86400.000000000", &column("timestamp_ntz", Some(0))),
            "1969-12-31 00:00:00"
        );
    }

    #[test]
    fn a_zoned_timestamp_is_shown_at_its_own_offset() {
        // 1440 is the bias, so 1560 is +02:00 and 1140 is -05:00.
        let zoned = column("timestamp_tz", Some(0));
        assert_eq!(
            render("1616173619.000000000 1560", &zoned),
            "2021-03-19 19:06:59 +02:00"
        );
        assert_eq!(
            render("1616173619.000000000 1140", &zoned),
            "2021-03-19 12:06:59 -05:00"
        );
        assert_eq!(
            render("1616173619.000000000 1770", &zoned),
            "2021-03-19 22:36:59 +05:30"
        );
    }

    #[test]
    fn a_value_that_is_not_the_promised_shape_is_shown_as_it_came() {
        assert_eq!(render("not-a-day", &column("date", None)), "not-a-day");
        assert_eq!(render("12.5", &column("fixed", Some(1))), "12.5");
        assert_eq!(render("{\"a\":1}", &column("variant", None)), "{\"a\":1}");
    }

    #[test]
    fn a_type_is_spelled_the_way_its_sql_spells_it() {
        let fixed = RowType {
            precision: Some(38),
            scale: Some(0),
            ..column("fixed", None)
        };
        assert_eq!(data_type(&fixed), "number(38,0)");
        assert_eq!(data_type(&column("real", None)), "float");
        assert_eq!(data_type(&column("text", None)), "varchar");
        assert_eq!(data_type(&column("timestamp_tz", Some(9))), "timestamp_tz");
        // The grid right-aligns on these names, so they have to be ones
        // `is_numeric_type` already answers to.
        assert!(super::super::is_numeric_type(&data_type(&fixed)));
        assert!(super::super::is_numeric_type("float"));
    }

    fn body(text: &str) -> Value {
        serde_json::from_str(text).expect("the fixture is JSON")
    }

    #[test]
    fn the_statement_is_sent_exactly_as_written() {
        // Odd spacing, a trailing comment and no semicolon: none of it is
        // dbdelve's to tidy.
        let sql = "select  1 -- one\n  ,2";
        assert_eq!(request_body(&config("k"), sql)["statement"], sql);
    }

    #[test]
    fn a_request_leaves_out_what_the_profile_left_blank() {
        let mut config = config("k");
        config.database = "ANALYTICS".into();
        config.warehouse = Some(String::new());
        let blank = request_body(&config, "select 1");
        assert_eq!(blank["database"], "ANALYTICS");
        assert_eq!(blank["parameters"]["MULTI_STATEMENT_COUNT"], "0");
        for absent in ["warehouse", "role", "timeout"] {
            assert!(blank.get(absent).is_none(), "{absent} was sent");
        }

        config.warehouse = Some("COMPUTE_WH".into());
        config.role = Some("ANALYST".into());
        config.statement_timeout = 30;
        let filled = request_body(&config, "select 1");
        assert_eq!(filled["warehouse"], "COMPUTE_WH");
        assert_eq!(filled["role"], "ANALYST");
        assert_eq!(filled["timeout"], 30);
    }

    #[test]
    fn a_result_becomes_rendered_rows_and_named_types() {
        let finished = body(
            r#"{
                "resultSetMetaData": {
                    "numRows": 2,
                    "rowType": [
                        {"name": "ID", "type": "fixed", "precision": 38, "scale": 0, "nullable": false},
                        {"name": "NOTE", "type": "text", "length": 16777216, "nullable": true},
                        {"name": "SEEN", "type": "date", "nullable": true}
                    ],
                    "partitionInfo": [{"rowCount": 2}, {"rowCount": 9}]
                },
                "data": [["1", "", "19000"], ["2", null, null]],
                "statementHandle": "01b0-aaaa"
            }"#,
        );
        let types = row_types(&finished).expect("described");
        assert_eq!(
            types.iter().map(data_type).collect::<Vec<_>>(),
            ["number(38,0)", "varchar", "date"]
        );
        // NULL and the empty string are different answers and stay different.
        assert_eq!(
            rows(&finished, &types),
            vec![
                vec![
                    Some("1".into()),
                    Some(String::new()),
                    Some("2022-01-08".into())
                ],
                vec![Some("2".into()), None, None],
            ]
        );
        assert_eq!(partition_count(&finished), 2);
        assert_eq!(last_child(&finished), None);
        assert_eq!(rows_affected(&finished), None);
    }

    #[test]
    fn several_statements_answer_with_the_last_ones_handle() {
        let finished = body(r#"{"statementHandles": ["01b0-aaaa", "01b0-bbbb"]}"#);
        assert_eq!(last_child(&finished), Some("01b0-bbbb"));
    }

    #[test]
    fn a_write_reports_the_rows_it_touched() {
        let finished =
            body(r#"{"stats": {"numRowsInserted": 3, "numRowsUpdated": 0, "numRowsDeleted": 1}}"#);
        assert_eq!(rows_affected(&finished), Some(4));
    }

    #[test]
    fn a_status_is_running_finished_or_the_servers_own_words() {
        let host = "myorg-myaccount.snowflakecomputing.com";
        assert_eq!(reply(host, 200, &Value::Null), Ok(Reply::Finished));
        assert_eq!(reply(host, 202, &Value::Null), Ok(Reply::Running));

        // A SQL error and a rejected token both explain themselves.
        let refused = body(
            r#"{"code": "001003", "sqlState": "42000",
                "message": "SQL compilation error:\nsyntax error line 1 at position 7 unexpected 'FORM'."}"#,
        );
        assert_eq!(
            reply(host, 422, &refused).expect_err("an error").message,
            "SQL compilation error:\nsyntax error line 1 at position 7 unexpected 'FORM'."
        );
        let unauthorised = body(r#"{"code": "390144", "message": "JWT token is invalid."}"#);
        assert_eq!(
            reply(host, 401, &unauthorised)
                .expect_err("an error")
                .message,
            "JWT token is invalid."
        );
        // Something that is not the API -- a proxy, a wrong host -- has no
        // message, so what happened is all there is to say.
        assert_eq!(
            reply(host, 403, &Value::Null)
                .expect_err("an error")
                .message,
            "myorg-myaccount.snowflakecomputing.com answered HTTP 403."
        );
    }

    #[test]
    fn a_handle_leaves_the_running_list_however_the_query_ends() {
        let running = Mutex::new(Cancelling {
            asked: false,
            handles: vec!["other".to_string()],
        });
        let attempt = || -> Result<(), DbError> {
            running
                .lock()
                .expect("unpoisoned")
                .handles
                .push("mine".into());
            let _guard = RunningGuard {
                running: &running,
                handle: "mine".into(),
            };
            Err(plain_error("the poll failed".into()))
        };
        assert!(attempt().is_err());
        assert_eq!(running.lock().expect("unpoisoned").handles, ["other"]);
    }

    #[test]
    #[ignore = "requires a network"]
    fn live_an_account_that_does_not_exist_is_an_error_in_words() {
        // Needs no account, which is the point: it is the one live check that
        // runs anywhere, and what it pins is that the TLS provider is there at
        // run time -- a missing one panics rather than failing the connect.
        let mut config = config("test-key-2048.p8");
        config.account = "dbdelve-no-such-account".into();
        let error = Connection::open(&config).err().expect("nobody is there");
        println!("{error}");
        assert!(!error.message.is_empty());
    }

    const LIVE: &str = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*";

    /// ACCOUNT, USER, PRIVATE_KEY (an absolute path) and DATABASE are required;
    /// WAREHOUSE, ROLE and HOST are taken when set.
    fn live_config() -> SnowflakeConfig {
        let required = |name: &str| {
            std::env::var(format!("DBDELVE_SNOWFLAKE_{name}")).unwrap_or_else(|_| panic!("{LIVE}"))
        };
        let optional = |name: &str| std::env::var(format!("DBDELVE_SNOWFLAKE_{name}")).ok();
        SnowflakeConfig {
            account: required("ACCOUNT"),
            host: optional("HOST"),
            user: required("USER"),
            private_key: required("PRIVATE_KEY"),
            database: required("DATABASE"),
            warehouse: optional("WAREHOUSE"),
            role: optional("ROLE"),
            statement_timeout: 0,
        }
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_query_round_trip() {
        let connection = Connection::open(&live_config()).expect("connects");
        let result = connection
            .query("SELECT 1 AS one, NULL AS nothing, '' AS blank, DATE '2024-02-29' AS leap")
            .expect("runs");
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["ONE", "NOTHING", "BLANK", "LEAP"]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Some("1".into()),
                None,
                Some(String::new()),
                Some("2024-02-29".into())
            ]]
        );
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_every_temporal_type_renders_as_the_server_would_print_it() {
        // The wire forms in `render` are from the API's documentation; this is
        // the server agreeing with them, compared against its own TO_VARCHAR.
        let connection = Connection::open(&live_config()).expect("connects");
        let result = connection
            .query(
                "SELECT '2021-03-19 17:06:59.250'::TIMESTAMP_NTZ(3), \
                        '2021-03-19 17:06:59 +05:30'::TIMESTAMP_TZ(0), \
                        '23:01:59.123'::TIME(3), \
                        '1969-12-31 23:59:59.500'::TIMESTAMP_NTZ(3)",
            )
            .expect("runs");
        assert_eq!(
            result.rows[0],
            vec![
                Some("2021-03-19 17:06:59.250".to_string()),
                Some("2021-03-19 17:06:59 +05:30".to_string()),
                Some("23:01:59.123".to_string()),
                Some("1969-12-31 23:59:59.500".to_string()),
            ]
        );
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_several_statements_return_the_last_result() {
        let connection = Connection::open(&live_config()).expect("connects");
        let result = connection.query("SELECT 1; SELECT 2 AS two").expect("runs");
        assert_eq!(result.rows, vec![vec![Some("2".into())]]);
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_a_large_result_arrives_whole_across_partitions() {
        let connection = Connection::open(&live_config()).expect("connects");
        let result = connection
            .query("SELECT SEQ4(), RANDSTR(64, RANDOM()) FROM TABLE(GENERATOR(ROWCOUNT => 200000))")
            .expect("runs");
        assert_eq!(result.rows.len(), 200_000);
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_cancel_stops_a_running_statement() {
        let connection = Connection::open(&live_config()).expect("connects");
        let (waiting, run) = (connection.clone(), CancelToken::default());
        let token = run.clone();
        let started = Instant::now();
        let query = std::thread::spawn(move || waiting.query_with("CALL SYSTEM$WAIT(60)", &token));
        // Long enough for the submit to have returned its handle.
        std::thread::sleep(Duration::from_secs(3));
        connection.cancel(&run).expect("the cancel is accepted");
        let error = query.join().expect("no panic").expect_err("stopped");
        println!("{error}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_a_statement_timeout_stops_a_statement() {
        let mut config = live_config();
        config.statement_timeout = 3;
        let connection = Connection::open(&config).expect("connects");
        let started = Instant::now();
        let error = connection
            .query("CALL SYSTEM$WAIT(60)")
            .expect_err("stopped");
        println!("{error}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_a_use_is_refused_rather_than_quietly_forgotten() {
        // The statelessness the module header describes, pinned. The API does
        // not run a `USE` and then lose it between requests; it declines to run
        // one at all, and says so, which is the better of the two ways to have
        // no session.
        let connection = Connection::open(&live_config()).expect("connects");
        let error = connection
            .query("USE SCHEMA INFORMATION_SCHEMA")
            .expect_err("refused");
        println!("{error}");
        assert!(error.message.contains("USE"), "{error}");
    }

    use super::super::result;

    #[test]
    fn a_composite_key_is_one_constraint_with_its_columns_in_order() {
        // A row per column, and not necessarily in key order.
        let primary = result(
            &[
                "table_name",
                "column_name",
                "key_sequence",
                "constraint_name",
            ],
            &[
                &[
                    Some("ORDER_LINES"),
                    Some("LINE"),
                    Some("2"),
                    Some("PK_LINES"),
                ],
                &[Some("ORDERS"), Some("ID"), Some("1"), Some("PK_ORDERS")],
                &[
                    Some("ORDER_LINES"),
                    Some("ORDER_ID"),
                    Some("1"),
                    Some("PK_LINES"),
                ],
            ],
        );
        assert_eq!(
            key_definitions(
                &primary,
                "table_name",
                "ORDER_LINES",
                "constraint_name",
                "PRIMARY KEY"
            ),
            vec![NamedDefinition {
                name: "PK_LINES".into(),
                definition: r#"PRIMARY KEY ("ORDER_ID", "LINE")"#.into(),
            }]
        );
    }

    fn imported_keys() -> QueryResult {
        result(
            &[
                "pk_database_name",
                "pk_schema_name",
                "pk_table_name",
                "pk_column_name",
                "fk_table_name",
                "fk_column_name",
                "key_sequence",
                "fk_name",
            ],
            &[
                &[
                    Some("ANALYTICS"),
                    Some("PUBLIC"),
                    Some("ORDERS"),
                    Some("ID"),
                    Some("ORDER_LINES"),
                    Some("ORDER_ID"),
                    Some("1"),
                    Some("FK_ORDER"),
                ],
                &[
                    Some("REFERENCE"),
                    Some("PUBLIC"),
                    Some("PRODUCTS"),
                    Some("SKU"),
                    Some("ORDER_LINES"),
                    Some("SKU"),
                    Some("1"),
                    Some("FK_PRODUCT"),
                ],
            ],
        )
    }

    #[test]
    fn a_foreign_key_is_rendered_with_what_it_references() {
        let definitions = key_definitions(
            &imported_keys(),
            "fk_table_name",
            "ORDER_LINES",
            "fk_name",
            "FOREIGN KEY",
        );
        assert_eq!(
            definitions[0].definition,
            r#"FOREIGN KEY ("ORDER_ID") REFERENCES "PUBLIC"."ORDERS" ("ID")"#
        );
        assert_eq!(definitions.len(), 2);
    }

    #[test]
    fn a_key_into_another_database_is_shown_but_not_followed() {
        // Following it would filter a `PRODUCTS` in this database, if there
        // happened to be one, on a key that belongs to a different table.
        let followed = foreign_keys(&imported_keys(), "ORDER_LINES", "ANALYTICS");
        assert_eq!(
            followed,
            vec![ForeignKey {
                column: "ORDER_ID".into(),
                referenced_schema: "PUBLIC".into(),
                referenced_table: "ORDERS".into(),
                referenced_column: "ID".into(),
            }]
        );
    }

    #[test]
    fn two_routine_results_are_one_only_when_they_agree() {
        let functions = result(
            &["schema_name", "routine_name"],
            &[&[Some("app"), Some("f")]],
        );
        let procedures = result(
            &["schema_name", "routine_name"],
            &[&[Some("app"), Some("p")]],
        );
        assert_eq!(
            appended(functions.clone(), procedures)
                .expect("same shape")
                .rows
                .len(),
            2
        );
        let other = result(&["schema_name"], &[&[Some("app")]]);
        assert!(appended(functions, other).is_err());
    }

    #[test]
    fn a_catalog_result_in_its_aliases_is_what_the_assemblers_read() {
        // The aliases in the SQL above and the names the shared assemblers look
        // up are the same strings in two places; this is what holds them together.
        for alias in ["schema_name", "relation_name", "relation_kind"] {
            assert!(
                RELATIONS_SQL.contains(&format!("AS \"{alias}\"")),
                "{alias}"
            );
        }
        for alias in [
            "schema_name",
            "routine_name",
            "routine_kind",
            "identity_arguments",
            "result_type",
            "language",
            "definition",
        ] {
            for sql in [FUNCTIONS_SQL, PROCEDURES_SQL] {
                assert!(sql.contains(&format!("AS \"{alias}\"")), "{alias}");
            }
        }
        for alias in ["column_name", "data_type", "nullable", "column_default"] {
            assert!(COLUMNS_SQL.contains(&format!("AS \"{alias}\"")), "{alias}");
        }
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_catalog_and_structure_round_trip() {
        // Needs a warehouse and the right to create a schema in the database.
        let connection = Connection::open(&live_config()).expect("connects");
        connection
            .query(
                "CREATE OR REPLACE SCHEMA DBDELVE_TEST; \
                 CREATE TABLE DBDELVE_TEST.ORDERS (ID NUMBER(38,0) PRIMARY KEY, NOTE VARCHAR(40) DEFAULT 'x'); \
                 CREATE TABLE DBDELVE_TEST.ORDER_LINES (ORDER_ID NUMBER(38,0) NOT NULL REFERENCES DBDELVE_TEST.ORDERS (ID), \
                     LINE NUMBER(38,0) NOT NULL, SEEN TIMESTAMP_NTZ, PRIMARY KEY (ORDER_ID, LINE)); \
                 CREATE VIEW DBDELVE_TEST.RECENT AS SELECT * FROM DBDELVE_TEST.ORDERS",
            )
            .expect("the fixture schema is created");

        let catalog = connection.catalog().expect("the catalog loads");
        let schema = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "DBDELVE_TEST")
            .expect("the schema is listed");
        let kinds: Vec<_> = schema
            .relations
            .iter()
            .map(|r| (r.name.as_str(), r.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("ORDERS", super::super::RelationKind::Table),
                ("ORDER_LINES", super::super::RelationKind::Table),
                ("RECENT", super::super::RelationKind::View),
            ]
        );
        assert!(
            catalog
                .schemas
                .iter()
                .all(|schema| schema.name != "INFORMATION_SCHEMA")
        );

        let lines = connection
            .structure("DBDELVE_TEST", "ORDER_LINES")
            .expect("described");
        println!("{lines:#?}");
        assert_eq!(lines.columns[0].data_type, "number(38,0)");
        assert!(!lines.columns[0].nullable);
        assert_eq!(lines.foreign_keys.len(), 1);
        assert_eq!(lines.constraints.len(), 2);
        // A view has columns and no keys, and asking is not an error.
        let view = connection
            .structure("DBDELVE_TEST", "RECENT")
            .expect("described");
        assert_eq!(view.columns.len(), 2);
        assert!(view.constraints.is_empty());

        connection
            .query("DROP SCHEMA DBDELVE_TEST")
            .expect("cleaned up");
    }

    #[test]
    fn a_key_file_is_the_same_key_however_it_was_written() {
        let pem = std::fs::read_to_string(test_key("test-key-2048.p8")).expect("readable");
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let expected = key_der(&pem).expect("the PEM reads");

        for (shape, text) in [
            ("the base64 body alone", body),
            // How it comes out of an environment variable.
            ("the PEM encoded once more", STANDARD.encode(&pem)),
        ] {
            assert_eq!(key_der(&text).as_ref(), Some(&expected), "{shape}");
        }
        assert_eq!(key_der("hunter2"), None);
    }

    #[test]
    fn an_account_is_found_inside_the_url_it_was_pasted_as() {
        for input in [
            "myorg-myaccount",
            "myorg-myaccount.snowflakecomputing.com",
            "https://myorg-myaccount.snowflakecomputing.com",
            "https://myorg-myaccount.snowflakecomputing.com/console/login?x=1",
            "  https://myorg-myaccount.snowflakecomputing.com:443/ ",
        ] {
            assert_eq!(account_identifier(input), "myorg-myaccount", "{input}");
        }
        // A legacy locator keeps its region: the host needs it, and the token
        // drops it for itself.
        assert_eq!(
            account_identifier("https://xy12345.eu-central-1.snowflakecomputing.com"),
            "xy12345.eu-central-1"
        );
    }

    #[test]
    fn a_database_typed_bare_is_the_upper_case_name_the_server_stores() {
        let named = |database: &str| SnowflakeConfig {
            database: database.into(),
            ..Default::default()
        };
        assert_eq!(named("analytics").stored_database(), "ANALYTICS");
        assert_eq!(named("L1_PROMIS$X").stored_database(), "L1_PROMIS$X");
        // Neither could have been created without quotes, so neither was folded.
        assert_eq!(named("my-db").stored_database(), "my-db");
        assert_eq!(named("1st").stored_database(), "1st");
    }

    /// The labels of the rows a filter bar's predicate keeps, out of four rows
    /// made on the spot. There is no session, so there is no temporary table
    /// to make them in.
    fn kept(
        connection: &Connection,
        column: &str,
        operator: crate::filter::Operator,
        value: &str,
    ) -> Result<Vec<String>, DbError> {
        let predicate = crate::filter::filter_predicate(Engine::Snowflake, column, operator, value)
            .expect("the bar adds up to a predicate");
        let sql = format!(
            "SELECT \"label\" FROM (\
                 SELECT column1 AS \"label\", column2 AS \"state\", column3 AS \"n\" \
                 FROM VALUES ('percent', '50%', 1), ('plain', '500', 2), \
                             ('quoted', 'it''s ok\\\\', 3), ('absent', NULL, NULL)\
             ) WHERE {predicate} ORDER BY 1"
        );
        Ok(connection
            .query(&sql)?
            .rows
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect())
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_a_filter_matches_what_its_operator_says() {
        use crate::filter::Operator;
        let connection = Connection::open(&live_config()).expect("connects");
        let kept = |operator, value: &str| {
            kept(&connection, "state", operator, value).unwrap_or_else(|error| panic!("{error}"))
        };

        // A percent sign in the value is a percent sign: '500' is not matched.
        assert_eq!(kept(Operator::Contains, "50%"), ["percent"]);
        assert_eq!(kept(Operator::StartsWith, "50"), ["percent", "plain"]);
        assert_eq!(kept(Operator::EndsWith, "%"), ["percent"]);
        assert_eq!(kept(Operator::NotContains, "5"), ["quoted"]);
        // A quote and a trailing backslash both survive the literal.
        assert_eq!(kept(Operator::Equals, r"it's ok\"), ["quoted"]);
        // Anywhere in the value, as on the other engines, and `\d` arrives as
        // `\d` rather than as `d`.
        assert_eq!(kept(Operator::Regex, r"^5\d"), ["percent", "plain"]);
        assert_eq!(kept(Operator::Regex, "ok"), ["quoted"]);
        assert_eq!(kept(Operator::IsNull, ""), ["absent"]);
        assert_eq!(kept(Operator::InList, "500, 50%"), ["percent", "plain"]);
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_every_operator_is_a_statement_the_server_accepts() {
        use crate::filter::Operator;
        let connection = Connection::open(&live_config()).expect("connects");
        for operator in Operator::ALL {
            let value = match operator {
                Operator::Between => "1..9",
                Operator::InList | Operator::NotInList => "1, 2",
                _ => "1",
            };
            // Against text every operator has to run.
            if let Err(error) = kept(&connection, "state", operator, value) {
                panic!("{} on a text column: {error}", operator.slug());
            }
            // Against a number the text operators may be refused, as `LIKE` on
            // an integer is on Postgres. Which ones is worth knowing, and is
            // not a failure.
            if let Err(error) = kept(&connection, "n", operator, value) {
                println!("{} on a number column: {error}", operator.slug());
            }
        }
    }

    #[test]
    #[ignore = "requires a Snowflake account configured through DBDELVE_SNOWFLAKE_*"]
    fn live_the_explorer_does_not_wait_on_the_routines() {
        // The relations are what the explorer opens with, so the half that is
        // reliably slower must not be in front of them.
        let connection = Connection::open(&live_config()).expect("connects");
        let started = Instant::now();
        let catalog = connection.catalog().expect("relations");
        let relations = started.elapsed();
        assert!(
            catalog
                .schemas
                .iter()
                .any(|schema| !schema.relations.is_empty())
        );
        assert!(
            catalog
                .schemas
                .iter()
                .all(|schema| schema.routines.is_empty())
        );

        let started = Instant::now();
        connection.routines().expect("routines");
        println!("relations {relations:?}, routines {:?}", started.elapsed());
    }

    // The same ground as the live tests, against responses recorded from a
    // real account and replayed by `mock`, so it runs anywhere.

    use super::mock::{Mock, Response};

    fn connected(mock: &Mock) -> Connection {
        Connection::open(&mock.config()).expect("connects")
    }

    #[test]
    fn a_query_is_submitted_verbatim_and_polled_to_its_result() {
        let mock = Mock::start();
        let sql = "SELECT 1 AS one, NULL AS nothing, '' AS blank, DATE '2024-02-29' AS leap";
        let handle = mock.answer(sql, "query");
        let result = connected(&mock).query(sql).expect("runs");

        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.data_type.as_deref().unwrap_or_default()))
                .collect::<Vec<_>>(),
            [
                ("ONE", "number(1,0)"),
                ("NOTHING", "varchar"),
                ("BLANK", "varchar"),
                ("LEAP", "date")
            ]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Some("1".into()),
                None,
                Some(String::new()),
                Some("2024-02-29".into())
            ]]
        );

        let requests = mock.requests();
        let submit = requests
            .iter()
            .find(|request| request.body["statement"] == sql)
            .expect("submitted");
        assert_eq!(submit.target, "?async=true");
        assert_eq!(submit.body["database"], "DBDELVE_TEST");
        assert_eq!(submit.body["warehouse"], "COMPUTE_WH");
        assert_eq!(
            submit.header("X-Snowflake-Authorization-Token-Type"),
            Some("KEYPAIR_JWT")
        );
        assert!(
            submit
                .header("Authorization")
                .is_some_and(|value| value.starts_with("Bearer ey"))
        );
        assert_eq!(mock.hits("GET", &format!("/{handle}")), 1);
    }

    #[test]
    fn a_statement_still_running_is_polled_until_it_finishes() {
        let mock = Mock::start();
        let sql = "SELECT 1 AS one, NULL AS nothing, '' AS blank, DATE '2024-02-29' AS leap";
        let handle = mock.accept(sql, "query");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [
                Response::fixture(202, "wait_running.json"),
                Response::fixture(202, "wait_running.json"),
                Response::fixture(200, "query_poll.json"),
            ],
        );
        let result = connected(&mock).query(sql).expect("runs");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(mock.hits("GET", &format!("/{handle}")), 3);
    }

    #[test]
    fn recorded_values_of_every_awkward_type_render_as_text() {
        let mock = Mock::start();
        let temporal = "SELECT '2021-03-19 17:06:59.250'::TIMESTAMP_NTZ(3), \
                        '2021-03-19 17:06:59 +05:30'::TIMESTAMP_TZ(0), \
                        '23:01:59.123'::TIME(3), \
                        '1969-12-31 23:59:59.500'::TIMESTAMP_NTZ(3)";
        mock.answer(temporal, "temporal");
        let customers = "SELECT * FROM SALES.CUSTOMERS ORDER BY 1 LIMIT 3";
        mock.answer(customers, "customers");
        let connection = connected(&mock);

        assert_eq!(
            connection.query(temporal).expect("runs").rows[0],
            vec![
                Some("2021-03-19 17:06:59.250".to_string()),
                Some("2021-03-19 17:06:59 +05:30".to_string()),
                Some("23:01:59.123".to_string()),
                Some("1969-12-31 23:59:59.500".to_string()),
            ]
        );

        let result = connection.query(customers).expect("runs");
        assert_eq!(
            result
                .columns
                .iter()
                .filter_map(|c| c.data_type.as_deref())
                .collect::<Vec<_>>(),
            [
                "number(38,0)",
                "varchar",
                "varchar",
                "varchar",
                "timestamp_ntz",
                "timestamp_ltz",
                "timestamp_tz",
                "binary",
                "variant"
            ]
        );
        assert_eq!(
            result.rows[0],
            vec![
                Some("1".to_string()),
                Some("eug6VhDX".to_string()),
                Some("c1aoec@example.com".to_string()),
                Some("DE".to_string()),
                Some("2025-07-21 18:54:00.000000000".to_string()),
                Some("2026-09-24 20:49:57.992000000Z".to_string()),
                Some("2026-09-24 13:49:57.992000000 -07:00".to_string()),
                // Binary arrives as hex and a variant as its JSON text, and
                // both are shown as they came.
                Some("31624950".to_string()),
                Some("{\n  \"beta\": true,\n  \"theme\": \"light\"\n}".to_string()),
            ]
        );
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn several_statements_answer_with_the_last_ones_result() {
        let mock = Mock::start();
        mock.answer("SELECT 1; SELECT 2 AS two", "multi");
        let last = super::mock::fixture("multi_poll.json");
        let last: Value = serde_json::from_slice(&last).expect("JSON");
        let child = last_child(&last).expect("a handle per statement");
        mock.on(
            "GET",
            &format!("/{child}"),
            [Response::fixture(200, "multi_child.json")],
        );

        let result = connected(&mock)
            .query("SELECT 1; SELECT 2 AS two")
            .expect("runs");
        assert_eq!(result.columns[0].name, "TWO");
        assert_eq!(result.rows, vec![vec![Some("2".into())]]);
    }

    #[test]
    fn a_large_result_is_read_across_its_compressed_partitions() {
        let mock = Mock::start();
        let sql = "SELECT SEQ4() AS N FROM TABLE(GENERATOR(ROWCOUNT => 20000))";
        let handle = mock.accept(sql, "large");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(200, "large_poll.json.gz")],
        );
        mock.on(
            "GET",
            &format!("/{handle}?partition=1"),
            [Response::fixture(200, "large_partition_1.json.gz")],
        );

        let result = connected(&mock).query(sql).expect("runs");
        assert_eq!(result.rows.len(), 20_000);
        assert_eq!(result.rows[0], vec![Some("0".to_string())]);
        assert_eq!(result.rows[19_999], vec![Some("19999".to_string())]);
        assert_eq!(mock.hits("GET", &format!("/{handle}?partition=1")), 1);
    }

    #[test]
    fn a_partition_cut_short_is_an_error_and_not_fewer_rows() {
        let mock = Mock::start();
        let sql = "SELECT SEQ4() AS N FROM TABLE(GENERATOR(ROWCOUNT => 20000))";
        let handle = mock.accept(sql, "large");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(200, "large_poll.json.gz")],
        );
        mock.on(
            "GET",
            &format!("/{handle}?partition=1"),
            [Response::fixture(200, "large_partition_1.json.gz").truncated(100)],
        );

        let error = connected(&mock).query(sql).expect_err("half a result");
        assert!(error.message.contains("was not read whole"), "{error}");
    }

    #[test]
    fn a_poll_or_partition_that_fails_for_a_moment_is_asked_again() {
        let mock = Mock::start();
        let sql = "SELECT SEQ4() AS N FROM TABLE(GENERATOR(ROWCOUNT => 20000))";
        let handle = mock.accept(sql, "large");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [
                Response::text(503, "Service Unavailable"),
                Response::fixture(202, "wait_running.json").truncated(10),
                Response::text(429, "Too Many Requests"),
                Response::fixture(200, "large_poll.json.gz"),
            ],
        );
        let partition = format!("/{handle}?partition=1");
        mock.on(
            "GET",
            &partition,
            [
                Response::text(504, "Gateway Timeout"),
                Response::fixture(200, "large_partition_1.json.gz"),
            ],
        );

        let result = connected(&mock).query(sql).expect("runs");
        assert_eq!(result.rows.len(), 20_000);
        assert_eq!(mock.hits("GET", &format!("/{handle}")), 4);
        assert_eq!(mock.hits("GET", &partition), 2);
    }

    #[test]
    fn a_poll_that_keeps_failing_asks_the_statement_to_stop_before_giving_up() {
        let mock = Mock::start();
        let handle = mock.accept("INSERT INTO T VALUES (1)", "wait");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::text(503, "Service Unavailable")],
        );
        let stop = format!("/{handle}/cancel");
        mock.on("POST", &stop, [Response::fixture(200, "cancel.json")]);

        let error = connected(&mock)
            .query("INSERT INTO T VALUES (1)")
            .expect_err("never answered");
        assert!(error.message.contains("HTTP 503"), "{error}");
        assert!(error.message.contains("asked to stop"), "{error}");
        assert_eq!(mock.hits("POST", &stop), 1);
        assert_eq!(
            mock.hits("POST", "?async=true"),
            2,
            "the submit is sent once"
        );
    }

    #[test]
    fn cancel_posts_to_the_running_handle_and_the_query_ends_in_the_servers_words() {
        let mock = Mock::start();
        let handle = mock.accept("CALL SYSTEM$WAIT(60)", "wait");
        let poll = format!("/{handle}");
        mock.on("GET", &poll, [Response::fixture(202, "wait_running.json")]);
        mock.on(
            "POST",
            &format!("/{handle}/cancel"),
            [Response::fixture(200, "cancel.json")],
        );
        let connection = connected(&mock);

        let (waiting, run) = (connection.clone(), CancelToken::default());
        let token = run.clone();
        let query = std::thread::spawn(move || waiting.query_with("CALL SYSTEM$WAIT(60)", &token));
        let started = Instant::now();
        while mock.hits("GET", &poll) == 0 {
            assert!(started.elapsed() < Duration::from_secs(10), "never polled");
            std::thread::sleep(Duration::from_millis(10));
        }
        connection.cancel(&run).expect("the cancel is accepted");
        assert_eq!(mock.hits("POST", &format!("/{handle}/cancel")), 1);
        // What the server says to the next poll once the cancel has landed.
        mock.on("GET", &poll, [Response::fixture(422, "cancelled.json")]);

        let error = query.join().expect("no panic").expect_err("stopped");
        assert_eq!(error.message, "SQL execution canceled");
        // Nothing left running, so a second cancel sends nothing.
        connection.cancel(&run).expect("nothing to cancel");
        assert_eq!(mock.hits("POST", &format!("/{handle}/cancel")), 1);
    }

    /// Waits for a statement to be polled, which is when it is cancellable.
    fn until_polled(mock: &Mock, handle: &str) {
        let started = Instant::now();
        while mock.hits("GET", &format!("/{handle}")) == 0 {
            assert!(started.elapsed() < Duration::from_secs(10), "never polled");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn cancel_stops_only_the_statements_of_the_run_it_was_asked_for() {
        let mock = Mock::start();
        // A catalog load past the API's 45-second window, which hands back a
        // handle as an asynchronous submit does.
        let background = mock.accept("CALL SYSTEM$WAIT(60)", "wait");
        mock.on_statement(
            "",
            "FROM INFORMATION_SCHEMA.TABLES",
            [Response::fixture(202, "wait_submit.json")],
        );
        let mine_sql = "SELECT 1 AS one, NULL AS nothing, '' AS blank, DATE '2024-02-29' AS leap";
        let mine = mock.accept(mine_sql, "query");
        let theirs_sql = "SELECT * FROM SALES.CUSTOMERS ORDER BY 1 LIMIT 3";
        let theirs = mock.accept(theirs_sql, "customers");
        for handle in [&background, &mine, &theirs] {
            mock.on(
                "GET",
                &format!("/{handle}"),
                [Response::fixture(202, "wait_running.json")],
            );
            mock.on(
                "POST",
                &format!("/{handle}/cancel"),
                [Response::fixture(200, "cancel.json")],
            );
        }
        let connection = connected(&mock);

        let catalog = {
            let connection = connection.clone();
            std::thread::spawn(move || connection.catalog())
        };
        let my_run = CancelToken::default();
        let my_query = {
            let (connection, token) = (connection.clone(), my_run.clone());
            std::thread::spawn(move || connection.query_with(mine_sql, &token))
        };
        let their_query = {
            let connection = connection.clone();
            std::thread::spawn(move || connection.query_with(theirs_sql, &CancelToken::default()))
        };
        for handle in [&background, &mine, &theirs] {
            until_polled(&mock, handle);
        }

        connection.cancel(&my_run).expect("the cancel is accepted");
        assert_eq!(mock.hits("POST", &format!("/{mine}/cancel")), 1);
        assert_eq!(mock.hits("POST", &format!("/{theirs}/cancel")), 0);
        assert_eq!(mock.hits("POST", &format!("/{background}/cancel")), 0);

        for handle in [&background, &mine, &theirs] {
            mock.on(
                "GET",
                &format!("/{handle}"),
                [Response::fixture(422, "cancelled.json")],
            );
        }
        for thread in [my_query, their_query] {
            assert!(thread.join().expect("no panic").is_err());
        }
        assert!(catalog.join().expect("no panic").is_err());
    }

    #[test]
    fn a_cancel_that_fails_for_one_statement_still_asks_for_the_rest() {
        let mock = Mock::start();
        let first_sql = "SELECT 1 AS one, NULL AS nothing, '' AS blank, DATE '2024-02-29' AS leap";
        let first = mock.accept(first_sql, "query");
        let second_sql = "SELECT * FROM SALES.CUSTOMERS ORDER BY 1 LIMIT 3";
        let second = mock.accept(second_sql, "customers");
        for handle in [&first, &second] {
            mock.on(
                "GET",
                &format!("/{handle}"),
                [Response::fixture(202, "wait_running.json")],
            );
        }
        mock.on(
            "POST",
            &format!("/{first}/cancel"),
            [Response::text(503, "Service Unavailable")],
        );
        mock.on(
            "POST",
            &format!("/{second}/cancel"),
            [Response::fixture(200, "cancel.json")],
        );
        let connection = connected(&mock);
        let run = CancelToken::default();
        let mut queries = Vec::new();
        for (sql, handle) in [(first_sql, &first), (second_sql, &second)] {
            let (connection, token) = (connection.clone(), run.clone());
            queries.push(std::thread::spawn(move || {
                connection.query_with(sql, &token)
            }));
            until_polled(&mock, handle);
        }

        let error = connection.cancel(&run).expect_err("one was not stopped");
        assert!(error.message.contains("HTTP 503"), "{error}");
        assert_eq!(mock.hits("POST", &format!("/{second}/cancel")), 1);

        for handle in [&first, &second] {
            mock.on(
                "GET",
                &format!("/{handle}"),
                [Response::fixture(422, "cancelled.json")],
            );
        }
        for query in queries {
            assert!(query.join().expect("no panic").is_err());
        }
    }

    #[test]
    fn a_cancel_sent_before_the_handle_arrives_stops_the_statement_when_it_does() {
        let mock = Mock::start();
        let sql = "CALL SYSTEM$WAIT(60)";
        let handle = mock.accept(sql, "wait");
        mock.on_statement(
            "?async=true",
            sql,
            [Response::fixture(202, "wait_submit.json").delayed(Duration::from_millis(200))],
        );
        let poll = format!("/{handle}");
        mock.on("GET", &poll, [Response::fixture(202, "wait_running.json")]);
        let stop = format!("/{handle}/cancel");
        mock.on("POST", &stop, [Response::fixture(200, "cancel.json")]);
        let connection = connected(&mock);

        let run = CancelToken::default();
        let query = {
            let (connection, token) = (connection.clone(), run.clone());
            std::thread::spawn(move || connection.query_with(sql, &token))
        };
        let started = Instant::now();
        // The version check's submit, then this one's, still unanswered.
        while mock.hits("POST", "?async=true") < 2 {
            assert!(started.elapsed() < Duration::from_secs(10), "never sent");
            std::thread::sleep(Duration::from_millis(5));
        }
        connection.cancel(&run).expect("nothing to cancel yet");
        assert_eq!(mock.hits("POST", &stop), 0);

        while mock.hits("POST", &stop) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "never cancelled"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        mock.on("GET", &poll, [Response::fixture(422, "cancelled.json")]);
        let error = query.join().expect("no panic").expect_err("stopped");
        assert_eq!(error.message, "SQL execution canceled");
    }

    #[test]
    fn a_body_that_stops_arriving_is_an_error_rather_than_a_wait() {
        let mock = Mock::start();
        let sql = "CALL SYSTEM$WAIT(60)";
        let pause = Duration::from_secs(5);
        mock.on_statement(
            "?async=true",
            sql,
            [Response::fixture(202, "wait_submit.json").stalled(10, pause)],
        );
        let connection = connected(&mock);

        let started = Instant::now();
        let error = connection.query(sql).expect_err("stalled");
        assert!(
            started.elapsed() < pause - Duration::from_secs(1),
            "{error}"
        );
        assert!(error.message.contains("was not read whole"), "{error}");
    }

    #[test]
    fn a_statement_timeout_is_sent_and_its_expiry_reported() {
        let mock = Mock::start();
        let handle = mock.accept("CALL SYSTEM$WAIT(60)", "timeout");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(408, "timed_out.json")],
        );
        let mut config = mock.config();
        config.statement_timeout = 3;
        let connection = Connection::open(&config).expect("connects");

        let error = connection
            .query("CALL SYSTEM$WAIT(60)")
            .expect_err("stopped");
        assert_eq!(
            error.message,
            "Statement reached its statement or warehouse timeout of 3 second(s) and was canceled."
        );
        assert!(
            mock.requests()
                .iter()
                .filter(|request| request.method == "POST")
                .all(|request| request.body["timeout"] == 3)
        );
    }

    #[test]
    fn a_refused_statement_is_an_error_in_the_servers_words() {
        let mock = Mock::start();
        let handle = mock.accept("USE SCHEMA", "use");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(422, "use_refused.json")],
        );
        let handle = mock.accept("FORM PEOPLE", "syntax");
        mock.on(
            "GET",
            &format!("/{handle}"),
            [Response::fixture(422, "syntax_error.json")],
        );
        let connection = connected(&mock);

        let error = connection
            .query("USE SCHEMA INFORMATION_SCHEMA")
            .expect_err("refused");
        assert_eq!(error.message, "Command not supported by SQL API: USE");
        let error = connection
            .query("SELECT * FORM PEOPLE")
            .expect_err("refused");
        assert_eq!(
            error.message,
            "SQL compilation error:\nsyntax error line 1 at position 9 unexpected 'FORM'."
        );
    }

    #[test]
    fn a_key_the_account_does_not_know_fails_the_connect() {
        let mock = Mock::start();
        mock.on_statement(
            "?async=true",
            "CURRENT_VERSION",
            [Response::fixture(401, "auth_failure.json")],
        );
        let error = Connection::open(&mock.config()).err().expect("refused");
        assert_eq!(error.message, "JWT token is invalid. null");
    }

    #[test]
    fn an_account_that_does_not_exist_is_the_status_its_host_answered() {
        // What `live_an_account_that_does_not_exist_is_an_error_in_words`
        // gets back: an HTML page with no message to quote.
        let mock = Mock::start();
        mock.on_statement(
            "?async=true",
            "CURRENT_VERSION",
            [Response::fixture(404, "unknown_account.html")],
        );
        let error = Connection::open(&mock.config())
            .err()
            .expect("nobody is there");
        assert!(error.message.ends_with("answered HTTP 404."), "{error}");
    }

    #[test]
    fn the_catalog_routines_and_structure_are_read_from_recorded_answers() {
        use super::super::{RelationKind, RoutineKind};
        let mock = Mock::start();
        let within = "IN SCHEMA \"DBDELVE_TEST\".\"SALES\"";
        for (statement, name) in [
            ("FROM INFORMATION_SCHEMA.TABLES", "relations"),
            ("FROM INFORMATION_SCHEMA.FUNCTIONS", "functions"),
            ("FROM INFORMATION_SCHEMA.PROCEDURES", "procedures"),
            ("TABLE_NAME = 'ORDER_LINES'", "columns_ORDER_LINES"),
            (
                "TABLE_NAME = 'REVENUE_BY_COUNTRY'",
                "columns_REVENUE_BY_COUNTRY",
            ),
            (&format!("SHOW PRIMARY KEYS {within}"), "show_primary"),
            (&format!("SHOW UNIQUE KEYS {within}"), "show_unique"),
            (&format!("SHOW IMPORTED KEYS {within}"), "show_imported"),
        ] {
            mock.on_statement(
                "",
                statement,
                [Response::fixture(200, &format!("{name}.json"))],
            );
        }
        let connection = connected(&mock);

        let mut catalog = connection.catalog().expect("the relations load");
        catalog.merge(connection.routines().expect("the routines load"));
        let listed: Vec<_> = catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema
                    .relations
                    .iter()
                    .map(move |r| (schema.name.as_str(), r.name.as_str(), r.kind))
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("PUBLIC", "PEOPLE", RelationKind::Table),
                ("SALES", "CUSTOMERS", RelationKind::Table),
                ("SALES", "ORDERS", RelationKind::Table),
                ("SALES", "ORDER_LINES", RelationKind::Table),
                ("SALES", "REVENUE_BY_COUNTRY", RelationKind::View),
            ]
        );
        let sales = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == "SALES")
            .expect("listed");
        let routines: Vec<_> = sales
            .routines
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.identity_arguments.as_str()))
            .collect();
        assert_eq!(
            routines,
            [
                ("WITH_VAT", RoutineKind::Function, "(AMOUNT NUMBER)"),
                ("REFUND", RoutineKind::Procedure, "(ORDER_ID NUMBER)"),
            ]
        );

        let lines = connection
            .structure("SALES", "ORDER_LINES")
            .expect("described");
        assert_eq!(
            lines
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
                .collect::<Vec<_>>(),
            [
                ("ORDER_ID", "number(38,0)", false),
                ("LINE_NO", "number(38,0)", false),
                ("SKU", "varchar", true),
                ("QTY", "number(38,0)", true),
            ]
        );
        assert_eq!(
            lines
                .constraints
                .iter()
                .map(|c| c.definition.as_str())
                .collect::<Vec<_>>(),
            [
                r#"PRIMARY KEY ("ORDER_ID", "LINE_NO")"#,
                r#"FOREIGN KEY ("ORDER_ID") REFERENCES "SALES"."ORDERS" ("ID")"#,
            ]
        );
        assert_eq!(
            lines.foreign_keys,
            vec![ForeignKey {
                column: "ORDER_ID".into(),
                referenced_schema: "SALES".into(),
                referenced_table: "ORDERS".into(),
                referenced_column: "ID".into(),
            }]
        );

        let view = connection
            .structure("SALES", "REVENUE_BY_COUNTRY")
            .expect("described");
        assert_eq!(view.columns.len(), 3);
        assert!(view.constraints.is_empty() && view.foreign_keys.is_empty());

        // dbdelve's own statements skip the asynchronous round trip.
        assert!(
            mock.requests()
                .iter()
                .filter(|request| request.body["statement"]
                    .as_str()
                    .is_some_and(|sql| !sql.contains("CURRENT_VERSION")))
                .all(|request| request.target.is_empty())
        );
    }

    #[test]
    fn each_filter_writes_the_statement_that_was_recorded_against_the_server() {
        // Routed by the whole predicate, so a filter that starts writing
        // different SQL finds no recording and fails here rather than live.
        use crate::filter::Operator;
        let mock = Mock::start();
        let connection = connected(&mock);
        for (name, operator, value, expected) in [
            ("contains", Operator::Contains, "50%", &["percent"][..]),
            ("starts", Operator::StartsWith, "50", &["percent", "plain"]),
            ("ends", Operator::EndsWith, "%", &["percent"]),
            ("notcontains", Operator::NotContains, "5", &["quoted"]),
            ("equals", Operator::Equals, r"it's ok\", &["quoted"]),
            (
                "regex_digit",
                Operator::Regex,
                r"^5\d",
                &["percent", "plain"],
            ),
            ("regex_ok", Operator::Regex, "ok", &["quoted"]),
            ("isnull", Operator::IsNull, "", &["absent"]),
            (
                "inlist",
                Operator::InList,
                "500, 50%",
                &["percent", "plain"],
            ),
        ] {
            let predicate =
                crate::filter::filter_predicate(Engine::Snowflake, "state", operator, value)
                    .expect("a predicate");
            mock.answer(
                &format!(") WHERE {predicate} ORDER BY 1"),
                &format!("filter_{name}"),
            );
            let kept = kept(&connection, "state", operator, value)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(kept, expected, "{name}");
        }
    }
}

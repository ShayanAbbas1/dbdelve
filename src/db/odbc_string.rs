//! ODBC connection strings: what the form and `open` need to know about one
//! without a driver. Which attributes are secrets, which host and port the
//! driver would dial, and the same string pointed at an SSH tunnel's forward.
//!
//! ponytail: ODBC's syntax is read in the app rather than in the bridge because
//! the `PWD` refusal has to hold before anything is written to disk (saving
//! starts no process), and the tunnel rewrite has to happen where the tunnel
//! is. The ceiling is the major drivers' spellings below; a driver spelling its
//! host another way finds no host key and is refused, never dialled past the
//! tunnel. Upgrade path: one more entry in the key lists.

use std::{net::SocketAddr, ops::Range};

/// The sentence the form and `open` show for a string that cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub String);

/// Where the string says the driver will connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// The attributes holding the host and the port, spelled as the user wrote
    /// them: the form names them when it says what the tunnel will do.
    pub keys: Vec<String>,
}

// Each spelling is the driver's own; the rewrite touches exactly these.
const HOST_KEYS: &[&str] = &[
    "HOSTNAME",   // Db2 CLI
    "HOST",       // Db2's alias, and others
    "SERVER",     // psqlODBC, MySQL Connector/ODBC, Microsoft's SQL Server drivers
    "SERVERNAME", // psqlODBC's alias
    "ADDRESS",    // SQL Server's, overriding Server
    "ADDR",       // SQL Server's short form of Address
];
/// Microsoft's drivers carry the port inside the host value, `host,port`.
const COMMA_PORT_KEYS: &[&str] = &["SERVER", "ADDRESS", "ADDR"];
const PORT_KEYS: &[&str] = &["PORT"]; // Db2, psqlODBC, MySQL

/// Where a driver reads the port it dials.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PortSpelling {
    /// Microsoft's SQL Server drivers: `Server=host,port`. They have no port
    /// key, so `Port=` beside `Server=` leaves them dialling 1433.
    InHost,
    /// psqlODBC, MySQL's and MariaDB's connectors and Db2's CLI driver: a
    /// `PORT` key. A `,` in the host is a list of hosts to libpq, dialled on
    /// the default port.
    Key,
}

/// Matched, lowercased, in the `DRIVER` value: the registered name, or the
/// file name of a library path. The drivers whose host and port keys are
/// written above; any other is refused, since a wrong guess at where it reads
/// its port sends it, and the password, past the tunnel.
const DRIVERS: &[(&str, PortSpelling)] = &[
    ("sql server", PortSpelling::InHost), // ODBC Driver 18 for SQL Server, SQL Server Native Client
    ("msodbcsql", PortSpelling::InHost),  // libmsodbcsql-18.so, msodbcsql18.dll
    ("postgres", PortSpelling::Key),      // PostgreSQL Unicode, PostgreSQL ANSI
    ("psqlodbc", PortSpelling::Key),      // psqlodbcw.so
    ("mysql", PortSpelling::Key),         // MySQL ODBC 8.0 Unicode Driver
    ("myodbc", PortSpelling::Key),        // libmyodbc8w.so
    ("mariadb", PortSpelling::Key),       // MariaDB ODBC 3.1 Driver
    ("maodbc", PortSpelling::Key),        // libmaodbc.so
    ("db2", PortSpelling::Key),           // IBM DB2 ODBC DRIVER, libdb2.dylib, db2cli64.dll
];
const DSN_KEYS: &[&str] = &["DSN", "FILEDSN"];
/// A key holding any of these is a secret: `PWD1` (MySQL's second factor),
/// `Pass`, `PSWD`, `PASSWD`, `sslpassword`, `priv_key_file_pwd`,
/// `private_key_passphrase`, `KeystoreSecret`, `AccessToken`, `passcode`.
/// Wide on purpose: a key read as a secret that is not one costs a refusal,
/// never a password on disk.
const SECRET_PARTS: &[&str] = &[
    "PWD",
    "PSWD",
    "PASS",
    "SECRET",
    "TOKEN",
    "ACCESSKEY",
    "APIKEY",
    "ACCOUNTKEY",
    "SHAREDKEY",
    "PRIVATEKEY",
    "CREDENTIAL",
];
// Other addresses the rewrite would not touch, so the driver could dial them,
// matched after `normalized`. A denylist, so it leans wide: a key read as an
// address that is not one costs a refusal, never a dial past the tunnel.
const OTHER_ADDRESS_KEYS: &[&str] = &[
    "DBQ",          // Oracle's TNS alias or descriptor
    "SERVICENAME",  // Db2's port alias
    "SOCKET",       // MySQL's and psqlODBC's Unix socket
    "A8",           // psqlODBC's abbreviation of Socket
    "PQOPT",        // psqlODBC's libpq options, which can carry host, hostaddr and port
    "D5",           // psqlODBC's abbreviation of Pqopt
    "ENABLEDNSSRV", // MySQL's: Server becomes a DNS SRV name
];
/// A key holding any of these, other than the host and port keys themselves,
/// may name another server or port by its spelling alone, in whichever
/// vendor's variant: `hostaddr`, `Host2`, `PortNumber`, Vertica's
/// `BackupServerNode`, SQL Server's failover partner, Db2's alternate servers,
/// client reroute and LDAP server, and the proxies of Snowflake's and Simba's
/// drivers.
const OTHER_ADDRESS_PARTS: &[&str] = &[
    "HOST",
    "SERVER",
    "ADDR",
    "PORT",
    "URL",
    "URI",
    "ENDPOINT",
    "FAILOVERPARTNER",
    "ALTERNATE",
    "REROUTE",
    "LDAP",
    "PROXY",
];
/// Keys that hold an address part and name no address: how SQL Server's
/// driver checks the certificate and the SPN (`TrustServerCertificate`,
/// `HostNameInCertificate`, `ServerSPN`, `ServerCertificate`), Db2's
/// certificate file and TLS switch (`SSLServerCertificate`, and `Security`,
/// which holds `URI`), and psqlODBC's prepare switch (`UseServerSidePrepare`).
const NOT_ADDRESSES: &[&str] = &[
    "TRUSTSERVERCERTIFICATE",
    "HOSTNAMEINCERTIFICATE",
    "SERVERSPN",
    "SERVERCERTIFICATE",
    "SSLSERVERCERTIFICATE",
    "SECURITY",
    "USESERVERSIDEPREPARE",
];

const DSN: &str = "A DSN keeps its server's address in odbc.ini, where DBDelve cannot route it through a tunnel. Write the connection string out to use one.";
const NO_HOST: &str = "The connection string names no host (HOSTNAME, HOST or SERVER), so there is nothing for the tunnel to forward to.";
const MANY_HOSTS: &str = "The connection string names its host more than once, and drivers disagree on which one wins, so DBDelve cannot tell where the tunnel should go.";
const MANY_PORTS: &str = "The connection string names its port more than once, so DBDelve cannot tell where the tunnel should go.";
const NO_PORT: &str =
    "Name the port (PORT=…, or Server=host,port) so the tunnel knows where to forward.";
const BAD_HOST: &str = "The host in the connection string is not one plain host name or address, which is all a tunnel can forward to.";
const BAD_PORT: &str = "The port in the connection string is not a number from 1 to 65535.";
const NO_DRIVER: &str = "Name the driver once (DRIVER=…), since where a driver reads its port decides where the tunnel's port goes.";
const UNKNOWN_DRIVER: &str = "DBDelve knows where the SQL Server, PostgreSQL, MySQL, MariaDB and Db2 drivers read their port, and not this DRIVER, so it cannot be sure the driver would dial the tunnel.";
const PORT_IN_HOST: &str = "Microsoft's SQL Server driver reads its port only after a comma in Server or Address (Server=host,port), so write it there.";
const PORT_AS_KEY: &str =
    "This driver reads no port after a comma in its host, so write the port as PORT=….";
const UNTERMINATED: &str = "The connection string has a { with no closing }.";
const AFTER_BRACE: &str = "The connection string has text after a closing } before the next ;.";
const BARE: &str = "The connection string has an entry that is not key=value.";
const EMPTY_KEY: &str = "The connection string has an entry with no key before the =.";
const ROUTE: &str = "This connection string could not be routed through the tunnel.";
const CONTROL: &str = "The connection string holds a control character, which drivers read differently, so DBDelve cannot be sure what it says.";
const AMBIGUOUS_BRACE: &str = "The connection string has a braced value holding ;, = or }}, or a { after a space, which drivers read differently, so DBDelve cannot be sure where the tunnel should go.";
const NOT_ODBC: &str = "This looks like a connection URL or another value that is not an ODBC connection string, and it could hold a password, which would be stored in plain text. Enter a DSN name or KEY=value pairs, and put the password in the Password field.";
const SECRET: &str = "The connection string holds a password or another secret, which would be stored in plain text. Put the password in the Password field, which is kept in the Keychain.";

struct Attribute {
    key: Range<usize>,
    /// Where the value is in the string, braces included when it has them.
    span: Range<usize>,
    text: String,
    braced: bool,
    /// Whitespace stood between `=` and `{`: a driver that opens a brace only
    /// right after `=` reads what is inside as attributes of their own.
    spaced: bool,
}

impl Attribute {
    fn is(&self, source: &str, keys: &[&str]) -> bool {
        keys.iter()
            .any(|key| source[self.key.clone()].eq_ignore_ascii_case(key))
    }

    fn is_secret(&self, source: &str) -> bool {
        names_secret(&source[self.key.clone()])
    }

    /// Whether this reads as another address once spaces and underscores
    /// are dropped, so `Failover Partner` is `Failover_Partner`.
    fn names_another_address(&self, source: &str) -> bool {
        if self.is(source, HOST_KEYS) || self.is(source, PORT_KEYS) {
            return false;
        }
        let key: String = source[self.key.clone()]
            .chars()
            .filter(|c| !matches!(c, ' ' | '_'))
            .collect::<String>()
            .to_ascii_uppercase();
        !NOT_ADDRESSES.contains(&key.as_str())
            && (OTHER_ADDRESS_KEYS.contains(&key.as_str())
                || OTHER_ADDRESS_PARTS.iter().any(|part| key.contains(part)))
    }

    /// What a driver that closes a brace at its first `}` could read
    /// differently: `;`, `=` and `}}` inside, or a brace after a space.
    fn ambiguous(&self, source: &str) -> bool {
        let raw = &source[self.span.clone()];
        let inner = raw.strip_prefix('{').unwrap_or(raw);
        let inner = inner.strip_suffix('}').unwrap_or(inner);
        self.braced && (self.spaced || inner.contains([';', '=']) || inner.contains("}}"))
    }
}

/// The attributes, and the first error, if any. Reading goes on past an error
/// from the next `;`, as a driver that skips what it cannot read would, so a
/// `PWD` after a broken entry is still found; only [`secrets`] uses what is
/// read after one. Every delimiter is ASCII, so each slice lands on a
/// character boundary.
fn scan(source: &str) -> (Vec<Attribute>, Option<&'static str>) {
    let bytes = source.as_bytes();
    let to_semicolon = |from: usize| {
        bytes[from..]
            .iter()
            .position(|&byte| byte == b';')
            .map_or(bytes.len(), |offset| from + offset)
    };
    let mut attributes = Vec::new();
    let mut error = None;
    let mut at = 0;
    loop {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b';') {
            at += 1;
        }
        if at >= bytes.len() {
            return (attributes, error);
        }
        let key_start = at;
        while at < bytes.len() && bytes[at] != b'=' && bytes[at] != b';' {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] == b';' {
            error.get_or_insert(BARE);
            continue;
        }
        let mut key_end = at;
        while key_end > key_start && bytes[key_end - 1].is_ascii_whitespace() {
            key_end -= 1;
        }
        if key_end == key_start {
            error.get_or_insert(EMPTY_KEY);
            at = to_semicolon(at);
            continue;
        }
        let equals = at;
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        let spaced = start > equals + 1;
        if bytes.get(at) != Some(&b'{') {
            at = to_semicolon(at);
            let mut end = at;
            while end > start && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            attributes.push(Attribute {
                key: key_start..key_end,
                span: start..end,
                text: source[start..end].to_string(),
                braced: false,
                spaced,
            });
            continue;
        }
        at += 1;
        let mut text = Vec::new();
        let closed = loop {
            match bytes.get(at) {
                None => break false,
                Some(b'}') if bytes.get(at + 1) == Some(&b'}') => {
                    text.push(b'}');
                    at += 2;
                }
                Some(b'}') => {
                    at += 1;
                    break true;
                }
                Some(&byte) => {
                    text.push(byte);
                    at += 1;
                }
            }
        };
        attributes.push(Attribute {
            key: key_start..key_end,
            span: start..at,
            text: String::from_utf8(text).expect("copied from a str, between ASCII braces"),
            braced: true,
            spaced,
        });
        if !closed {
            error.get_or_insert(UNTERMINATED);
            at = to_semicolon(start + 1);
            continue;
        }
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at < bytes.len() && bytes[at] != b';' {
            error.get_or_insert(AFTER_BRACE);
            at = to_semicolon(at);
        }
    }
}

/// The secret keys outside braces, as spelled, past any entry that does not
/// parse.
pub fn secrets(source: &str) -> Vec<&str> {
    scan(source)
        .0
        .into_iter()
        .filter(|attribute| attribute.is_secret(source))
        .map(|attribute| &source[attribute.key])
        .collect()
}

/// Whether the string may be written to `profiles.toml`: not when it names a
/// secret, and not when it does not parse cleanly or holds a control
/// character, since what this parser cannot read is where a driver could find
/// one. A secret-shaped `key=` inside a value is refused too, braced or not,
/// as a driver that reads braces differently, or libpq inside psqlODBC's
/// `pqopt`, would take it as one. A DSN, a name with no `=`, holds nothing.
pub fn storable(source: &str) -> Result<(), Refusal> {
    if source.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(Refusal(CONTROL.to_string()));
    }
    if !source.contains('=') {
        let is_dsn =
            !source.contains("://") && !source.chars().any(|c| "[]{}(),;?*=!@".contains(c));
        return if is_dsn {
            Ok(())
        } else {
            Err(Refusal(NOT_ODBC.to_string()))
        };
    }
    let (attributes, error) = scan(source);
    if attributes.iter().any(|attribute| {
        let key = &source[attribute.key.clone()];
        key.contains("://") || key.contains('@')
    }) {
        return Err(Refusal(NOT_ODBC.to_string()));
    }
    let hidden = attributes
        .iter()
        .any(|attribute| names_secret_inside(&attribute.text));
    if hidden || !secrets(source).is_empty() {
        return Err(Refusal(SECRET.to_string()));
    }
    error.map_or(Ok(()), |error| Err(Refusal(error.to_string())))
}

fn names_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    SECRET_PARTS.iter().any(|part| key.contains(part))
}

/// Whether `text` holds a `key=` whose key, the word just before the `=`, is a
/// secret's.
fn names_secret_inside(text: &str) -> bool {
    text.match_indices('=').any(|(at, _)| {
        let word = text[..at]
            .trim_end()
            .rsplit(|c: char| c.is_ascii_whitespace() || matches!(c, ';' | '{' | '}' | '=' | ','))
            .next()
            .unwrap_or_default();
        names_secret(word)
    })
}

/// Where in the string the driver's host and port are.
struct Located {
    host: usize,
    /// The port's own attribute, or `None` when it rides in the host value.
    port: Option<usize>,
    endpoint: Endpoint,
    /// Length of a `tcp:` prefix on the host value.
    prefix: usize,
}

fn locate(source: &str) -> Result<(Vec<Attribute>, Located), Refusal> {
    let refuse = |sentence: &str| Refusal(sentence.to_string());
    if !source.contains('=') {
        return Err(refuse(DSN));
    }
    if source.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(refuse(CONTROL));
    }
    let (attributes, error) = scan(source);
    if let Some(error) = error {
        return Err(refuse(error));
    }
    if attributes.iter().any(|a| a.is(source, DSN_KEYS)) {
        return Err(refuse(DSN));
    }
    if attributes.iter().any(|a| a.ambiguous(source)) {
        return Err(refuse(AMBIGUOUS_BRACE));
    }
    if let Some(other) = attributes.iter().find(|a| a.names_another_address(source)) {
        return Err(Refusal(format!(
            "{} names another address that the tunnel would not carry, so the driver could dial it directly. Remove it to use a tunnel.",
            source[other.key.clone()].to_ascii_uppercase()
        )));
    }
    let hosts: Vec<usize> = (0..attributes.len())
        .filter(|&i| attributes[i].is(source, HOST_KEYS))
        .collect();
    let ports: Vec<usize> = (0..attributes.len())
        .filter(|&i| attributes[i].is(source, PORT_KEYS))
        .collect();
    if hosts.len() > 1 {
        return Err(refuse(MANY_HOSTS));
    }
    if ports.len() > 1 {
        return Err(refuse(MANY_PORTS));
    }
    let &[host] = hosts.as_slice() else {
        return Err(refuse(NO_HOST));
    };
    let spelling = port_spelling(source, &attributes)?;
    let value = attributes[host].text.as_str();
    match spelling {
        PortSpelling::InHost
            if !ports.is_empty() || !attributes[host].is(source, COMMA_PORT_KEYS) =>
        {
            return Err(refuse(PORT_IN_HOST));
        }
        PortSpelling::Key if value.contains(',') => return Err(refuse(PORT_AS_KEY)),
        _ => {}
    }
    let prefix = match value.get(..4) {
        Some(head) if spelling == PortSpelling::InHost && head.eq_ignore_ascii_case("tcp:") => 4,
        _ => 0,
    };
    let rest = &value[prefix..];
    let (name, comma_port) = match rest.split_once(',') {
        Some((name, port)) if spelling == PortSpelling::InHost => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(refuse(BAD_HOST));
            }
            (name, Some(port))
        }
        _ => (rest, None),
    };
    // An allowlist rather than a list of what is wrong: `:`, `\`, `/`, `(`,
    // brackets and whitespace all fall out, and anything new a driver accepts
    // is refused instead of forwarded past.
    let plain = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !plain {
        return Err(refuse(BAD_HOST));
    }
    let (port_text, port_index) = match (comma_port, ports.first()) {
        (Some(text), None) => (text, None),
        (None, Some(&i)) => (attributes[i].text.as_str(), Some(i)),
        _ => return Err(refuse(NO_PORT)),
    };
    let port = port_text
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| refuse(BAD_PORT))?;
    let mut keys = vec![source[attributes[host].key.clone()].to_string()];
    keys.extend(port_index.map(|i| source[attributes[i].key.clone()].to_string()));
    let located = Located {
        host,
        port: port_index,
        endpoint: Endpoint {
            host: name.to_string(),
            port,
            keys,
        },
        prefix,
    };
    Ok((attributes, located))
}

/// Where the string's one `DRIVER` reads its port. A library path is matched
/// by its file name, so a directory that happens to name another driver does
/// not decide it; a name matching drivers that disagree is refused.
fn port_spelling(source: &str, attributes: &[Attribute]) -> Result<PortSpelling, Refusal> {
    let drivers: Vec<&Attribute> = attributes
        .iter()
        .filter(|a| a.is(source, &["DRIVER"]))
        .collect();
    let &[driver] = drivers.as_slice() else {
        return Err(Refusal(NO_DRIVER.to_string()));
    };
    let name = driver
        .text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut matched = DRIVERS
        .iter()
        .filter(|(needle, _)| name.contains(needle))
        .map(|&(_, spelling)| spelling);
    let first = matched.next();
    match first {
        Some(spelling) if matched.all(|other| other == spelling) => Ok(spelling),
        _ => Err(Refusal(UNKNOWN_DRIVER.to_string())),
    }
}

/// Whether the string turns on MySQL Connector/ODBC's `AUTO_RECONNECT`, which
/// opens a new session behind a lost one without a word, and a Read-only hold
/// set on the old one is not on the new. Anything but a plain off is read as
/// on, since the driver's own reading of the value is not known here.
pub fn reconnects_silently(source: &str) -> bool {
    scan(source).0.iter().any(|attribute| {
        let key: String = source[attribute.key.clone()]
            .chars()
            .filter(|c| !matches!(c, ' ' | '_'))
            .collect();
        let off = ["0", "no", "false", "off", ""]
            .iter()
            .any(|off| attribute.text.trim().eq_ignore_ascii_case(off));
        key.eq_ignore_ascii_case("AUTORECONNECT") && !off
    })
}

/// The one host and port the string makes the driver dial, or why it cannot be
/// said for certain.
pub fn endpoint(source: &str) -> Result<Endpoint, Refusal> {
    locate(source).map(|(_, located)| located.endpoint)
}

/// `source` with its host and port pointed at `local`, every other byte
/// unchanged. The result is read back and must name exactly `local`; the
/// rewrite and that check are one function so no caller gets one without the
/// other.
pub fn through_tunnel(source: &str, local: SocketAddr) -> Result<String, Refusal> {
    let (attributes, located) = locate(source)?;
    let host = &attributes[located.host];
    let new_host = match located.port {
        Some(_) => local.ip().to_string(),
        None => format!("{},{}", local.ip(), local.port()),
    };
    let new_host = format!("{}{new_host}", &host.text[..located.prefix]);
    let mut edits = vec![(host.span.clone(), spelled(&new_host, host.braced))];
    if let Some(i) = located.port {
        let port = &attributes[i];
        edits.push((
            port.span.clone(),
            spelled(&local.port().to_string(), port.braced),
        ));
    }
    edits.sort_by_key(|(span, _)| span.start);
    let mut rewritten = String::with_capacity(source.len());
    let mut copied = 0;
    for (span, text) in edits {
        rewritten.push_str(&source[copied..span.start]);
        rewritten.push_str(&text);
        copied = span.end;
    }
    rewritten.push_str(&source[copied..]);
    verify(source, &rewritten, local)?;
    Ok(rewritten)
}

fn spelled(value: &str, braced: bool) -> String {
    match braced {
        true => format!("{{{value}}}"),
        false => value.to_string(),
    }
}

/// `rewritten` must dial exactly `local` and differ from `original` in no
/// other attribute: not a value, not a key, not the order, not a count.
fn verify(original: &str, rewritten: &str, local: SocketAddr) -> Result<(), Refusal> {
    let refuse = || Refusal(ROUTE.to_string());
    let (before, was) = locate(original)?;
    let (after, now) = locate(rewritten).map_err(|_| refuse())?;
    if now.endpoint.host != local.ip().to_string()
        || now.endpoint.port != local.port()
        || (was.host, was.port) != (now.host, now.port)
        || before.len() != after.len()
    {
        return Err(refuse());
    }
    for (i, (a, b)) in before.iter().zip(&after).enumerate() {
        let endpoint_part = i == was.host || Some(i) == was.port;
        let same_key = original[a.key.clone()] == rewritten[b.key.clone()];
        if !same_key || (!endpoint_part && a.text != b.text) {
            return Err(refuse());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> SocketAddr {
        "127.0.0.1:61234".parse().unwrap()
    }

    fn found(source: &str) -> (String, u16, Vec<String>) {
        let e = endpoint(source).unwrap_or_else(|r| panic!("{source}: {}", r.0));
        (e.host, e.port, e.keys)
    }

    #[test]
    fn the_endpoint_is_found_however_the_string_is_spelled() {
        let keys = |names: &[&str]| names.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        for (source, host, port, spelled) in [
            (
                "DRIVER=Db2;HOSTNAME=db.example;PORT=50000;DATABASE=x",
                "db.example",
                50000,
                keys(&["HOSTNAME", "PORT"]),
            ),
            (
                "driver={PostgreSQL Unicode}; server = pg.internal ;Port=5432;",
                "pg.internal",
                5432,
                keys(&["server", "Port"]),
            ),
            (
                "Driver={SQL Server Native Client 11.0};Server=tcp:sql.example,1433;Database=d",
                "sql.example",
                1433,
                keys(&["Server"]),
            ),
            (
                "DRIVER=Db2;HOSTNAME={db.example};PORT={50000}",
                "db.example",
                50000,
                keys(&["HOSTNAME", "PORT"]),
            ),
            (
                "DRIVER={IBM DB2 ODBC DRIVER};HOSTNAME=h;PORT=50000",
                "h",
                50000,
                keys(&["HOSTNAME", "PORT"]),
            ),
            (
                "Driver={ODBC Driver 18 for SQL Server};Server=tcp:h,1433;Encrypt=yes",
                "h",
                1433,
                keys(&["Server"]),
            ),
            (
                "DRIVER={/opt/ibm/clidriver/lib/libdb2.dylib};HOSTNAME=h;PORT=1",
                "h",
                1,
                keys(&["HOSTNAME", "PORT"]),
            ),
            (
                "DRIVER=/opt/db2/psqlodbcw.so;Servername=h;Port=1",
                "h",
                1,
                keys(&["Servername", "Port"]),
            ),
            (
                "Driver={ODBC Driver 18 for SQL Server};Server=h,1;TrustServerCertificate=no;\
                 HostNameInCertificate=db.example;ServerSPN=MSSQLSvc/db.example",
                "h",
                1,
                keys(&["Server"]),
            ),
            (
                "DRIVER=Db2;HOSTNAME=h;PORT=1;Security=SSL;SSLServerCertificate=/c.arm",
                "h",
                1,
                keys(&["HOSTNAME", "PORT"]),
            ),
        ] {
            assert_eq!(found(source), (host.to_string(), port, spelled), "{source}");
        }
    }

    #[test]
    fn every_string_the_tunnel_cannot_be_sure_of_is_refused_in_its_own_words() {
        let other = |key: &str| {
            format!(
                "{key} names another address that the tunnel would not carry, so the driver could dial it directly. Remove it to use a tunnel."
            )
        };
        for (source, sentence) in [
            ("Db2Dev", DSN.to_string()),
            ("DSN=x;HOST=h;PORT=1", DSN.to_string()),
            ("FILEDSN=f;HOST=h;PORT=1", DSN.to_string()),
            ("DRIVER=x;PORT=1", NO_HOST.to_string()),
            ("HOST=a;HOSTNAME=b;PORT=1", MANY_HOSTS.to_string()),
            ("Server=a;Address=b;Port=1", MANY_HOSTS.to_string()),
            ("Server=a;Server=b;Port=1", MANY_HOSTS.to_string()),
            ("PORT=1;port=2", MANY_PORTS.to_string()),
            ("HOST=h;PORT=1", NO_DRIVER.to_string()),
            ("DRIVER=Db2;DRIVER=Db2;HOST=h;PORT=1", NO_DRIVER.to_string()),
            (
                "DRIVER={FreeTDS};Server=h;Port=1",
                UNKNOWN_DRIVER.to_string(),
            ),
            (
                "DRIVER={MySQL to SQL Server};Server=h;Port=1",
                UNKNOWN_DRIVER.to_string(),
            ),
            (
                "Driver={ODBC Driver 18 for SQL Server};Server=h;Port=1433",
                PORT_IN_HOST.to_string(),
            ),
            (
                "Driver={ODBC Driver 18 for SQL Server};Server=a,1433;Port=1433",
                PORT_IN_HOST.to_string(),
            ),
            (
                "Driver={ODBC Driver 18 for SQL Server};HOSTNAME=h,1",
                PORT_IN_HOST.to_string(),
            ),
            (
                "Driver={PostgreSQL Unicode};Servername=a,5432",
                PORT_AS_KEY.to_string(),
            ),
            ("DRIVER=Db2;HOST=a,1", PORT_AS_KEY.to_string()),
            ("DRIVER=Db2;HOST=h", NO_PORT.to_string()),
            ("Driver={SQL Server};Server=h", NO_PORT.to_string()),
            ("Driver={SQL Server};Server=a,b", BAD_HOST.to_string()),
            ("Driver={SQL Server};Server=a,1,2", BAD_HOST.to_string()),
            ("DRIVER=Db2;SERVER=h:3306", BAD_HOST.to_string()),
            (
                r"Driver={SQL Server};Server=h\SQLEXPRESS,1",
                BAD_HOST.to_string(),
            ),
            ("Driver={SQL Server};Server=np:h,1", BAD_HOST.to_string()),
            ("DRIVER=Db2;Server=tcp:h;Port=1", BAD_HOST.to_string()),
            ("DRIVER=Db2;Server=[::1];Port=1", BAD_HOST.to_string()),
            ("DRIVER=Db2;HOST=h/x;PORT=1", BAD_HOST.to_string()),
            ("DRIVER=Db2;HOST=h(x);PORT=1", BAD_HOST.to_string()),
            ("DRIVER=Db2;HOST={a b};PORT=1", BAD_HOST.to_string()),
            ("DRIVER=Db2;HOST=;PORT=1", BAD_HOST.to_string()),
            ("HOSTNAME=h;PORT=1;Host2=b", other("HOST2")),
            ("Servername=h;Port=1;hostaddr=10.0.0.1", other("HOSTADDR")),
            ("HostName=h;Port=1;PortNumber=2", other("PORTNUMBER")),
            (
                "Servername=h;Port=1;BackupServerNode=real2:5433",
                other("BACKUPSERVERNODE"),
            ),
            ("Server=h;Port=1;Server Name=b", other("SERVER NAME")),
            (
                "Server=h;Port=1;ConnectionEndpoint=b",
                other("CONNECTIONENDPOINT"),
            ),
            ("HOST=h;PORT=1;DBQ=x", other("DBQ")),
            ("HOSTNAME=h;SERVICENAME=50000", other("SERVICENAME")),
            ("Server=h;Port=1;Socket=/tmp/s", other("SOCKET")),
            ("Server=h,1;Failover_Partner=b", other("FAILOVER_PARTNER")),
            ("Server=h,1;FailoverPartner=b", other("FAILOVERPARTNER")),
            ("Server=h,1;AlternateServers=b", other("ALTERNATESERVERS")),
            ("Server=h,1;Url=b", other("URL")),
            ("Server=h,1;Failover Partner=b", other("FAILOVER PARTNER")),
            ("Servername=h;Port=1;pqopt=host=evil", other("PQOPT")),
            ("Servername=h;Port=1;D5=host=evil", other("D5")),
            ("Servername=h;Port=1;A8=/tmp", other("A8")),
            ("Server=h;Port=1;MULTI_HOST=1", other("MULTI_HOST")),
            ("Server=h;Port=1;ENABLE_DNS_SRV=1", other("ENABLE_DNS_SRV")),
            (
                "HOSTNAME=h;PORT=1;alternateServerName=b",
                other("ALTERNATESERVERNAME"),
            ),
            (
                "HOSTNAME=h;PORT=1;ClientRerouteAlternateServerName=b",
                other("CLIENTREROUTEALTERNATESERVERNAME"),
            ),
            (
                "HOSTNAME=h;PORT=1;LDAPServerHost=l",
                other("LDAPSERVERHOST"),
            ),
            ("Server=h;Port=1;proxy=p:8080", other("PROXY")),
            ("Server=h;Port=1;Proxy Host=p", other("PROXY HOST")),
            ("Driver={a}}b;c};Host=h;Port=1", AMBIGUOUS_BRACE.to_string()),
            (
                "Note={a}};HOST=evil;b};HOST=db;PORT=1",
                AMBIGUOUS_BRACE.to_string(),
            ),
            (
                "Note= {a;HOST=evil;};HOST=db;PORT=1",
                AMBIGUOUS_BRACE.to_string(),
            ),
            ("Driver={a=b};HOST=h;PORT=1", AMBIGUOUS_BRACE.to_string()),
            ("Driver={a;b};HOST=h;PORT=1", AMBIGUOUS_BRACE.to_string()),
            ("\x0BHOST=evil;HOST=db;PORT=1", CONTROL.to_string()),
            ("HOST=db;PORT=1\0;HOST=evil", CONTROL.to_string()),
            ("HOST=db;\tPORT=1", CONTROL.to_string()),
            ("DRIVER=Db2;HOST=h;PORT=0", BAD_PORT.to_string()),
            ("DRIVER=Db2;HOST=h;PORT=70000", BAD_PORT.to_string()),
            ("DRIVER=Db2;HOST=h;PORT=x", BAD_PORT.to_string()),
            ("Driver={x;HOST=h;PORT=1", UNTERMINATED.to_string()),
            ("Driver={x}y;HOST=h;PORT=1", AFTER_BRACE.to_string()),
            ("HOST=h;PORT=1;stray", BARE.to_string()),
            ("=x;HOST=h;PORT=1", EMPTY_KEY.to_string()),
        ] {
            assert_eq!(endpoint(source), Err(Refusal(sentence)), "{source}");
        }
    }

    #[test]
    fn the_rewrite_changes_the_host_and_port_and_nothing_else() {
        for (source, expected) in [
            (
                "DRIVER=Db2;HOSTNAME=db.example;PORT=50000;UID=a",
                "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a",
            ),
            (
                "Driver={SQL Server};Server=tcp:sql.example,1433;Encrypt=yes",
                "Driver={SQL Server};Server=tcp:127.0.0.1,61234;Encrypt=yes",
            ),
            (
                "DRIVER=Db2;HOSTNAME={db.example};PORT={50000}",
                "DRIVER=Db2;HOSTNAME={127.0.0.1};PORT={61234}",
            ),
            (
                " driver ={IBM DB2 ODBC DRIVER} ;  Host =  h ;Port= 5 ;;PWD=",
                " driver ={IBM DB2 ODBC DRIVER} ;  Host =  127.0.0.1 ;Port= 61234 ;;PWD=",
            ),
            (
                "PORT=1;HOST=h;DRIVER=Db2",
                "PORT=61234;HOST=127.0.0.1;DRIVER=Db2",
            ),
        ] {
            assert_eq!(through_tunnel(source, local()).as_deref(), Ok(expected));
        }
    }

    #[test]
    fn auto_reconnect_is_on_unless_it_is_plainly_off() {
        for on in [
            "DRIVER=x;AUTO_RECONNECT=1",
            "auto_reconnect = yes",
            "AutoReconnect=TRUE",
            "AUTO_RECONNECT={2}",
        ] {
            assert!(reconnects_silently(on), "{on}");
        }
        for off in [
            "DRIVER=x;AUTO_RECONNECT=0",
            "AUTO_RECONNECT=off",
            "DRIVER=x",
        ] {
            assert!(!reconnects_silently(off), "{off}");
        }
    }

    #[test]
    fn a_refused_string_is_not_rewritten() {
        assert_eq!(
            through_tunnel("Db2Dev", local()),
            Err(Refusal(DSN.to_string()))
        );
    }

    #[test]
    fn the_check_after_rewriting_refuses_anything_but_the_tunnel() {
        let original = "DRIVER=Db2;HOSTNAME=db.example;PORT=50000;UID=a";
        let routed = Refusal(ROUTE.to_string());
        for tampered in [
            "DRIVER=Db2;HOSTNAME=db.example;PORT=61234;UID=a",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=50000;UID=a",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=b",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a;PWD=x",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a;HOST=other",
            "DRIVER=Db2;PORT=61234;HOSTNAME=127.0.0.1;UID=a",
            "DRIVER=Db3;HOSTNAME=127.0.0.1;PORT=61234;UID=a",
            "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a;DBQ=x",
        ] {
            assert_eq!(
                verify(original, tampered, local()),
                Err(routed.clone()),
                "{tampered}"
            );
        }
        assert_eq!(
            verify(
                original,
                "DRIVER=Db2;HOSTNAME=127.0.0.1;PORT=61234;UID=a",
                local()
            ),
            Ok(())
        );
    }

    #[test]
    fn secrets_are_the_secret_shaped_keys_outside_braces() {
        assert_eq!(secrets("DSN=x;PWD=y"), ["PWD"]);
        assert_eq!(
            secrets("PWD1=a;UID=u;sslpassword=b;AccessToken=c"),
            ["PWD1", "sslpassword", "AccessToken"]
        );
        assert!(secrets("Driver={a;PWD=b};UID=u").is_empty());
        assert_eq!(secrets("uid=u; Password =p;pwd=q"), ["Password", "pwd"]);
        assert_eq!(secrets("PWD=y;Driver={broken"), ["PWD"]);
        // Past an entry that does not parse, as a driver skipping it reads on.
        for source in [
            "Driver=x;junk;PWD=hunter2",
            "Driver={broken;PWD=hunter2",
            "=x;PWD=hunter2",
            "Driver={x}y;PWD=hunter2",
        ] {
            assert_eq!(secrets(source), ["PWD"], "{source}");
        }
    }

    #[test]
    fn only_a_string_that_parses_cleanly_and_names_no_password_is_stored() {
        for source in [
            "Db2Dev",
            "DRIVER={IBM DB2 ODBC DRIVER};HOSTNAME=h;PORT=50000;UID=a@b.example",
            "Servername=h;pqopt={sslcert=/x sslkey=/y}",
            "Driver=x;KeyFilePath=/k",
        ] {
            assert_eq!(storable(source), Ok(()), "{source}");
        }
        for (source, sentence) in [
            ("DSN=x;PWD=y", SECRET),
            ("Driver=x;junk;PWD=hunter2", SECRET),
            ("Note= {a;PWD=x}", SECRET),
            ("Driver={a;PWD=b};UID=u", SECRET),
            ("Servername=h;pqopt={password = x}", SECRET),
            ("Servername=h;pqopt=password=hunter2", SECRET),
            ("Servername=h;pqopt={sslpassword=x}", SECRET),
            ("Driver=x;PWD1=x", SECRET),
            ("Driver=x;sslpassword=x", SECRET),
            ("Driver=x;priv_key_file_pwd=x", SECRET),
            ("Driver=x;KeystoreSecret=x", SECRET),
            ("Driver=x;AccessToken=x", SECRET),
            ("Driver=x;token=x", SECRET),
            ("Driver=x;passcode=x", SECRET),
            ("Driver=x;Pass=x", SECRET),
            ("Driver=x;PSWD=x", SECRET),
            ("Driver=x;passwd=x", SECRET),
            ("Driver=x;private_key_passphrase=x", SECRET),
            ("Servername=h;pqopt={pass=x}", SECRET),
            ("Driver=x;AccessKey=x", SECRET),
            ("Driver=x;ApiKey=x", SECRET),
            ("Driver=x;AccountKey=x", SECRET),
            ("Driver=x;SharedAccessKey=x", SECRET),
            ("Driver=x;PrivateKey=x", SECRET),
            ("Driver=x;Credential=x", SECRET),
            ("postgresql://app:hunter2@db/app", NOT_ODBC),
            ("postgresql://app:hunter2@db/app?sslmode=require", NOT_ODBC),
            ("Driver=x;a@b=c", NOT_ODBC),
            ("\x0BPWD=x", CONTROL),
            ("Driver=x;\tUID=u", CONTROL),
            ("Driver=x;junk", BARE),
            ("Driver={x", UNTERMINATED),
            ("Driver={x}y", AFTER_BRACE),
        ] {
            assert_eq!(
                storable(source),
                Err(Refusal(sentence.to_string())),
                "{source}"
            );
        }
    }
}

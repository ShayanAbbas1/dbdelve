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
const DSN_KEYS: &[&str] = &["DSN", "FILEDSN"];
// Other addresses the rewrite would not touch, so the driver could dial them.
const OTHER_ADDRESS_KEYS: &[&str] = &[
    "DBQ",              // Oracle's TNS alias or descriptor
    "SERVICENAME",      // Db2's port alias
    "SOCKET",           // MySQL's Unix socket
    "FAILOVER_PARTNER", // SQL Server's mirror
    "FAILOVERPARTNER",  // the same, spelled without the underscore
    "ALTERNATESERVERS", // a list of further servers
    "URL",
];

const DSN: &str = "A DSN keeps its server's address in odbc.ini, where DBDelve cannot route it through a tunnel. Write the connection string out to use one.";
const NO_HOST: &str = "The connection string names no host (HOSTNAME, HOST or SERVER), so there is nothing for the tunnel to forward to.";
const MANY_HOSTS: &str = "The connection string names its host more than once, and drivers disagree on which one wins, so DBDelve cannot tell where the tunnel should go.";
const MANY_PORTS: &str = "The connection string names its port more than once, so DBDelve cannot tell where the tunnel should go.";
const NO_PORT: &str =
    "Name the port (PORT=…, or Server=host,port) so the tunnel knows where to forward.";
const BAD_HOST: &str = "The host in the connection string is not one plain host name or address, which is all a tunnel can forward to.";
const BAD_PORT: &str = "The port in the connection string is not a number from 1 to 65535.";
const UNTERMINATED: &str = "The connection string has a { with no closing }.";
const AFTER_BRACE: &str = "The connection string has text after a closing } before the next ;.";
const BARE: &str = "The connection string has an entry that is not key=value.";
const EMPTY_KEY: &str = "The connection string has an entry with no key before the =.";
const ROUTE: &str = "This connection string could not be routed through the tunnel.";

struct Attribute {
    key: Range<usize>,
    /// Where the value is in the string, braces included when it has them.
    span: Range<usize>,
    text: String,
    braced: bool,
}

impl Attribute {
    fn is(&self, source: &str, keys: &[&str]) -> bool {
        keys.iter()
            .any(|key| source[self.key.clone()].eq_ignore_ascii_case(key))
    }
}

/// The attributes read before the first error, and the error, if any. Every
/// delimiter is ASCII, so each slice lands on a character boundary.
fn scan(source: &str) -> (Vec<Attribute>, Option<&'static str>) {
    let bytes = source.as_bytes();
    let mut attributes = Vec::new();
    let mut at = 0;
    loop {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b';') {
            at += 1;
        }
        if at >= bytes.len() {
            return (attributes, None);
        }
        let key_start = at;
        while at < bytes.len() && bytes[at] != b'=' && bytes[at] != b';' {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] == b';' {
            return (attributes, Some(BARE));
        }
        let mut key_end = at;
        while key_end > key_start && bytes[key_end - 1].is_ascii_whitespace() {
            key_end -= 1;
        }
        if key_end == key_start {
            return (attributes, Some(EMPTY_KEY));
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        let (end, text, braced) = if bytes.get(at) == Some(&b'{') {
            at += 1;
            let mut text = Vec::new();
            loop {
                match bytes.get(at) {
                    None => return (attributes, Some(UNTERMINATED)),
                    Some(b'}') if bytes.get(at + 1) == Some(&b'}') => {
                        text.push(b'}');
                        at += 2;
                    }
                    Some(b'}') => {
                        at += 1;
                        break;
                    }
                    Some(byte) => {
                        text.push(*byte);
                        at += 1;
                    }
                }
            }
            let end = at;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if at < bytes.len() && bytes[at] != b';' {
                return (attributes, Some(AFTER_BRACE));
            }
            let text = String::from_utf8(text).expect("copied from a str, between ASCII braces");
            (end, text, true)
        } else {
            while at < bytes.len() && bytes[at] != b';' {
                at += 1;
            }
            let mut end = at;
            while end > start && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            (end, source[start..end].to_string(), false)
        };
        attributes.push(Attribute {
            key: key_start..key_end,
            span: start..end,
            text,
            braced,
        });
    }
}

/// The `PWD` / `PASSWORD` keys outside braces, as spelled, so the form can
/// refuse to save a string with a secret in it. Reads as far as the string
/// parses: a string that breaks later still has its earlier secrets named.
pub fn secrets(source: &str) -> Vec<&str> {
    scan(source)
        .0
        .into_iter()
        .filter(|attribute| attribute.is(source, &["PWD", "PASSWORD"]))
        .map(|attribute| &source[attribute.key])
        .collect()
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
    let (attributes, error) = scan(source);
    if let Some(error) = error {
        return Err(refuse(error));
    }
    if attributes.iter().any(|a| a.is(source, DSN_KEYS)) {
        return Err(refuse(DSN));
    }
    if let Some(other) = attributes.iter().find(|a| a.is(source, OTHER_ADDRESS_KEYS)) {
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
    let value = attributes[host].text.as_str();
    let prefix = match value.get(..4) {
        Some(head) if head.eq_ignore_ascii_case("tcp:") => 4,
        _ => 0,
    };
    let rest = &value[prefix..];
    let (name, comma_port) = match rest.split_once(',') {
        Some((name, port)) if attributes[host].is(source, COMMA_PORT_KEYS) => {
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
    if comma_port.is_some() && !ports.is_empty() {
        return Err(refuse(MANY_PORTS));
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
                "Server=tcp:sql.example,1433;Database=d",
                "sql.example",
                1433,
                keys(&["Server"]),
            ),
            (
                "HOSTNAME={db.example};PORT={50000}",
                "db.example",
                50000,
                keys(&["HOSTNAME", "PORT"]),
            ),
            (
                "Driver={a}}b;c};Host=h;Port=1",
                "h",
                1,
                keys(&["Host", "Port"]),
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
            ("Server=a,1433;Port=1433", MANY_PORTS.to_string()),
            ("Server=h", NO_PORT.to_string()),
            ("Server=a,b;Port=1", BAD_HOST.to_string()),
            ("Server=a,1,2", BAD_HOST.to_string()),
            ("HOST=a,1", BAD_HOST.to_string()),
            ("SERVER=h:3306", BAD_HOST.to_string()),
            (r"Server=h\SQLEXPRESS;Port=1", BAD_HOST.to_string()),
            ("Server=np:h,1", BAD_HOST.to_string()),
            ("Server=[::1];Port=1", BAD_HOST.to_string()),
            ("HOST=h/x;PORT=1", BAD_HOST.to_string()),
            ("HOST=h(x);PORT=1", BAD_HOST.to_string()),
            ("HOST={a b};PORT=1", BAD_HOST.to_string()),
            ("HOST=;PORT=1", BAD_HOST.to_string()),
            ("HOST=h;PORT=1;DBQ=x", other("DBQ")),
            ("HOSTNAME=h;SERVICENAME=50000", other("SERVICENAME")),
            ("Server=h;Port=1;Socket=/tmp/s", other("SOCKET")),
            ("Server=h,1;Failover_Partner=b", other("FAILOVER_PARTNER")),
            ("Server=h,1;FailoverPartner=b", other("FAILOVERPARTNER")),
            ("Server=h,1;AlternateServers=b", other("ALTERNATESERVERS")),
            ("Server=h,1;Url=b", other("URL")),
            ("HOST=h;PORT=0", BAD_PORT.to_string()),
            ("HOST=h;PORT=70000", BAD_PORT.to_string()),
            ("HOST=h;PORT=x", BAD_PORT.to_string()),
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
                "Server=tcp:sql.example,1433;Encrypt=yes",
                "Server=tcp:127.0.0.1,61234;Encrypt=yes",
            ),
            (
                "HOSTNAME={db.example};PORT={50000}",
                "HOSTNAME={127.0.0.1};PORT={61234}",
            ),
            (
                " driver = {x;y} ;  Host =  h ;Port= 5 ;;PWD=",
                " driver = {x;y} ;  Host =  127.0.0.1 ;Port= 61234 ;;PWD=",
            ),
            ("PORT=1;HOST=h", "PORT=61234;HOST=127.0.0.1"),
        ] {
            assert_eq!(through_tunnel(source, local()).as_deref(), Ok(expected));
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
    fn secrets_are_the_password_keys_outside_braces() {
        assert_eq!(secrets("DSN=x;PWD=y"), ["PWD"]);
        assert!(secrets("Driver={a;PWD=b};UID=u").is_empty());
        assert_eq!(secrets("uid=u; Password =p;pwd=q"), ["Password", "pwd"]);
        assert_eq!(secrets("PWD=y;Driver={broken"), ["PWD"]);
    }
}

//! A connection through an engine plugin: one child process per connection,
//! spoken to in `dbdelve-plugin-protocol` frames.
//!
//! A plugin that dies or breaks the protocol loses its connection, which is
//! reported as a reset and never reopened or replayed here: the workspace's own
//! reconnect opens a fresh one and re-applies the Read-only hold before
//! anything else runs on it.

#[cfg(test)]
mod echo;
mod host;
mod rows;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use dbdelve_plugin_protocol::limits::{HELLO_DEADLINE, OPEN_DEADLINE};
use dbdelve_plugin_protocol::{PROTOCOL, Password, Request, Response};

use super::plugin::Plugin;
use super::{DbError, Engine};
use host::{Host, Routed, failure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalConfig {
    pub plugin: &'static Plugin,
    /// The built-in dialect DBDelve writes SQL in for this connection.
    pub speaks: Engine,
    /// As the user typed it; the plugin dials it.
    pub connection: String,
    pub user: String,
    pub password: String,
    /// Seconds, 0 for none.
    pub statement_timeout: u32,
}

/// What the plugin said about the connection it opened.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Opened {
    pub(super) dbms_name: String,
    pub(super) dbms_version: String,
    pub(super) driver: String,
    pub(super) stops_early: bool,
    /// The optional messages both `hello` and `opened` listed: the only ones
    /// this connection ever sends.
    pub(super) optional: Vec<String>,
}

#[derive(Clone)]
pub struct Connection {
    host: Arc<Host>,
    opened: Arc<Opened>,
}

impl Connection {
    pub fn open(config: &ExternalConfig) -> Result<Self, DbError> {
        Self::handshake(Host::start(config.plugin)?, config)
    }

    fn handshake(host: Arc<Host>, config: &ExternalConfig) -> Result<Self, DbError> {
        let name = &host.plugin.name;
        let hello = Request::Hello { protocol: PROTOCOL };
        let Response::Hello {
            protocol, optional, ..
        } = ask(&host, hello, "hello", HELLO_DEADLINE)?
        else {
            return Err(unexpected(&host, "hello"));
        };
        if protocol != PROTOCOL {
            return Err(failure(format!(
                "The {name} plugin speaks protocol {protocol}; this DBDelve speaks {PROTOCOL}."
            )));
        }

        let fields = BTreeMap::from([
            ("connection".to_owned(), config.connection.clone()),
            ("user".to_owned(), config.user.clone()),
        ]);
        let prepare = Request::Prepare {
            fields: fields.clone(),
        };
        let Response::Prepared { .. } = ask(&host, prepare, "prepare", HELLO_DEADLINE)? else {
            return Err(unexpected(&host, "prepare"));
        };

        let open = Request::Open {
            fields,
            password: Password(config.password.clone()),
            statement_timeout: config.statement_timeout,
        };
        let Response::Opened {
            dbms_name,
            dbms_version,
            driver,
            stops_early,
            optional: offered,
        } = ask(&host, open, "open", OPEN_DEADLINE)?
        else {
            return Err(unexpected(&host, "open"));
        };
        let opened = Opened {
            dbms_name,
            dbms_version,
            driver,
            stops_early,
            optional: offered
                .into_iter()
                .filter(|message| optional.contains(message))
                .collect(),
        };
        Ok(Self {
            host,
            opened: Arc::new(opened),
        })
    }

    pub(super) fn opened(&self) -> &Opened {
        &self.opened
    }

    pub fn is_lost(&self) -> bool {
        self.host.is_reset()
    }
}

/// One step of opening: its answer, or the error that ends the open. A plugin
/// that dies on the way is reported with its stderr, since a missing driver
/// library says why there and nowhere else.
fn ask(host: &Host, request: Request, what: &str, deadline: Duration) -> Result<Response, DbError> {
    let (_, answers) = host.request(request).map_err(|_| host.startup_error())?;
    match answers.recv_timeout(deadline) {
        Ok(Routed::Control(Response::Error { message, .. })) => Err(failure(message)),
        Ok(Routed::Control(response)) => Ok(response),
        Ok(Routed::Data(_)) => Err(unexpected(host, what)),
        Ok(Routed::Reset(_)) | Err(RecvTimeoutError::Disconnected) => Err(host.startup_error()),
        Err(RecvTimeoutError::Timeout) => {
            host.kill();
            Err(failure(format!(
                "The {} plugin did not answer `{what}` within {} s.",
                host.plugin.name,
                deadline.as_secs()
            )))
        }
    }
}

fn unexpected(host: &Host, what: &str) -> DbError {
    host.kill();
    failure(format!(
        "The {} plugin answered `{what}` with something protocol {PROTOCOL} does not allow there.",
        host.plugin.name
    ))
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use dbdelve_plugin_protocol::limits::CLOSE_GRACE;

    use super::*;

    fn config() -> ExternalConfig {
        ExternalConfig {
            plugin: echo::plugin(),
            speaks: Engine::Postgres,
            connection: "DSN=echo".to_owned(),
            user: "someone@example.com".to_owned(),
            password: String::new(),
            statement_timeout: 0,
        }
    }

    fn open_with(env: &[(&str, &str)]) -> Result<Connection, DbError> {
        let plugin = echo::plugin();
        let mut command = Command::new(&plugin.executable);
        command.envs(env.iter().copied());
        Connection::handshake(Host::spawn(plugin, command)?, &config())
    }

    fn gone_within(pid: u32, deadline: Duration) -> bool {
        let start = Instant::now();
        while echo::alive(pid) {
            if start.elapsed() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn open_answers_and_dropping_the_last_handle_ends_the_process() {
        let connection = Connection::open(&config()).unwrap();
        assert_eq!(
            *connection.opened(),
            Opened {
                dbms_name: "Echo".to_owned(),
                dbms_version: env!("CARGO_PKG_VERSION").to_owned(),
                driver: "dbdelve-echo".to_owned(),
                stops_early: true,
                optional: vec!["references".to_owned()],
            }
        );
        let pid = connection.host.pid().unwrap();
        assert!(echo::alive(pid));

        let clone = connection.clone();
        drop(connection);
        assert!(echo::alive(pid), "a clone still holds the connection");
        drop(clone);
        assert!(gone_within(pid, CLOSE_GRACE + Duration::from_secs(1)));
    }

    #[test]
    fn an_optional_message_only_one_side_lists_is_not_negotiated() {
        let connection = open_with(&[("DBDELVE_ECHO_PLAIN", "1")]).unwrap();
        assert!(connection.opened().optional.is_empty());
    }

    #[test]
    fn a_protocol_mismatch_fails_the_open_naming_both() {
        let error = open_with(&[("DBDELVE_ECHO_PROTOCOL", "99")]).err().unwrap();
        assert_eq!(
            error.message,
            "The Echo plugin speaks protocol 99; this DBDelve speaks 1."
        );
    }

    #[test]
    fn a_plugin_that_dies_before_hello_says_why_through_its_stderr() {
        let said = "libodbc.so.2: cannot open shared object file";
        let error = open_with(&[("DBDELVE_ECHO_STDERR", said)]).err().unwrap();
        assert_eq!(
            error.message,
            format!("The Echo plugin could not start: {said}")
        );
    }

    #[test]
    fn a_missing_executable_fails_the_open() {
        let plugin = Box::leak(Box::new(Plugin {
            id: "echo".to_owned(),
            name: "Echo".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executable: echo::plugin().executable.with_file_name("dbdelve-nowhere"),
        }));
        let error = Connection::open(&ExternalConfig { plugin, ..config() })
            .err()
            .unwrap();
        assert!(
            error
                .message
                .starts_with("The Echo plugin could not start: "),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_violation_after_open_is_a_reset_and_ends_the_process() {
        for statement in ["garbage", "oversize"] {
            let connection = Connection::open(&config()).unwrap();
            let pid = connection.host.pid().unwrap();
            let query = Request::Query {
                sql: statement.to_owned(),
                eligible: false,
            };
            let (_, answers) = connection.host.request(query).unwrap();
            let Ok(Routed::Reset(cause)) = answers.recv_timeout(Duration::from_secs(5)) else {
                panic!("{statement}: no reset");
            };
            assert!(
                cause.starts_with("The Echo plugin broke protocol 1 ("),
                "{statement}: {cause}"
            );
            assert!(connection.is_lost());
            assert!(
                !echo::alive(pid),
                "{statement}: the plugin outlived its reset"
            );

            let again = connection.host.request(Request::Catalog).err().unwrap();
            assert!(
                again.message.starts_with(&format!(
                    "{cause} The connection was reset: any open transaction"
                )),
                "{statement}: {}",
                again.message
            );
            assert!(!again.message.contains("committed is unknown"));
            let running = connection.host.reset_error(true).message;
            assert!(
                running.ends_with("Whether the statement that was running committed is unknown.")
            );
        }
    }

    /// Past the window echo waits for grants, so reaching `end` proves each
    /// one was counted and none was spent before the reader knew of it.
    #[test]
    fn a_read_larger_than_the_window_flows_on_granted_credit() {
        use dbdelve_plugin_protocol::limits::WINDOW;

        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "rows 1000000".to_owned(),
            eligible: false,
        };
        let (id, answers) = connection.host.request(query).unwrap();
        let mut received = 0;
        let rows = loop {
            match answers.recv_timeout(Duration::from_secs(10)).unwrap() {
                Routed::Data(payload) => {
                    received += payload.len() as u64;
                    let bytes = payload.len() as u64;
                    connection.host.send(id, Request::Credit { bytes });
                }
                Routed::Control(Response::End { rows, .. }) => break rows,
                Routed::Control(other) => panic!("{other:?}"),
                Routed::Reset(cause) => panic!("{cause}"),
            }
        };
        assert_eq!(rows, 1_000_000);
        assert!(
            received > 2 * WINDOW,
            "{received} bytes never needed a grant"
        );
        assert!(!connection.is_lost());
    }

    #[test]
    fn data_beyond_credit_is_a_reset() {
        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "flood".to_owned(),
            eligible: false,
        };
        let (_, answers) = connection.host.request(query).unwrap();
        let cause = loop {
            match answers.recv_timeout(Duration::from_secs(10)).unwrap() {
                Routed::Data(_) => {}
                Routed::Reset(cause) => break cause,
                Routed::Control(other) => panic!("{other:?}"),
            }
        };
        assert!(cause.contains("bytes of credit left"), "{cause}");
        assert!(connection.is_lost());
    }

    #[test]
    fn an_exit_after_open_is_reported_with_its_status() {
        let connection = Connection::open(&config()).unwrap();
        let query = Request::Query {
            sql: "exit 3".to_owned(),
            eligible: false,
        };
        let (_, answers) = connection.host.request(query).unwrap();
        let Ok(Routed::Reset(cause)) = answers.recv_timeout(Duration::from_secs(5)) else {
            panic!("no reset");
        };
        // ExitStatus's own wording differs on Windows; the prefix is ours.
        assert!(cause.starts_with("The Echo plugin stopped ("), "{cause}");
        assert!(cause.contains('3'), "{cause}");
        assert!(connection.is_lost());
    }

    #[test]
    fn stdin_eof_ends_echo() {
        let mut child = Command::new(&echo::plugin().executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        // Generous because macOS has no pipe2: std marks a pipe close-on-exec
        // only after creating it, so a plugin another test spawns in between
        // inherits this stdin's write end and holds EOF off until it exits.
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(15) {
                let _ = child.kill();
                panic!("echo outlived its stdin");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

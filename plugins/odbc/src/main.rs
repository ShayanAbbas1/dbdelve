//! The ODBC bridge: an engine plugin that reaches any database an ODBC driver
//! serves, through the platform's driver manager (unixODBC linked in on macOS,
//! the system's elsewhere).
//!
//! The main thread reads stdin frames for as long as the host writes them, so
//! `credit`, `stop` and `cancel` reach a running statement; one worker thread
//! answers every other request, in order, and is the only writer of stdout.
//! It exits on stdin EOF, as every plugin must.

mod catalog;
mod connect;
mod failure;
mod raw;
mod run;

use std::collections::BTreeMap;
use std::io::{self, BufWriter, Read, Write};
use std::process;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use dbdelve_plugin_protocol::{
    ErrorKind, Kind, MAX_PAYLOAD, PROTOCOL, REFERENCES, Request, Response, Tls, Violation,
    read_frame,
};
use odbc_api::handles;

use crate::connect::Session;
use crate::run::Control;

fn main() {
    let control = Arc::new(Control::default());
    let (jobs, inbox) = mpsc::channel();
    let worker = Arc::clone(&control);
    thread::spawn(move || {
        Bridge::new(BufWriter::new(io::stdout().lock()), worker).work(inbox);
        process::exit(0);
    });
    match serve(&mut io::stdin().lock(), &jobs, &control) {
        Ok(()) => process::exit(0),
        Err(violation) => {
            eprintln!("dbdelve-odbc: {violation}");
            process::exit(2);
        }
    }
}

enum Job {
    Run(u64, Request),
    Refuse(u64, ErrorKind, String),
}

/// Reads frames until EOF, or until the worker has answered `close`.
fn serve(input: &mut impl Read, jobs: &Sender<Job>, control: &Control) -> Result<(), Violation> {
    while let Some(frame) = read_frame(input)? {
        let id = frame.request;
        let job = match frame.kind {
            Kind::Data => Job::Refuse(
                id,
                ErrorKind::Protocol,
                "The ODBC bridge takes no data frames.".to_string(),
            ),
            Kind::Control => match Request::from_payload(&frame.payload) {
                Ok(Request::Credit { bytes }) => {
                    control.credit(id, bytes);
                    continue;
                }
                Ok(Request::Stop) => {
                    control.stop(id);
                    continue;
                }
                Ok(Request::Cancel) => {
                    control.cancel(id);
                    continue;
                }
                Ok(request) => {
                    if matches!(request, Request::Query { .. }) {
                        control.start(id);
                    }
                    Job::Run(id, request)
                }
                Err(violation) => Job::Refuse(id, ErrorKind::Unsupported, violation.to_string()),
            },
        };
        if jobs.send(job).is_err() {
            break;
        }
    }
    Ok(())
}

struct Bridge<W: Write> {
    out: W,
    control: Arc<Control>,
    environment: Option<&'static handles::Environment>,
    session: Option<Session>,
}

impl<W: Write> Bridge<W> {
    /// Nothing touches the driver manager until the first `open`.
    fn new(out: W, control: Arc<Control>) -> Self {
        Self {
            out,
            control,
            environment: None,
            session: None,
        }
    }

    /// Answers jobs until the reader is gone or `close` is answered, and hands
    /// back the output.
    fn work(mut self, inbox: Receiver<Job>) -> W {
        for job in inbox {
            match job {
                Job::Run(id, Request::Close) => {
                    self.session = None;
                    self.reply(id, Response::Closed);
                    break;
                }
                Job::Run(id, request) => {
                    let response = self.run(id, request);
                    self.reply(id, response);
                }
                Job::Refuse(id, kind, message) => self.reply(id, refusal(kind, message)),
            }
        }
        self.out
    }

    fn run(&mut self, id: u64, request: Request) -> Response {
        match request {
            Request::Hello { .. } => Response::Hello {
                protocol: PROTOCOL,
                implementation: format!("dbdelve-odbc {}", env!("CARGO_PKG_VERSION")),
                optional: vec![REFERENCES.to_string()],
            },
            // The driver verifies whatever host the string names, and the
            // bridge has no way to give it another name to check.
            Request::Prepare { .. } => Response::Prepared {
                tls: Tls {
                    separate_name: false,
                    name_in_verify_ca: true,
                },
            },
            Request::Open {
                fields,
                password,
                statement_timeout,
            } => self
                .open(&fields, &password.0, statement_timeout)
                .unwrap_or_else(|error| error),
            // `eligible` is not read: the bridge always drains (`stops_early`).
            Request::Query { sql, .. } => match &self.session {
                Some(session) => run::query(session, &self.control, &mut self.out, id, &sql),
                None => not_open(),
            },
            Request::Catalog => self.catalog(catalog::catalog),
            Request::Structure { schema, relation } => {
                self.catalog(|session| catalog::structure(session, &schema, &relation))
            }
            Request::References { schema, relation } => {
                self.catalog(|session| catalog::references(session, &schema, &relation))
            }
            _ => refusal(
                ErrorKind::Unsupported,
                "The ODBC bridge does not answer this request.".to_string(),
            ),
        }
    }

    fn open(
        &mut self,
        fields: &BTreeMap<String, String>,
        password: &str,
        statement_timeout: u32,
    ) -> Result<Response, Response> {
        if self.session.is_some() {
            let message = "The ODBC bridge is already open.".to_string();
            return Err(refusal(ErrorKind::Protocol, message));
        }
        let Some(connection) = fields.get("connection") else {
            let message = "An open names no connection.".to_string();
            return Err(refusal(ErrorKind::Protocol, message));
        };
        let user = fields.get("user").map_or("", String::as_str);
        let environment = match self.environment {
            Some(environment) => environment,
            None => *self.environment.insert(connect::environment()?),
        };
        let session = Session::open(environment, connection, user, password, statement_timeout)?;
        let opened = session.opened()?;
        self.session = Some(session);
        Ok(opened)
    }

    fn catalog(&self, read: impl FnOnce(&Session) -> Result<Response, Response>) -> Response {
        match &self.session {
            Some(session) => read(session).unwrap_or_else(|error| error),
            None => not_open(),
        }
    }

    fn reply(&mut self, id: u64, response: Response) {
        let mut payload = response.to_payload();
        // A catalog of a vast database can outgrow a frame; writing it would
        // fail and end the process, where this keeps the session.
        if payload.len() > MAX_PAYLOAD {
            payload = failure::plain("The answer is larger than one protocol frame.").to_payload();
        }
        run::write_or_exit(&mut self.out, Kind::Control, id, &payload);
    }
}

fn not_open() -> Response {
    refusal(
        ErrorKind::Protocol,
        "The ODBC bridge has no open connection.".to_string(),
    )
}

fn refusal(kind: ErrorKind, message: String) -> Response {
    Response::Error {
        kind,
        message,
        sqlstate: None,
        position: None,
        lost: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbdelve_plugin_protocol::write_frame;

    fn frame(kind: Kind, id: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, kind, id, payload).unwrap();
        bytes
    }

    fn request(id: u64, request: Request) -> Vec<u8> {
        frame(Kind::Control, id, &request.to_payload())
    }

    /// The frame loop and the worker over in-memory pipes; no driver manager
    /// is touched, since nothing here opens.
    fn exchange(input: Vec<u8>) -> Vec<(u64, Response)> {
        let control = Arc::new(Control::default());
        let (jobs, inbox) = mpsc::channel();
        let worker = Arc::clone(&control);
        let worker = thread::spawn(move || Bridge::new(Vec::new(), worker).work(inbox));
        serve(&mut input.as_slice(), &jobs, &control).unwrap();
        drop(jobs);
        let output = worker.join().unwrap();
        let mut output = output.as_slice();
        let mut responses = Vec::new();
        while let Some(frame) = read_frame(&mut output).unwrap() {
            assert_eq!(frame.kind, Kind::Control);
            responses.push((
                frame.request,
                Response::from_payload(&frame.payload).unwrap(),
            ));
        }
        responses
    }

    fn error_kind(response: &Response) -> Option<ErrorKind> {
        match response {
            Response::Error { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    #[test]
    fn hello_is_answered_and_eof_ends_the_loop() {
        let responses = exchange(request(1, Request::Hello { protocol: PROTOCOL }));
        assert_eq!(responses.len(), 1);
        let (
            1,
            Response::Hello {
                protocol, optional, ..
            },
        ) = &responses[0]
        else {
            panic!("a hello, got {responses:?}");
        };
        assert_eq!(*protocol, PROTOCOL);
        assert_eq!(optional, &[REFERENCES.to_string()]);
    }

    #[test]
    fn an_unknown_request_is_unsupported_and_data_or_a_catalog_unopened_is_a_protocol_error() {
        let input = [
            frame(Kind::Control, 2, br#"{"type":"databases"}"#),
            frame(Kind::Data, 3, b"rows"),
            request(4, Request::Catalog),
        ]
        .concat();
        let responses = exchange(input);
        let answered: Vec<_> = responses
            .iter()
            .map(|(id, response)| (*id, error_kind(response)))
            .collect();
        assert_eq!(
            answered,
            [
                (2, Some(ErrorKind::Unsupported)),
                (3, Some(ErrorKind::Protocol)),
                (4, Some(ErrorKind::Protocol)),
            ]
        );
    }

    #[test]
    fn prepare_answers_that_the_driver_verifies_the_name_it_dials() {
        let responses = exchange(request(
            1,
            Request::Prepare {
                fields: BTreeMap::new(),
            },
        ));
        assert_eq!(
            responses,
            [(
                1,
                Response::Prepared {
                    tls: Tls {
                        separate_name: false,
                        name_in_verify_ca: true,
                    },
                },
            )]
        );
    }

    #[test]
    fn close_is_the_last_answer() {
        let input = [
            request(1, Request::Close),
            request(2, Request::Hello { protocol: PROTOCOL }),
        ]
        .concat();
        assert_eq!(exchange(input), [(1, Response::Closed)]);
    }

    #[test]
    fn an_open_naming_no_connection_is_refused() {
        let open = Request::Open {
            fields: BTreeMap::new(),
            password: dbdelve_plugin_protocol::Password(String::new()),
            statement_timeout: 0,
        };
        let responses = exchange(request(1, open));
        assert_eq!(error_kind(&responses[0].1), Some(ErrorKind::Protocol));
    }

    #[test]
    fn a_frame_cut_short_is_a_violation() {
        let whole = request(1, Request::Hello { protocol: PROTOCOL });
        let (jobs, _inbox) = mpsc::channel();
        assert_eq!(
            serve(&mut &whole[..whole.len() - 1], &jobs, &Control::default()),
            Err(Violation::Truncated)
        );
    }
}

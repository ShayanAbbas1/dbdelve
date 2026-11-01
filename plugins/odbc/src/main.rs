//! The ODBC bridge: an engine plugin that reaches any database an ODBC driver
//! serves, through the platform's driver manager (unixODBC linked in on macOS,
//! the system's elsewhere).
//!
//! The main thread reads stdin frames for as long as the host writes them, so
//! `credit`, `stop` and `cancel` reach a running statement; one worker thread
//! answers every other request, in order, and is the only writer of stdout.
//! It exits on stdin EOF, as every plugin must.

mod connect;
mod failure;
mod raw;

use std::collections::BTreeMap;
use std::io::{self, BufWriter, Read, Write};
use std::process;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use dbdelve_plugin_protocol::{
    ErrorKind, Kind, PROTOCOL, REFERENCES, Request, Response, Tls, Violation, read_frame,
    write_frame,
};
use odbc_api::handles;

use crate::connect::Session;

fn main() {
    let (jobs, inbox) = mpsc::channel();
    thread::spawn(move || {
        Bridge::new(BufWriter::new(io::stdout().lock())).work(inbox);
        process::exit(0);
    });
    match serve(&mut io::stdin().lock(), &jobs) {
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
fn serve(input: &mut impl Read, jobs: &Sender<Job>) -> Result<(), Violation> {
    while let Some(frame) = read_frame(input)? {
        let id = frame.request;
        let job = match frame.kind {
            Kind::Data => Job::Refuse(
                id,
                ErrorKind::Protocol,
                "The ODBC bridge takes no data frames.".to_string(),
            ),
            Kind::Control => match Request::from_payload(&frame.payload) {
                // Each targets a running statement, and this bridge runs none
                // yet: a no-op, as `cancel` is with nothing running.
                Ok(Request::Credit { .. } | Request::Stop | Request::Cancel) => continue,
                Ok(request) => Job::Run(id, request),
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
    environment: Option<&'static handles::Environment>,
    session: Option<Session>,
}

impl<W: Write> Bridge<W> {
    /// Nothing touches the driver manager until the first `open`.
    fn new(out: W) -> Self {
        Self {
            out,
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
                    let response = self.run(request);
                    self.reply(id, response);
                }
                Job::Refuse(id, kind, message) => self.reply(id, refusal(kind, message)),
            }
        }
        self.out
    }

    fn run(&mut self, request: Request) -> Response {
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
                fields, password, ..
            } => self
                .open(&fields, &password.0)
                .unwrap_or_else(|error| error),
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
        let session = Session::open(environment, connection, user, password)?;
        let opened = session.opened()?;
        self.session = Some(session);
        Ok(opened)
    }

    fn reply(&mut self, id: u64, response: Response) {
        // The host has gone, or killed this process mid-write: nothing is left
        // to answer.
        if write_frame(&mut self.out, Kind::Control, id, &response.to_payload()).is_err() {
            process::exit(1);
        }
    }
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
        let (jobs, inbox) = mpsc::channel();
        let worker = thread::spawn(move || Bridge::new(Vec::new()).work(inbox));
        serve(&mut input.as_slice(), &jobs).unwrap();
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
    fn an_unknown_request_is_unsupported_and_data_is_a_protocol_error() {
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
                (4, Some(ErrorKind::Unsupported)),
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
            serve(&mut &whole[..whole.len() - 1], &jobs),
            Err(Violation::Truncated)
        );
    }
}

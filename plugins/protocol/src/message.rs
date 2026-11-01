use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Violation;

pub const PROTOCOL: u32 = 1;

/// What the host sends. The request id is in the frame header, never here, so
/// the host's reader routes a frame without parsing it; `credit`, `stop` and
/// `cancel` carry the id of the query they target and get no answer of their
/// own, the query's terminal frame being the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
    },
    /// The driver's abilities known before any socket opens, which is why it
    /// carries no password.
    Prepare {
        fields: BTreeMap<String, String>,
    },
    Open {
        fields: BTreeMap<String, String>,
        password: Password,
        /// Seconds; 0 is none.
        statement_timeout: u32,
    },
    /// `eligible`: the host judged the statement one that may be ended early at
    /// the row limit. The plugin still decides whether ending it now keeps the
    /// session and any open transaction.
    Query {
        sql: String,
        eligible: bool,
    },
    Credit {
        bytes: u64,
    },
    /// The row limit is reached: send no more rows, then end the statement
    /// early if that is provably safe, or fetch it to the end without sending.
    Stop,
    /// The user's Cancel, which may abort their transaction.
    Cancel,
    Catalog,
    Structure {
        schema: String,
        relation: String,
    },
    /// Optional: sent only when both `hello` and `opened` listed it.
    References {
        schema: String,
        relation: String,
    },
    Close,
}

/// Its own type so no `{:?}` of a request can put the password in a log.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Password(pub String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(..)")
    }
}

/// The optional message v1 defines. A later one is added beside it and
/// negotiated the same way, with no protocol bump, because a host never sends
/// what a plugin did not list.
pub const REFERENCES: &str = "references";

/// What a plugin answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u32,
        implementation: String,
        optional: Vec<String>,
    },
    Prepared {
        tls: Tls,
    },
    Opened {
        dbms_name: String,
        dbms_version: String,
        driver: String,
        /// Whether this connection can establish, when `stop` arrives, that
        /// ending a read early keeps the session and any open transaction.
        stops_early: bool,
        optional: Vec<String>,
    },
    /// A query's successful terminal frame.
    End {
        /// Every row the statement produced, including those not sent after a
        /// `stop` was drained.
        rows: u64,
        rows_affected: Option<u64>,
        stopped: bool,
        messages: Vec<String>,
        /// Result sets after the first, which protocol 1 does not carry.
        more_sets: u32,
    },
    Error {
        kind: ErrorKind,
        message: String,
        sqlstate: Option<String>,
        /// Byte offset into the statement.
        position: Option<u32>,
        /// The session is gone, as a dropped socket would leave it.
        lost: bool,
    },
    Catalog {
        schemas: Vec<Schema>,
    },
    Structure(Structure),
    References {
        references: Vec<Reference>,
    },
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tls {
    /// The driver can dial one address and verify the certificate against
    /// another name, which `verify-full` through an SSH tunnel needs.
    pub separate_name: bool,
    /// The driver checks the name even in `verify-ca`, so through a tunnel it
    /// would check the loopback name and `verify-ca` is refused there too.
    pub name_in_verify_ca: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Server,
    Cancelled,
    /// A request this plugin does not understand.
    Unsupported,
    /// The host broke the protocol.
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub relations: Vec<Relation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structure {
    pub columns: Vec<Column>,
    /// The key's column names, in key order; empty when there is none.
    pub primary_key: Vec<String>,
    pub indexes: Vec<Index>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub column: String,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_column: String,
}

/// A single-column foreign key of another relation that points at the one
/// asked about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub referenced_column: String,
}

impl Request {
    pub fn to_payload(&self) -> Vec<u8> {
        to_payload(self)
    }

    /// An unknown `type` is an error: the host never sends one a plugin cannot
    /// read, and a plugin answers it with `error { kind: unsupported }`.
    pub fn from_payload(payload: &[u8]) -> Result<Self, Violation> {
        from_payload(payload)
    }
}

impl Response {
    pub fn to_payload(&self) -> Vec<u8> {
        to_payload(self)
    }

    /// Unknown fields are ignored; an unknown `type` is a violation.
    pub fn from_payload(payload: &[u8]) -> Result<Self, Violation> {
        from_payload(payload)
    }
}

fn to_payload(message: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(message).expect("a message serializes: every key is a string")
}

fn from_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, Violation> {
    serde_json::from_slice(payload).map_err(|error| Violation::Json(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("connection".to_string(), "DSN=db2;".to_string()),
            ("user".to_string(), "db2inst1@corp".to_string()),
        ])
    }

    #[test]
    fn every_request_round_trips() {
        let requests = [
            Request::Hello { protocol: PROTOCOL },
            Request::Prepare { fields: fields() },
            Request::Open {
                fields: fields(),
                password: Password(String::new()),
                statement_timeout: 30,
            },
            Request::Query {
                sql: "SELECT 1 -- x".to_string(),
                eligible: true,
            },
            Request::Credit { bytes: 1 << 20 },
            Request::Stop,
            Request::Cancel,
            Request::Catalog,
            Request::Structure {
                schema: "s".to_string(),
                relation: "r".to_string(),
            },
            Request::References {
                schema: "s".to_string(),
                relation: "r".to_string(),
            },
            Request::Close,
        ];
        for request in requests {
            assert_eq!(
                Request::from_payload(&request.to_payload()),
                Ok(request.clone())
            );
        }
    }

    #[test]
    fn every_response_round_trips() {
        let responses = [
            Response::Hello {
                protocol: PROTOCOL,
                implementation: "echo 1".to_string(),
                optional: vec![REFERENCES.to_string()],
            },
            Response::Prepared {
                tls: Tls {
                    separate_name: false,
                    name_in_verify_ca: true,
                },
            },
            Response::Opened {
                dbms_name: "DB2/LINUXX8664".to_string(),
                dbms_version: "11.05.0900".to_string(),
                driver: "libdb2o.so".to_string(),
                stops_early: false,
                optional: Vec::new(),
            },
            Response::End {
                rows: 1_000_000,
                rows_affected: Some(3),
                stopped: false,
                messages: vec!["note".to_string()],
                more_sets: 2,
            },
            Response::Error {
                kind: ErrorKind::Server,
                message: "syntax error".to_string(),
                sqlstate: Some("42000".to_string()),
                position: Some(7),
                lost: false,
            },
            Response::Error {
                kind: ErrorKind::Cancelled,
                message: String::new(),
                sqlstate: None,
                position: None,
                lost: true,
            },
            Response::Catalog {
                schemas: vec![Schema {
                    name: "s".to_string(),
                    relations: vec![
                        Relation {
                            name: "t".to_string(),
                            kind: RelationKind::Table,
                        },
                        Relation {
                            name: "v".to_string(),
                            kind: RelationKind::MaterializedView,
                        },
                    ],
                }],
            },
            Response::Structure(Structure {
                columns: vec![Column {
                    name: "id".to_string(),
                    data_type: "INTEGER".to_string(),
                    nullable: false,
                    default: Some("0".to_string()),
                }],
                primary_key: vec!["id".to_string()],
                indexes: vec![Index {
                    name: "pk".to_string(),
                    columns: vec!["id".to_string()],
                    unique: true,
                    primary: true,
                }],
                foreign_keys: vec![ForeignKey {
                    column: "owner".to_string(),
                    referenced_schema: "s".to_string(),
                    referenced_table: "users".to_string(),
                    referenced_column: "id".to_string(),
                }],
            }),
            Response::References {
                references: vec![Reference {
                    schema: "s".to_string(),
                    table: "orders".to_string(),
                    column: "item".to_string(),
                    referenced_column: "id".to_string(),
                }],
            },
            Response::Closed,
        ];
        for response in responses {
            assert_eq!(
                Response::from_payload(&response.to_payload()),
                Ok(response.clone())
            );
        }
    }

    #[test]
    fn the_wire_spelling_is_snake_case_and_tagged() {
        let payload = Request::Query {
            sql: "x".to_string(),
            eligible: false,
        }
        .to_payload();
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            r#"{"type":"query","sql":"x","eligible":false}"#
        );
        let structure = String::from_utf8(Response::Structure(Structure::default()).to_payload());
        assert!(
            structure
                .unwrap()
                .starts_with(r#"{"type":"structure","columns":[]"#)
        );
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        assert_eq!(
            Request::from_payload(br#"{"type":"stop","from_the_future":1}"#),
            Ok(Request::Stop)
        );
        assert_eq!(
            Response::from_payload(br#"{"type":"closed","why":"asked"}"#),
            Ok(Response::Closed)
        );
    }

    #[test]
    fn an_unknown_type_is_an_error() {
        assert!(matches!(
            Request::from_payload(br#"{"type":"databases"}"#),
            Err(Violation::Json(_))
        ));
        assert!(matches!(
            Response::from_payload(br#"{"type":"stopped"}"#),
            Err(Violation::Json(_))
        ));
        assert!(matches!(
            Response::from_payload(b"not json"),
            Err(Violation::Json(_))
        ));
    }

    #[test]
    fn the_password_stays_out_of_debug_output() {
        let open = Request::Open {
            fields: fields(),
            password: Password("hunter2".to_string()),
            statement_timeout: 0,
        };
        assert!(!format!("{open:?}").contains("hunter2"));
    }
}

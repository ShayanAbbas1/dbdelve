//! The frames a call is made of: newline-delimited JSON, one object per line,
//! at most [`FRAME_LIMIT`] bytes, or [`REPLY_LIMIT`] for the answer. A
//! connection opens with one [`Hello`] and is answered with one `Reply` line.

use std::io::{self, BufRead, Read};

use serde::{Deserialize, Serialize};

use super::tools::Tool;

/// The version the first frame must carry. A frame from another one is
/// refused rather than read as if it meant what this build's frames mean.
pub(super) const PROTOCOL_VERSION: u32 = 3;

// ponytail: a first frame is a tool name, a profile name and now an agent's
// statement, so 64 KiB is beyond any statement worth reading back as rows; the
// cap is what stops a connection from making the app buffer without end.
pub(super) const FRAME_LIMIT: u64 = 64 * 1024;

// ponytail: an answer is the app's text under `RESULT_BUDGET`, and JSON writes
// each `"` and `\` as two bytes, so the worst case is twice the text plus the
// frame's own keys; the assert below is what keeps the two numbers honest. A
// request keeps `FRAME_LIMIT`: only the answer carries rows.
pub(super) const REPLY_LIMIT: u64 = 256 * 1024;

const _: () = assert!(2 * super::result::RESULT_BUDGET as u64 + 4096 <= REPLY_LIMIT);

/// The first frame of a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Hello {
    Call {
        v: u32,
        tool: Tool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sql: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        analyze: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        routine: Option<String>,
    },
    /// An agent is attached. Asks nothing, and ends when the connection does.
    Presence { v: u32 },
}

/// One frame as a line ending in `\n`.
pub(super) fn encode<T: Serialize>(frame: &T) -> String {
    let mut line = serde_json::to_string(frame)
        .expect("a frame of strings, numbers and unit enums always serializes");
    line.push('\n');
    line
}

/// The next frame, without its newline. `None` is a reader with nothing left;
/// a line longer than `limit`, one cut short by the end of the stream, or one
/// that is not UTF-8 is an error.
pub(super) fn read_frame(reader: &mut impl BufRead, limit: u64) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(limit).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        let reason = if read as u64 >= limit {
            "a frame ran past its size limit without ending"
        } else {
            "a frame ended before its newline"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a frame was not UTF-8"))
}

/// Read a first frame, refusing an unknown field, kind or tool and any
/// version but [`PROTOCOL_VERSION`].
pub(super) fn decode_hello(line: &str) -> Result<Hello, String> {
    let hello: Hello = serde_json::from_str(line)
        .map_err(|error| format!("The first frame was refused: {error}."))?;
    let (Hello::Call { v, .. } | Hello::Presence { v }) = &hello;
    if *v != PROTOCOL_VERSION {
        return Err(format!(
            "The first frame speaks version {v}; this build speaks version {PROTOCOL_VERSION}."
        ));
    }
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::agent::{Reply, Status, tests::frame};

    #[test]
    fn a_call_frame_and_an_answer_read_back_as_written() {
        let hello = Hello::Call {
            v: PROTOCOL_VERSION,
            tool: Tool::ExecuteSql,
            connection: Some("Orders".to_string()),
            sql: Some("SELECT 1".to_string()),
            schema: None,
            table: None,
            analyze: None,
            query: None,
            routine: None,
        };
        let line = encode(&hello);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let mut reader = Cursor::new(line.into_bytes());
        let frame = read_frame(&mut reader, FRAME_LIMIT)
            .expect("the frame reads")
            .expect("there is a frame");
        assert_eq!(decode_hello(&frame), Ok(hello));

        let reply = Reply {
            status: Status::Executed,
            text: "[]".to_string(),
        };
        let line = encode(&reply);
        assert_eq!(
            serde_json::from_str::<Reply>(line.trim_end()).expect("the answer decodes"),
            reply
        );

        let codes: Vec<String> = [
            Status::Executed,
            Status::Rejected,
            Status::RefusedByGate,
            Status::Cancelled,
            Status::Expired,
            Status::AppNotRunning,
            Status::Failed,
        ]
        .map(|status| serde_json::to_string(&status).expect("a status serializes"))
        .into();
        assert_eq!(
            codes,
            [
                "\"executed\"",
                "\"rejected\"",
                "\"refused_by_gate\"",
                "\"cancelled\"",
                "\"expired\"",
                "\"app_not_running\"",
                "\"failed\""
            ]
        );
    }

    #[test]
    fn a_frame_with_an_unknown_field_or_another_version_is_refused() {
        let call = |fields: &str| frame(&format!(r#""kind":"call",{fields}"#));
        for text in [
            call(r#""tool":"list_connections","extra":1"#),
            r#"{"v":1,"kind":"call","tool":"list_connections"}"#.to_string(),
            r#"{"v":2,"kind":"call","tool":"list_connections"}"#.to_string(),
            r#"{"v":4,"kind":"call","tool":"list_connections"}"#.to_string(),
            frame(r#""kind":"shout","tool":"list_connections""#),
            call(r#""tool":"drop_everything""#),
            String::new(),
            "not json".to_string(),
        ] {
            assert!(decode_hello(&text).is_err(), "{text:?} was accepted");
        }
        assert!(decode_hello(&call(r#""tool":"list_connections""#)).is_ok());
        assert!(decode_hello(&frame(r#""kind":"presence""#)).is_ok());
        assert!(decode_hello(r#"{"v":2,"kind":"presence"}"#).is_err());
        assert!(decode_hello(r#"{"v":4,"kind":"presence"}"#).is_err());
        assert_eq!(
            decode_hello(&call(
                r#""tool":"execute_sql","connection":"Orders","sql":"SELECT 1""#
            )),
            Ok(Hello::Call {
                v: PROTOCOL_VERSION,
                tool: Tool::ExecuteSql,
                connection: Some("Orders".to_string()),
                sql: Some("SELECT 1".to_string()),
                schema: None,
                table: None,
                analyze: None,
                query: None,
                routine: None,
            })
        );

        let mut long = Cursor::new(vec![b'a'; FRAME_LIMIT as usize + 10]);
        let error = read_frame(&mut long, FRAME_LIMIT).expect_err("a frame with no end is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let mut empty = Cursor::new(Vec::new());
        assert!(matches!(read_frame(&mut empty, FRAME_LIMIT), Ok(None)));
    }

    #[test]
    fn the_largest_reply_fits_the_reply_limit() {
        let text: String = (0..crate::agent::result::RESULT_BUDGET)
            .map(|at| if at % 2 == 0 { '"' } else { '\\' })
            .collect();
        let line = encode(&Reply {
            status: Status::Executed,
            text,
        });
        assert!(line.len() as u64 <= REPLY_LIMIT, "{}", line.len());
    }
}

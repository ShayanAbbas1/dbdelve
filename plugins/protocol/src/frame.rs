use std::io::{self, Read, Write};

use crate::Violation;
use crate::limits::MAX_FRAME;

/// The bytes after the length field that every frame carries before its payload:
/// the kind and the request id.
const HEADER: u32 = 9;

/// The largest payload one frame can carry.
pub const MAX_PAYLOAD: usize = (MAX_FRAME - HEADER) as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A UTF-8 JSON object: a [`crate::Request`] or a [`crate::Response`].
    Control = 0,
    /// One Arrow IPC encapsulated message (see [`crate::arrow`]).
    Data = 1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub request: u64,
    pub payload: Vec<u8>,
}

/// One frame, or `Ok(None)` when the input ends exactly at a frame boundary.
///
/// The length is checked before anything is allocated for it, so a length
/// prefix that claims gigabytes costs nothing.
pub fn read_frame(input: &mut impl Read) -> Result<Option<Frame>, Violation> {
    let mut len = [0; 4];
    let mut filled = 0;
    while filled < len.len() {
        match input.read(&mut len[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(Violation::Truncated),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Violation::Io(error.to_string())),
        }
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(Violation::Oversize(len));
    }
    if len < HEADER {
        return Err(Violation::Truncated);
    }

    let mut header = [0; HEADER as usize];
    read_exact(input, &mut header)?;
    let kind = match header[0] {
        0 => Kind::Control,
        1 => Kind::Data,
        other => return Err(Violation::UnknownKind(other)),
    };
    let request = u64::from_le_bytes(header[1..].try_into().expect("eight bytes"));
    let mut payload = vec![0; (len - HEADER) as usize];
    read_exact(input, &mut payload)?;
    Ok(Some(Frame {
        kind,
        request,
        payload,
    }))
}

fn read_exact(input: &mut impl Read, buffer: &mut [u8]) -> Result<(), Violation> {
    input
        .read_exact(buffer)
        .map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => Violation::Truncated,
            _ => Violation::Io(error.to_string()),
        })
}

/// Refuses a payload over [`MAX_PAYLOAD`] rather than writing a frame the other
/// side has to treat as a violation.
pub fn write_frame(
    output: &mut impl Write,
    kind: Kind,
    request: u64,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a frame payload of {} bytes is over the {MAX_PAYLOAD} protocol 1 allows",
                payload.len()
            ),
        ));
    }
    let len = HEADER + payload.len() as u32;
    let mut header = [0; 4 + HEADER as usize];
    header[..4].copy_from_slice(&len.to_le_bytes());
    header[4] = kind as u8;
    header[5..].copy_from_slice(&request.to_le_bytes());
    output.write_all(&header)?;
    output.write_all(payload)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(kind: Kind, request: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, kind, request, payload).unwrap();
        bytes
    }

    #[test]
    fn a_frame_round_trips() {
        let bytes = [
            written(Kind::Control, 7, b"{\"type\":\"stop\"}"),
            written(Kind::Data, u64::MAX, b""),
        ]
        .concat();
        let mut input = bytes.as_slice();

        let first = read_frame(&mut input).unwrap().unwrap();
        assert_eq!(first.kind, Kind::Control);
        assert_eq!(first.request, 7);
        assert_eq!(first.payload, b"{\"type\":\"stop\"}");
        let second = read_frame(&mut input).unwrap().unwrap();
        assert_eq!(second.kind, Kind::Data);
        assert_eq!(second.request, u64::MAX);
        assert!(second.payload.is_empty());
        assert_eq!(read_frame(&mut input), Ok(None));
    }

    /// The reader would block forever on a pipe if it tried to read a frame this
    /// long, so failing at once also proves nothing was allocated or awaited.
    #[test]
    fn a_length_over_the_cap_is_oversize_before_anything_is_read() {
        let mut bytes = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bytes.push(0);
        assert_eq!(
            read_frame(&mut bytes.as_slice()),
            Err(Violation::Oversize(MAX_FRAME + 1))
        );
    }

    #[test]
    fn a_frame_cut_short_is_truncated() {
        let whole = written(Kind::Control, 1, b"{}");
        for cut in [2, 4, 8, whole.len() - 1] {
            assert_eq!(
                read_frame(&mut &whole[..cut]),
                Err(Violation::Truncated),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_length_shorter_than_the_header_is_truncated() {
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.extend([0; 8]);
        assert_eq!(read_frame(&mut bytes.as_slice()), Err(Violation::Truncated));
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let mut bytes = written(Kind::Control, 1, b"{}");
        bytes[4] = 7;
        assert_eq!(
            read_frame(&mut bytes.as_slice()),
            Err(Violation::UnknownKind(7))
        );
    }

    #[test]
    fn a_payload_over_the_cap_is_not_written() {
        let mut bytes = Vec::new();
        let payload = vec![0; MAX_PAYLOAD + 1];
        assert!(write_frame(&mut bytes, Kind::Data, 1, &payload).is_err());
        assert!(bytes.is_empty());
    }
}

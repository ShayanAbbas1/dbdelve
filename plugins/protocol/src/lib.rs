//! Protocol 1 between DBDelve and an engine plugin: length-prefixed frames over
//! the plugin's stdin and stdout, JSON control messages, and Arrow IPC result
//! data under byte credit. Both sides link this crate, so neither can drift from
//! the other's reading of a frame.

pub mod arrow;
mod frame;
pub mod limits;
mod message;

use std::fmt;

pub use frame::{Frame, Kind, MAX_PAYLOAD, read_frame, write_frame};
pub use message::*;

/// Something the other side sent that protocol 1 does not allow. The host
/// answers every one by killing the plugin and reporting the connection reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// A length prefix over [`limits::MAX_FRAME`].
    Oversize(u32),
    /// The stream ended inside a frame, or a length was shorter than the
    /// header every frame carries.
    Truncated,
    UnknownKind(u8),
    Json(String),
    Arrow(String),
    /// Reading the pipe failed: not the other side's doing, but the stream is
    /// as unusable as after a violation.
    Io(String),
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize(len) => write!(
                f,
                "a frame of {len} bytes is over the {} protocol 1 allows",
                limits::MAX_FRAME
            ),
            Self::Truncated => f.write_str("a frame was cut short"),
            Self::UnknownKind(kind) => write!(f, "a frame of unknown kind {kind}"),
            Self::Json(error) => write!(f, "a control message that does not read: {error}"),
            Self::Arrow(error) => write!(f, "result data that does not read: {error}"),
            Self::Io(error) => write!(f, "the pipe could not be read: {error}"),
        }
    }
}

impl std::error::Error for Violation {}

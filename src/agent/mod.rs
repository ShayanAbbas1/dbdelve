//! The bridge between `dbdelve mcp` and the window.
//!
//! One socket connection is one call: closing it is the cancel, so there is no
//! cancel frame. The app side is std threads and a channel, and the window
//! reaches them only through [`Inbound`] arriving on the pump in
//! `workspace::agent`. [`Status`], [`Reply`] and [`Inbound`] are the
//! vocabulary both halves speak, and they build on every OS; the socket itself
//! is Unix only.
//!
//! `wire`, `tools`, `ipc` and the shim never name gpui, `crate::db` or the
//! keychain: they are the part that can be read, and run, without a window and
//! without a database.
#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "the bridge only listens on Unix, so on Windows the workspace names these types and nothing builds them"
    )
)]

pub(crate) mod policy;
pub(crate) mod result;
pub(crate) mod shim;
pub(crate) mod tools;

#[cfg(unix)]
mod ipc;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod wire;

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub(crate) use ipc::socket_path;
#[cfg(all(unix, test))]
pub(crate) use server::listen;
#[cfg(unix)]
pub(crate) use server::{Listener, listen_if};
use tools::Tool;

/// Whether another window of this variant is serving agents at this build's
/// socket. Asked of a window that is not listening itself, so a live answer
/// can only be someone else. The probe is an unframed connection, which the
/// serving window closes without counting (see [`Listener`]).
#[cfg(unix)]
pub(crate) fn socket_is_live() -> bool {
    socket_path().is_ok_and(|path| ipc::is_live(&path))
}

/// What the agent is told: `executed` is the app's, for a call it carried out;
/// `refused_by_gate` is the app's, for a name that resolves to nothing visible,
/// a statement the gate refused, or a bridge that is full; `refused_by_policy`
/// is the app's, for a tool or statement class set to Deny in AI Features;
/// `expired` is the app's, for an approval nobody answered;
/// `cancelled` is the app's, logged only, because the caller has already gone;
/// `rejected` is the human's no, from the dialog or the kill switch;
/// `app_not_running` is the shim's, for a connection that failed or closed
/// without an answer; `failed` is the app's, for a statement that failed, ran
/// out of time, or a connection that would not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Executed,
    Rejected,
    RefusedByGate,
    RefusedByPolicy,
    Cancelled,
    Expired,
    AppNotRunning,
    Failed,
}

impl Status {
    /// The code as the agent reads it, at the head of every answer: the same
    /// string serde writes for it on the socket.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Status::Executed => "executed",
            Status::Rejected => "rejected",
            Status::RefusedByGate => "refused_by_gate",
            Status::RefusedByPolicy => "refused_by_policy",
            Status::Cancelled => "cancelled",
            Status::Expired => "expired",
            Status::AppNotRunning => "app_not_running",
            Status::Failed => "failed",
        }
    }
}

/// The one answer frame a call gets: a status and one sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub(crate) status: Status,
    pub(crate) text: String,
}

// ponytail: the one fixed wait for a person to answer an approval. It is not a
// setting: a call a person has walked away from has to end by itself, and
// nothing here answers it for them.
pub(crate) const EXPIRY: Duration = Duration::from_secs(300);

/// A call's identity: the connection it arrived on, which is also how it is
/// found again when that connection closes. Never a position in a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallId(pub(crate) u64);

/// One accepted connection, from the moment it is classified until it ends.
/// What a presence is held by; a call has its own identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnId(pub(crate) u64);

/// The arguments a call carries beyond the connection it names, across the
/// socket boundary. One field per argument a tool reads, so none is ever
/// carried unread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CallArgs {
    /// The statement `execute_sql` was asked to run, exactly as written.
    pub(crate) sql: Option<String>,
    /// The schema `list_tables` or `describe_table` was asked about, exactly as
    /// written.
    pub(crate) schema: Option<String>,
    /// The table `describe_table` was asked about, exactly as written.
    pub(crate) table: Option<String>,
    /// Whether `explain_query` was asked to run the statement to measure it.
    /// `None` is not asked, which is `false`.
    pub(crate) analyze: Option<bool>,
    /// What `search_schema` was asked to find, exactly as written.
    pub(crate) query: Option<String>,
    /// The routine `describe_routine` was asked about, exactly as written.
    pub(crate) routine: Option<String>,
}

/// What a connection thread tells the window.
pub(crate) enum Inbound {
    /// A call, with the one-slot channel its answer goes back on. Dropping the
    /// sender unsent closes the connection without writing, which is how a
    /// cancelled call ends.
    Call {
        id: CallId,
        tool: Tool,
        /// The profile name the agent gave, if it gave one.
        connection: Option<String>,
        args: CallArgs,
        reply: std::sync::mpsc::SyncSender<Reply>,
        /// Set by the hangup watcher before it sends `Hangup`, so the window
        /// can know the caller left before it hears so.
        gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    /// The caller closed its connection, so nobody is waiting for the call
    /// any more. Sent for every call, answered or not, so one that arrives
    /// after the call finished is expected.
    Hangup { id: CallId },
    /// The answer to this call could not be delivered, because its caller had
    /// already gone: the human answered in the moment between the caller
    /// leaving and its `Hangup` reaching the window. Sent after the call has
    /// finished, so the window only ever marks what it logged.
    Undelivered { id: CallId },
    /// A shim opened its presence connection: an agent is attached, though it
    /// may have asked nothing yet.
    PresenceOpened { conn: ConnId },
    /// A presence connection ended, however it did.
    PresenceClosed { conn: ConnId },
}

/// Why the socket was not bound.
pub(crate) enum Declined {
    /// A live socket is already at the path: another window is serving agents,
    /// and its socket is left as it is.
    ServedElsewhere,
    /// Something went wrong, in words that say what.
    Failed(String),
    /// The lock was taken, and the saved choice read under it no longer said
    /// to serve: the lock is let go and nothing is bound.
    Withdrawn,
}

/// Said wherever the bridge is asked for on an OS it does not run on.
#[cfg(not(unix))]
pub(crate) const UNAVAILABLE: &str = "The agent integration is not available on Windows.";

/// The socket is Unix only, so there is nothing to keep alive here.
#[cfg(not(unix))]
pub(crate) enum Listener {}

/// There is no socket path.
#[cfg(not(unix))]
pub(crate) fn socket_path() -> Result<std::path::PathBuf, String> {
    Err(UNAVAILABLE.to_string())
}

/// There is no socket for another window to be serving.
#[cfg(not(unix))]
pub(crate) fn socket_is_live() -> bool {
    false
}

/// There is no socket to listen on.
#[cfg(not(unix))]
pub(crate) fn listen(
    _path: &std::path::Path,
    _inbound: tokio::sync::mpsc::UnboundedSender<Inbound>,
) -> Result<Listener, Declined> {
    Err(Declined::Failed(UNAVAILABLE.to_string()))
}

/// There is no socket to listen on, whatever is permitted.
#[cfg(not(unix))]
pub(crate) fn listen_if(
    path: &std::path::Path,
    inbound: tokio::sync::mpsc::UnboundedSender<Inbound>,
    _permitted: impl FnOnce() -> bool,
) -> Result<Listener, Declined> {
    listen(path, inbound)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::{Status, wire::PROTOCOL_VERSION};

    /// A first frame at this build's version: `fields` is the rest of the
    /// object, so no test spells the version it speaks.
    pub(crate) fn frame(fields: &str) -> String {
        format!(r#"{{"v":{PROTOCOL_VERSION},{fields}}}"#)
    }

    #[test]
    fn a_status_reads_as_the_string_serde_writes() {
        for status in [
            Status::Executed,
            Status::Rejected,
            Status::RefusedByGate,
            Status::RefusedByPolicy,
            Status::Cancelled,
            Status::Expired,
            Status::AppNotRunning,
            Status::Failed,
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("a status serializes"),
                status.as_str()
            );
        }
    }

    /// A socket path short enough to bind on every platform. Not under
    /// `temp_dir()`: macOS's is already about 49 bytes, and `sun_path` holds
    /// 104 there.
    pub(crate) fn short_socket_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        PathBuf::from(format!(
            "/tmp/dbd-{}-{}/s",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
